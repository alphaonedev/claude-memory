// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]

mod autonomy;
mod color;
mod config;
mod curator;
mod db;
mod embeddings;
mod errors;
mod federation;
mod handlers;
mod hnsw;
mod identity;
mod llm;
mod mcp;
mod metrics;
#[cfg(feature = "sal")]
mod migrate;
mod mine;
mod models;
mod replication;
mod reranker;
#[cfg(feature = "sal")]
mod store;
mod subscriptions;
mod toon;
mod validate;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
};
use chrono::{Duration, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::models::Tier;

const DEFAULT_DB: &str = "ai-memory.db";
const DEFAULT_PORT: u16 = 9077;
const GC_INTERVAL_SECS: u64 = 1800;
/// WAL auto-checkpoint cadence in the HTTP daemon. Bounds `*-wal`
/// file growth between `SQLite`'s internal page-count checkpoints.
const WAL_CHECKPOINT_INTERVAL_SECS: u64 = 600;

fn id_short(id: &str) -> &str {
    let end = id.len().min(8);
    // Find a valid UTF-8 boundary
    let mut end = end;
    while end > 0 && !id.is_char_boundary(end) {
        end -= 1;
    }
    &id[..end]
}

#[derive(Parser)]
#[command(
    name = "ai-memory",
    version,
    about = "AI-agnostic persistent memory — MCP server, HTTP API, and CLI for any AI platform"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[arg(long, env = "AI_MEMORY_DB", default_value = DEFAULT_DB, global = true)]
    db: PathBuf,
    /// Output as JSON (machine-parseable)
    #[arg(long, global = true, default_value_t = false)]
    json: bool,
    /// Agent identifier used for store operations. If unset, an NHI-hardened
    /// default is synthesized (see `ai-memory store --help`). Accepts the
    /// `AI_MEMORY_AGENT_ID` environment variable as a fallback.
    #[arg(long, env = "AI_MEMORY_AGENT_ID", global = true)]
    agent_id: Option<String>,
    /// v0.6.0.0: path to a file containing the `SQLCipher` passphrase.
    /// Only meaningful when the binary was built with
    /// `--features sqlcipher` (standard builds ignore this flag). File
    /// must be root-readable (mode 0400 recommended). The passphrase is
    /// read once at startup and exported as `AI_MEMORY_DB_PASSPHRASE`
    /// for the duration of the process — passing the passphrase
    /// directly as an env var or as a flag value leaks to the process
    /// list (`ps -E`) and shell history.
    #[arg(long, global = true, value_name = "PATH")]
    db_passphrase_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP memory daemon
    Serve(ServeArgs),
    /// Run as an MCP (Model Context Protocol) tool server over stdio
    Mcp {
        /// Feature tier: keyword (FTS only) or semantic (embeddings + FTS)
        #[arg(long, default_value = "semantic")]
        tier: String,
    },
    /// Store a new memory
    Store(StoreArgs),
    /// Update an existing memory by ID
    Update(UpdateArgs),
    /// Recall memories relevant to a context
    Recall(RecallArgs),
    /// Search memories by text
    Search(SearchArgs),
    /// Retrieve a memory by ID
    Get(GetArgs),
    /// List memories
    List(ListArgs),
    /// Delete a memory by ID
    Delete(DeleteArgs),
    /// Promote a memory to long-term
    Promote(PromoteArgs),
    /// Delete memories matching a pattern
    Forget(ForgetArgs),
    /// Link two memories
    Link(LinkArgs),
    /// Consolidate multiple memories into one
    Consolidate(ConsolidateArgs),
    /// Run garbage collection
    Gc,
    /// Show statistics
    Stats,
    /// List all namespaces
    Namespaces,
    /// Export all memories as JSON
    Export,
    /// Import memories from JSON (stdin)
    Import(ImportArgs),
    /// Resolve a contradiction — mark one memory as superseding another
    Resolve(ResolveArgs),
    /// Interactive memory shell (REPL)
    Shell,
    /// Sync memories between two database files
    Sync(SyncArgs),
    /// Run the peer-to-peer sync daemon — continuously exchange memories
    /// with one or more HTTP peers (Phase 3 Task 3b.1). The defining
    /// grand-slam capability: two agents on two machines form a live
    /// knowledge mesh with no cloud, no login, no `SaaS`.
    SyncDaemon(SyncDaemonArgs),
    /// Auto-consolidate short-term memories by namespace
    AutoConsolidate(AutoConsolidateArgs),
    /// Generate shell completions
    Completions(CompletionsArgs),
    /// Generate man page
    Man,
    /// Import memories from historical conversations (Claude, `ChatGPT`, Slack exports)
    Mine(MineArgs),
    /// Manage the memory archive (list, restore, purge, stats)
    Archive(ArchiveArgs),
    /// Register or list agents (Task 1.3)
    Agents(AgentsArgs),
    /// List / approve / reject governance-pending actions (Task 1.9)
    Pending(PendingArgs),
    /// v0.6.0.0: snapshot the `SQLite` database to a timestamped backup
    /// file. Uses `SQLite` `VACUUM INTO` which is hot-backup safe (no daemon
    /// stop required). Writes a `manifest.json` alongside (sha256 + version).
    Backup(BackupArgs),
    /// v0.6.0.0: restore the `SQLite` database from a backup file written
    /// by `ai-memory backup`. Verifies the manifest sha256 before
    /// replacing the current DB. The current DB is moved aside as a safety
    /// net before the replacement.
    Restore(RestoreArgs),
    /// v0.6.1: run the autonomous curator. `--once` runs a single sweep
    /// and prints a JSON report; `--daemon` loops with `--interval-secs`
    /// between cycles. Auto-tags memories without tags and flags
    /// contradictions against nearby siblings in the same namespace.
    Curator(CuratorArgs),
    /// v0.7: migrate memories between SAL backends. Gated behind
    /// `--features sal`. Reads pages via `MemoryStore::list`, writes
    /// via `MemoryStore::store`. Idempotent: source ids are preserved
    /// and both adapters upsert on id.
    #[cfg(feature = "sal")]
    Migrate(MigrateArgs),
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
struct CuratorArgs {
    /// Run exactly one sweep and exit. Mutually exclusive with --daemon.
    #[arg(long, conflicts_with = "daemon")]
    once: bool,
    /// Loop forever, sleeping --interval-secs between sweeps. SIGINT /
    /// SIGTERM trigger a clean shutdown between cycles.
    #[arg(long)]
    daemon: bool,
    /// Seconds between daemon sweeps. Clamped to [60, 86400].
    #[arg(long, default_value_t = 3600)]
    interval_secs: u64,
    /// Hard cap on LLM-invoking operations per cycle.
    #[arg(long, default_value_t = 100)]
    max_ops: usize,
    /// Emit the report without persisting any metadata changes.
    #[arg(long)]
    dry_run: bool,
    /// Only curate memories in these namespaces. Repeat flag for multiple.
    #[arg(long = "include-namespace")]
    include_namespaces: Vec<String>,
    /// Exclude these namespaces from curation. Repeat flag for multiple.
    #[arg(long = "exclude-namespace")]
    exclude_namespaces: Vec<String>,
    /// Print the report as JSON rather than a human-readable summary.
    #[arg(long)]
    json: bool,
    /// Reverse rollback-log entries instead of running a sweep. Accepts
    /// a specific rollback-memory id, or `--last N` for the most recent.
    /// Mutually exclusive with `--once` and `--daemon`.
    #[arg(long, conflicts_with_all = ["once", "daemon"])]
    rollback: Option<String>,
    /// With `--rollback`, reverse the N most recent rollback-log entries
    /// instead of a single id.
    #[arg(long)]
    rollback_last: Option<usize>,
}

#[cfg(feature = "sal")]
#[derive(Args)]
struct MigrateArgs {
    /// Source URL. `sqlite:///path/to/file.db` or
    /// `postgres://user:pass@host:port/dbname`.
    #[arg(long)]
    from: String,
    /// Destination URL. Same URL shape as `--from`.
    #[arg(long)]
    to: String,
    /// Page size. Clamped to [1, 10000]. Default 1000.
    #[arg(long, default_value_t = 1000)]
    batch: usize,
    /// Only migrate memories in this namespace.
    #[arg(long)]
    namespace: Option<String>,
    /// Emit the report but do NOT write to the destination.
    #[arg(long)]
    dry_run: bool,
    /// Emit the report as JSON rather than human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct BackupArgs {
    /// Directory where the snapshot and manifest are written. Created if
    /// missing.
    #[arg(long, default_value = "./backups")]
    to: PathBuf,
    /// Retention: after writing a new snapshot, delete the oldest
    /// snapshots so that at most this many remain. 0 disables rotation.
    #[arg(long, default_value_t = 48)]
    keep: usize,
}

#[derive(Args)]
struct RestoreArgs {
    /// Path to a snapshot file OR a backup directory. When a directory is
    /// supplied, the most recent snapshot is used.
    #[arg(long)]
    from: PathBuf,
    /// Skip sha256 verification against the manifest. Not recommended.
    #[arg(long)]
    skip_verify: bool,
}

#[derive(Args)]
struct PendingArgs {
    #[command(subcommand)]
    action: PendingAction,
}

#[derive(Subcommand)]
enum PendingAction {
    /// List pending actions (optionally filter by status).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Approve a pending action by id.
    Approve { id: String },
    /// Reject a pending action by id.
    Reject { id: String },
}

#[derive(Args)]
struct AgentsArgs {
    #[command(subcommand)]
    action: Option<AgentsAction>,
}

#[derive(Subcommand)]
enum AgentsAction {
    /// List registered agents (default)
    List,
    /// Register or refresh an agent
    Register {
        /// Agent identifier
        #[arg(long)]
        agent_id: String,
        /// Agent type. Curated values: human, system, ai:claude-opus-4.6,
        /// ai:claude-opus-4.7, ai:codex-5.4, ai:grok-4.2. Any `ai:<name>`
        /// form is also accepted (e.g. `ai:gpt-5`, `ai:gemini-2.5`) —
        /// red-team #235.
        #[arg(long)]
        agent_type: String,
        /// Comma-separated capability tags
        #[arg(long, default_value = "")]
        capabilities: String,
    },
}

#[derive(Args)]
struct ArchiveArgs {
    #[command(subcommand)]
    action: ArchiveAction,
}

#[derive(Subcommand)]
enum ArchiveAction {
    /// List archived memories
    List {
        #[arg(long, short)]
        namespace: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Restore an archived memory back to active
    Restore { id: String },
    /// Permanently delete old archive entries
    Purge {
        /// Delete archive entries older than N days (all if omitted)
        #[arg(long)]
        older_than_days: Option<i64>,
    },
    /// Show archive statistics
    Stats,
}

#[derive(Args)]
struct MineArgs {
    /// Path to the export file or directory
    path: PathBuf,
    /// Export format: claude, chatgpt, slack
    #[arg(long, short)]
    format: String,
    /// Namespace for imported memories (auto-detected if omitted)
    #[arg(long, short)]
    namespace: Option<String>,
    /// Memory tier for imported memories
    #[arg(long, short, default_value = "mid")]
    tier: String,
    /// Minimum message count to import a conversation
    #[arg(long, default_value_t = 3)]
    min_messages: usize,
    /// Dry run — show what would be imported without writing
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Path to PEM-encoded TLS certificate (may include the full chain).
    /// Passing both `--tls-cert` and `--tls-key` switches `serve` to
    /// HTTPS. rustls under the hood — no OpenSSL dep. Absent both
    /// flags = plain HTTP (same as every previous release).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// Path to PEM-encoded TLS private key (PKCS#8 or RSA).
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Path to a file containing SHA-256 fingerprints of trusted client
    /// certificates, one per line (case-insensitive hex, optionally with
    /// `:` separators; comments start with `#`). When set, `serve`
    /// demands client-cert mTLS on every connection and refuses any peer
    /// whose cert fingerprint is not on the list. Requires `--tls-cert`
    /// and `--tls-key`. This is the peer-mesh identity gate — a peer
    /// without an authorised cert can't even open a TCP connection, let
    /// alone hit `/sync/push`. Layer 2 of the peer-mesh crypto stack;
    /// attested `agent_id` extraction (Layer 2b) lands post-v0.6.0.
    #[arg(long, requires = "tls_cert")]
    mtls_allowlist: Option<PathBuf>,
    /// Seconds to wait for in-flight requests to complete on graceful
    /// shutdown (SIGINT). Default 30. Bumped from 10 in v0.6.0 because
    /// large `/sync/push` batches can take longer than 10s under load
    /// (red-team #233).
    #[arg(long, default_value_t = 30)]
    shutdown_grace_secs: u64,

    // -------- v0.7 federation (ADR-0001) ---------------------------
    /// W-of-N write quorum. When >=1 and `--quorum-peers` is non-empty,
    /// every HTTP write fans out to every peer and returns OK only
    /// after the local commit + W-1 peer acks land within
    /// `--quorum-timeout-ms`. Default 0 = federation disabled, daemon
    /// behaves exactly like v0.6.0.
    #[arg(long, default_value_t = 0)]
    quorum_writes: usize,
    /// Comma-separated list of peer base URLs. Each peer is assumed to
    /// expose `POST /api/v1/sync/push` — the same endpoint the
    /// sync-daemon already uses.
    #[arg(long, value_delimiter = ',')]
    quorum_peers: Vec<String>,
    /// Deadline for quorum-ack collection. After this many ms the
    /// write returns 503 `quorum_not_met`. Default 2000.
    #[arg(long, default_value_t = 2000)]
    quorum_timeout_ms: u64,
    /// Optional mTLS client cert for outbound federation POSTs. Same
    /// cert material the sync-daemon's `--client-cert` accepts.
    #[arg(long)]
    quorum_client_cert: Option<PathBuf>,
    /// Optional mTLS client key for outbound federation POSTs.
    #[arg(long)]
    quorum_client_key: Option<PathBuf>,
    /// Optional root CA cert to trust for outbound federation HTTPS.
    /// Required whenever peers present a cert NOT rooted in Mozilla's
    /// `webpki-roots` bundle (self-signed, private CA, ephemeral test
    /// CA, etc.) — without this, the reqwest rustls-tls client rejects
    /// peer certs and every quorum write times out as `quorum_not_met`.
    /// See #333.
    #[arg(long)]
    quorum_ca_cert: Option<PathBuf>,
    /// v0.6.0.1 (#320) — how often, in seconds, the daemon pulls peers
    /// for any updates it missed while offline or partitioned. 0 disables
    /// the catchup loop entirely. Default 30s keeps a post-partition
    /// node convergent within one interval after resume.
    #[arg(long, default_value_t = 30)]
    catchup_interval_secs: u64,
}

#[derive(Args)]
struct StoreArgs {
    #[arg(long, short, default_value = "mid")]
    tier: String,
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long, short = 'T', allow_hyphen_values = true)]
    title: String,
    /// Content (use - to read from stdin)
    #[arg(long, short, allow_hyphen_values = true)]
    content: String,
    #[arg(long, default_value = "")]
    tags: String,
    #[arg(long, short, default_value_t = 5)]
    priority: i32,
    /// Confidence 0.0-1.0
    #[arg(long, default_value_t = 1.0)]
    confidence: f64,
    /// Source: user, claude, hook, api
    #[arg(long, short = 'S', default_value = "cli")]
    source: String,
    /// Explicit expiry timestamp (RFC3339). Overrides tier default.
    #[arg(long)]
    expires_at: Option<String>,
    /// TTL in seconds. Overrides tier default.
    #[arg(long)]
    ttl_secs: Option<i64>,
    /// Task 1.5 visibility scope: private (default) / team / unit / org / collective.
    /// Stored as `metadata.scope`; affects which agents can recall this memory
    /// when queries use `--as-agent`.
    #[arg(long)]
    scope: Option<String>,
}

#[derive(Args)]
struct UpdateArgs {
    id: String,
    #[arg(long, short = 'T', allow_hyphen_values = true)]
    title: Option<String>,
    #[arg(long, short, allow_hyphen_values = true)]
    content: Option<String>,
    #[arg(long, short)]
    tier: Option<String>,
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long)]
    tags: Option<String>,
    #[arg(long, short)]
    priority: Option<i32>,
    #[arg(long)]
    confidence: Option<f64>,
    /// Expiry timestamp (RFC3339), or empty string to clear
    #[arg(long)]
    expires_at: Option<String>,
}

#[derive(Args)]
struct RecallArgs {
    #[arg(allow_hyphen_values = true)]
    context: String,
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long, default_value_t = 10)]
    limit: usize,
    #[arg(long)]
    tags: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    /// Feature tier for recall: keyword, semantic, smart, autonomous
    #[arg(long, short = 'T')]
    tier: Option<String>,
    /// Task 1.5: querying agent's namespace position. Enables scope-based
    /// visibility filtering (private/team/unit/org/collective).
    #[arg(long)]
    as_agent: Option<String>,
    /// Task 1.11: context-budget-aware recall. Return the top-ranked
    /// memories whose cumulative estimated tokens fit within N. Omit
    /// for unlimited (limit-based only).
    #[arg(long)]
    budget_tokens: Option<usize>,
    /// v0.6.0.0 contextual recall. Comma-separated list of recent
    /// conversation tokens used to bias the query embedding at 70/30
    /// (primary/context). Shifts the recall towards memories that
    /// match both the explicit query and the conversation's nearby
    /// topics.
    #[arg(long, value_delimiter = ',')]
    context_tokens: Option<Vec<String>>,
}

#[derive(Args)]
struct SearchArgs {
    #[arg(allow_hyphen_values = true)]
    query: String,
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long, short)]
    tier: Option<String>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    tags: Option<String>,
    /// Filter by `metadata.agent_id` (exact match)
    #[arg(long)]
    agent_id: Option<String>,
    /// Task 1.5: querying agent's namespace position for scope-based
    /// visibility filtering.
    #[arg(long)]
    as_agent: Option<String>,
}

#[derive(Args)]
struct GetArgs {
    id: String,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long, short)]
    tier: Option<String>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    tags: Option<String>,
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Filter by `metadata.agent_id` (exact match)
    #[arg(long)]
    agent_id: Option<String>,
}

#[derive(Args)]
struct DeleteArgs {
    id: String,
}

#[derive(Args)]
struct PromoteArgs {
    id: String,
    /// Task 1.7: clone this memory into a hierarchical-ancestor namespace
    /// (the original is untouched). Must be an ancestor of the memory's
    /// current namespace. Skips the tier bump — vertical promotion is a
    /// separate axis from tier promotion.
    #[arg(long)]
    to_namespace: Option<String>,
}

#[derive(Args)]
struct ForgetArgs {
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long, short)]
    pattern: Option<String>,
    #[arg(long, short)]
    tier: Option<String>,
}

#[derive(Args)]
struct LinkArgs {
    source_id: String,
    target_id: String,
    #[arg(long, short, default_value = "related_to")]
    relation: String,
}

#[derive(Args)]
struct ConsolidateArgs {
    /// Comma-separated memory IDs
    ids: String,
    #[arg(long, short = 'T', allow_hyphen_values = true)]
    title: String,
    #[arg(long, short = 's', allow_hyphen_values = true)]
    summary: String,
    #[arg(long, short)]
    namespace: Option<String>,
}

#[derive(Args)]
struct ResolveArgs {
    /// ID of the memory that wins (supersedes)
    winner_id: String,
    /// ID of the memory that loses (superseded)
    loser_id: String,
}

#[derive(Args)]
struct SyncDaemonArgs {
    /// Comma-separated list of peer HTTP endpoints to mesh with.
    /// Each URL must point at another `ai-memory serve` instance —
    /// e.g. `http://laptop-b:9077,http://laptop-c:9077`. The local
    /// daemon polls each peer's `/api/v1/sync/since` for new memories
    /// and pushes local deltas via `/api/v1/sync/push`.
    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>,
    /// Seconds between sync cycles. Each cycle reconciles every peer
    /// in parallel (one pull + one push per peer). Defaults to 2 seconds
    /// — the v0.6.0 cadence that keeps a 3-node mesh within a handful
    /// of records of steady-state under heavy writes. Minimum 1.
    #[arg(long, default_value_t = 2)]
    interval: u64,
    /// Optional `X-API-Key` to present to peers that have api-key auth
    /// enabled. Same key is sent to every peer in this invocation; use
    /// separate daemons if peers need distinct keys. Future work
    /// (Task 3b.2): per-peer auth tokens.
    #[arg(long)]
    api_key: Option<String>,
    /// Cap on the number of memories transferred per peer per cycle.
    /// Prevents an initial cold-start sync from hogging one cycle;
    /// subsequent cycles pick up the remainder. Defaults to 500.
    #[arg(long, default_value_t = 500)]
    batch_size: usize,
    /// Layer 2 client-cert PEM used when the peer demands mTLS. Pair
    /// with `--client-key`. If the peer has `--mtls-allowlist` set and
    /// this cert's SHA-256 fingerprint isn't on it, the TLS handshake
    /// is rejected before the daemon ever reaches the sync endpoints.
    #[arg(long, requires = "client_key")]
    client_cert: Option<PathBuf>,
    /// Layer 2 client-key PEM. Must pair with `--client-cert`.
    #[arg(long, requires = "client_cert")]
    client_key: Option<PathBuf>,
    /// Disable server-cert verification on outbound HTTPS to peers.
    /// **DANGEROUS** — accepts any server cert without validation,
    /// enabling MITM attacks. Use only in trusted local labs with
    /// self-signed peer certs and no mTLS. For untrusted networks,
    /// pair `--client-cert` with the peer's `--mtls-allowlist` so
    /// the peer authenticates US (red-team #232).
    #[arg(long, default_value_t = false)]
    insecure_skip_server_verify: bool,
}

#[derive(Args)]
struct SyncArgs {
    /// Path to the remote database to sync with
    remote_db: PathBuf,
    /// Direction: pull, push, or merge
    #[arg(long, short, default_value = "merge")]
    direction: String,
    /// Trust `metadata.agent_id` in remote memories (default: restamp with caller's id).
    /// Only use this when syncing between databases you fully control (e.g., your own backup).
    #[arg(long, default_value_t = false)]
    trust_source: bool,
    /// Phase 3 foundation (issue #224): preview what would change without
    /// writing anything. Counts new / updated / unchanged memories and
    /// links in each direction. Uses today's timestamp-aware merge
    /// semantics; CRDT-lite field-level diagnostics land with #224 Task 3a.1.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Args)]
struct ImportArgs {
    /// Trust `metadata.agent_id` in imported JSON (default: restamp with caller's id).
    /// Only use this when importing a JSON export you fully trust (e.g., your own backup).
    #[arg(long, default_value_t = false)]
    trust_source: bool,
}

#[derive(Args)]
struct AutoConsolidateArgs {
    /// Namespace to consolidate
    #[arg(long, short)]
    namespace: Option<String>,
    /// Only consolidate short-term memories
    #[arg(long, default_value_t = false)]
    short_only: bool,
    /// Minimum number of memories to trigger consolidation
    #[arg(long, default_value_t = 3)]
    min_count: usize,
    /// Dry run — show what would be consolidated without doing it
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Args)]
struct CompletionsArgs {
    shell: Shell,
}

fn auto_namespace() -> String {
    // Try git remote name
    if let Ok(out) = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !url.is_empty() {
            // Extract repo name from URL
            if let Some(name) = url.rsplit('/').next() {
                let name = name.trim_end_matches(".git");
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    // Fallback to current directory name
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "global".to_string())
}

fn human_age(iso: &str) -> String {
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return iso.to_string();
    };
    let dur = Utc::now().signed_duration_since(dt);
    if dur.num_seconds() < 60 {
        return "just now".to_string();
    }
    if dur.num_minutes() < 60 {
        return format!("{}m ago", dur.num_minutes());
    }
    if dur.num_hours() < 24 {
        return format!("{}h ago", dur.num_hours());
    }
    if dur.num_days() < 30 {
        return format!("{}d ago", dur.num_days());
    }
    format!("{}mo ago", dur.num_days() / 30)
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    color::init();
    let app_config = config::AppConfig::load();
    config::AppConfig::write_default_if_missing();
    // #198: config → env mapping for agent_id anonymization. Env var already
    // set by the caller wins; config is only applied when the env is unset.
    if app_config.effective_anonymize_default() && std::env::var("AI_MEMORY_ANONYMIZE").is_err() {
        // SAFETY: single-threaded startup before any worker threads spawn.
        unsafe { std::env::set_var("AI_MEMORY_ANONYMIZE", "1") };
    }
    let cli = Cli::parse();
    // v0.6.0.0: read the SQLCipher passphrase from a file and export it as
    // AI_MEMORY_DB_PASSPHRASE for the duration of the process. File path
    // comes from the --db-passphrase-file flag (global). No-op on standard
    // SQLite builds (the env var is ignored unless the binary was built
    // with --features sqlcipher).
    if let Some(path) = &cli.db_passphrase_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading passphrase file {}", path.display()))?;
        let passphrase = raw.trim_end_matches(['\n', '\r']).to_string();
        if passphrase.is_empty() {
            anyhow::bail!("passphrase file {} is empty", path.display());
        }
        // SAFETY: single-threaded startup before any worker threads spawn.
        unsafe { std::env::set_var("AI_MEMORY_DB_PASSPHRASE", passphrase) };
    }
    let db_path = app_config.effective_db(&cli.db);
    let j = cli.json;
    let cli_agent_id: Option<String> = cli.agent_id.clone();
    // Track whether command writes to DB (for WAL checkpoint)
    let is_write_command = matches!(
        cli.command,
        Command::Store(_)
            | Command::Update(_)
            | Command::Delete(_)
            | Command::Promote(_)
            | Command::Forget(_)
            | Command::Link(_)
            | Command::Consolidate(_)
            | Command::Resolve(_)
            | Command::Sync(_)
            | Command::SyncDaemon(_)
            | Command::Import(_)
            | Command::AutoConsolidate(_)
            | Command::Gc
    );
    let db_path_for_checkpoint = if is_write_command {
        Some(db_path.clone())
    } else {
        None
    };

    let result = match cli.command {
        Command::Serve(a) => serve(db_path, a, &app_config).await,
        Command::Mcp { tier } => {
            let feature_tier = app_config.effective_tier(Some(&tier));
            mcp::run_mcp_server(&db_path, feature_tier, &app_config)?;
            Ok(())
        }
        Command::Store(a) => cmd_store(&db_path, a, j, &app_config, cli_agent_id.as_deref()),
        Command::Update(a) => cmd_update(&db_path, &a, j),
        Command::Recall(a) => cmd_recall(&db_path, &a, j, &app_config),
        Command::Search(a) => cmd_search(&db_path, &a, j, &app_config),
        Command::Get(a) => cmd_get(&db_path, &a, j),
        Command::List(a) => cmd_list(&db_path, &a, j, &app_config),
        Command::Delete(a) => cmd_delete(&db_path, &a, j, cli_agent_id.as_deref()),
        Command::Promote(a) => cmd_promote(&db_path, &a, j, cli_agent_id.as_deref()),
        Command::Forget(a) => cmd_forget(&db_path, &a, j),
        Command::Link(a) => cmd_link(&db_path, &a, j),
        Command::Consolidate(a) => cmd_consolidate(&db_path, a, j, cli_agent_id.as_deref()),
        Command::Resolve(a) => cmd_resolve(&db_path, &a, j),
        Command::Shell => cmd_shell(&db_path),
        Command::Sync(a) => cmd_sync(&db_path, &a, j, cli_agent_id.as_deref()),
        Command::SyncDaemon(a) => cmd_sync_daemon(&db_path, a, cli_agent_id.as_deref()).await,
        Command::AutoConsolidate(a) => {
            cmd_auto_consolidate(&db_path, &a, j, cli_agent_id.as_deref())
        }
        Command::Gc => cmd_gc(&db_path, j, &app_config),
        Command::Stats => cmd_stats(&db_path, j),
        Command::Namespaces => cmd_namespaces(&db_path, j),
        Command::Export => cmd_export(&db_path),
        Command::Import(a) => cmd_import(&db_path, &a, j, cli_agent_id.as_deref()),
        Command::Completions(a) => {
            generate(
                a.shell,
                &mut Cli::command(),
                "ai-memory",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Command::Man => {
            let cmd = Cli::command();
            let man = clap_mangen::Man::new(cmd);
            man.render(&mut std::io::stdout())?;
            Ok(())
        }
        Command::Mine(a) => cmd_mine(&db_path, a, j, &app_config, cli_agent_id.as_deref()),
        Command::Archive(a) => cmd_archive(&db_path, a, j),
        Command::Agents(a) => cmd_agents(&db_path, a, j),
        Command::Pending(a) => cmd_pending(&db_path, a, j, cli_agent_id.as_deref()),
        Command::Backup(a) => cmd_backup(&db_path, &a, j),
        Command::Restore(a) => cmd_restore(&db_path, &a, j),
        Command::Curator(a) => cmd_curator(&db_path, &a, &app_config).await,
        #[cfg(feature = "sal")]
        Command::Migrate(a) => cmd_migrate(&a).await,
    };

    // WAL checkpoint after write commands to prevent unbounded WAL growth
    if result.is_ok()
        && let Some(cp_path) = db_path_for_checkpoint
        && let Ok(conn) = db::open(&cp_path)
    {
        let _ = db::checkpoint(&conn);
    }

    result
}

#[allow(clippy::too_many_lines)]
async fn serve(db_path: PathBuf, args: ServeArgs, app_config: &config::AppConfig) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("ai_memory=info".parse()?)
                .add_directive("tower_http=info".parse()?),
        )
        .init();

    let resolved_ttl = app_config.effective_ttl();
    let archive_on_gc = app_config.effective_archive_on_gc();
    let conn = db::open(&db_path)?;

    // Issue #219: build the embedder + HNSW index up front so HTTP write
    // paths can populate them. Previously the daemon never constructed an
    // embedder, silently excluding every HTTP-authored memory from semantic
    // recall. Build only when the configured feature tier enables it —
    // keyword-only deployments keep their zero-dep, zero-RAM profile.
    // Daemon has no per-invocation tier override; honour the config tier.
    let feature_tier = app_config.effective_tier(None);
    let tier_config = feature_tier.config();
    // The HF-Hub sync API and candle model-load are blocking CPU work that
    // internally spin their own tokio runtime. Running them directly in this
    // async context panics with "Cannot drop a runtime in a context where
    // blocking is not allowed." Move the whole construction onto the blocking
    // pool so the inner runtime is owned by a dedicated thread.
    let embedder: Option<embeddings::Embedder> =
        if let Some(emb_model) = tier_config.embedding_model {
            let embed_url = app_config.effective_embed_url().to_string();
            let build = tokio::task::spawn_blocking(move || {
                let embed_client = llm::OllamaClient::new_with_url(&embed_url, "nomic-embed-text")
                    .ok()
                    .map(Arc::new);
                embeddings::Embedder::for_model(emb_model, embed_client)
            })
            .await?;
            match build {
                Ok(emb) => {
                    tracing::info!(
                        "embedder loaded ({}) — tier={} semantic recall enabled",
                        emb.model_description(),
                        feature_tier.as_str()
                    );
                    Some(emb)
                }
                Err(e) => {
                    // v0.6.2 (#327): make embedder load failures loud. The
                    // prior WARN level was easy to miss in DO droplet logs,
                    // which led to scenario-18 black-holing (semantic recall
                    // falling back to keyword-only without the operator
                    // noticing). An ERROR-level log with an obvious marker
                    // surfaces this immediately in `journalctl -u ai-memory`
                    // or tail -f /var/log/ai-memory-serve.log.
                    tracing::error!(
                        "⚠️  EMBEDDER LOAD FAILED — tier={} requested semantic features, \
                         but embedder init errored: {e}. Daemon falls back to keyword-only. \
                         Semantic recall, sync_push embedding refresh (#322), and HNSW index \
                         will be NO-OPS. Check network egress to HuggingFace Hub + available \
                         memory for model weights. To force keyword-only explicitly (silences \
                         this error), set `tier = \"keyword\"` in config.toml.",
                        feature_tier.as_str()
                    );
                    None
                }
            }
        } else {
            tracing::info!(
                "embedder disabled — tier={} keyword-only (FTS5); semantic recall not wired",
                feature_tier.as_str()
            );
            None
        };
    let vector_index = if embedder.is_some() {
        match db::get_all_embeddings(&conn) {
            Ok(entries) if !entries.is_empty() => Some(hnsw::VectorIndex::build(entries)),
            _ => Some(hnsw::VectorIndex::empty()),
        }
    } else {
        None
    };

    let db_state: handlers::Db = Arc::new(Mutex::new((
        conn,
        db_path.clone(),
        resolved_ttl,
        archive_on_gc,
    )));
    // Federation: parsed from --quorum-writes / --quorum-peers. Disabled
    // entirely when either is absent — daemon behaves exactly like
    // v0.6.0 in that case.
    let federation = federation::FederationConfig::build(
        args.quorum_writes,
        &args.quorum_peers,
        std::time::Duration::from_millis(args.quorum_timeout_ms),
        args.quorum_client_cert.as_deref(),
        args.quorum_client_key.as_deref(),
        args.quorum_ca_cert.as_deref(),
        format!("host:{}", gethostname::gethostname().to_string_lossy()),
    )
    .context("federation config")?;
    if let Some(ref fed) = federation {
        tracing::info!(
            "federation enabled: W={} over {} peer(s), timeout {}ms",
            fed.policy.w,
            fed.peer_count(),
            args.quorum_timeout_ms,
        );
        // v0.6.0.1 (#320) — post-partition catchup poller. Closes the gap
        // where a rejoining node only sees post-resume writes.
        if args.catchup_interval_secs > 0 {
            let interval = std::time::Duration::from_secs(args.catchup_interval_secs);
            tracing::info!(
                "catchup loop enabled: polling {} peer(s) every {}s",
                fed.peer_count(),
                args.catchup_interval_secs,
            );
            federation::spawn_catchup_loop(fed.clone(), db_state.clone(), interval);
        } else {
            tracing::info!("catchup loop disabled (--catchup-interval-secs=0)");
        }
    }

    let app_state = handlers::AppState {
        db: db_state.clone(),
        embedder: Arc::new(embedder),
        vector_index: Arc::new(Mutex::new(vector_index)),
        federation: Arc::new(federation),
    };
    let state = db_state;

    // Automatic GC
    let gc_state = state.clone();
    let archive_max_days = app_config.archive_max_days;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(GC_INTERVAL_SECS)).await;
            let lock = gc_state.lock().await;
            match db::gc(&lock.0, lock.3) {
                Ok(n) if n > 0 => tracing::info!("gc: expired {n} memories"),
                _ => {}
            }
            // Auto-purge old archives if configured
            match db::auto_purge_archive(&lock.0, archive_max_days) {
                Ok(n) if n > 0 => tracing::info!("gc: purged {n} old archived memories"),
                _ => {}
            }
        }
    });

    // v0.6.0 GA: periodic WAL checkpoint. Under continuous writes the WAL
    // file grows until SQLite's auto-checkpoint fires (every 1000 pages by
    // default) — which is inconsistent timing and can leave the file at
    // hundreds of MB between auto-checkpoints. A dedicated task running on
    // a fixed cadence keeps the WAL bounded and makes operational storage
    // behaviour predictable. We stagger from GC to avoid lock-contention
    // bursts. See docs/ARCHITECTURAL_LIMITS.md for why this workaround is
    // necessary in a single-connection daemon.
    let checkpoint_state = state.clone();
    tokio::spawn(async move {
        // First checkpoint runs halfway through the GC interval so the two
        // long-running maintenance tasks never overlap on cold start.
        tokio::time::sleep(tokio::time::Duration::from_secs(
            WAL_CHECKPOINT_INTERVAL_SECS / 2,
        ))
        .await;
        loop {
            {
                let lock = checkpoint_state.lock().await;
                match db::checkpoint(&lock.0) {
                    Ok(()) => tracing::debug!("wal checkpoint: ok"),
                    Err(e) => tracing::warn!("wal checkpoint failed: {e}"),
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(
                WAL_CHECKPOINT_INTERVAL_SECS,
            ))
            .await;
        }
    });

    // Graceful shutdown with WAL checkpoint
    let shutdown_state = state.clone();
    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down — checkpointing WAL");
        let lock = shutdown_state.lock().await;
        let _ = db::checkpoint(&lock.0);
    };

    let api_key_state = handlers::ApiKeyState {
        key: app_config.api_key.clone(),
    };
    if api_key_state.key.is_some() {
        tracing::info!("API key authentication enabled");
    }

    let app = Router::new()
        .route("/api/v1/health", get(handlers::health))
        // v0.6.0.0: Prometheus scrape endpoint. Exposed at both /metrics
        // (the community convention) and /api/v1/metrics (consistent with
        // the rest of the REST surface).
        .route("/metrics", get(handlers::prometheus_metrics))
        .route("/api/v1/metrics", get(handlers::prometheus_metrics))
        .route("/api/v1/memories", get(handlers::list_memories))
        .route("/api/v1/memories", post(handlers::create_memory))
        .route("/api/v1/memories/bulk", post(handlers::bulk_create))
        .route("/api/v1/memories/{id}", get(handlers::get_memory))
        .route("/api/v1/memories/{id}", put(handlers::update_memory))
        .route("/api/v1/memories/{id}", delete(handlers::delete_memory))
        .route(
            "/api/v1/memories/{id}/promote",
            post(handlers::promote_memory),
        )
        .route("/api/v1/search", get(handlers::search_memories))
        .route("/api/v1/recall", get(handlers::recall_memories_get))
        .route("/api/v1/recall", post(handlers::recall_memories_post))
        .route("/api/v1/forget", post(handlers::forget_memories))
        .route("/api/v1/consolidate", post(handlers::consolidate_memories))
        .route(
            "/api/v1/contradictions",
            get(handlers::detect_contradictions),
        )
        .route("/api/v1/links", post(handlers::create_link))
        .route("/api/v1/links", delete(handlers::delete_link))
        .route("/api/v1/links/{id}", get(handlers::get_links))
        .route("/api/v1/namespaces", get(handlers::list_namespaces))
        .route("/api/v1/stats", get(handlers::get_stats))
        .route("/api/v1/gc", post(handlers::run_gc))
        .route("/api/v1/export", get(handlers::export_memories))
        .route("/api/v1/import", post(handlers::import_memories))
        .route("/api/v1/archive", get(handlers::list_archive))
        .route("/api/v1/archive", delete(handlers::purge_archive))
        .route(
            "/api/v1/archive/{id}/restore",
            post(handlers::restore_archive),
        )
        .route("/api/v1/archive/stats", get(handlers::archive_stats))
        .route("/api/v1/agents", get(handlers::list_agents))
        .route("/api/v1/agents", post(handlers::register_agent))
        .route("/api/v1/pending", get(handlers::list_pending))
        .route(
            "/api/v1/pending/{id}/approve",
            post(handlers::approve_pending),
        )
        .route(
            "/api/v1/pending/{id}/reject",
            post(handlers::reject_pending),
        )
        // Phase 3 foundation (issue #224) — peer-to-peer sync endpoints.
        // Skeletons running today's timestamp-aware merge; field-level CRDT
        // and streaming land in v0.8.0.
        .route("/api/v1/sync/push", post(handlers::sync_push))
        .route("/api/v1/sync/since", get(handlers::sync_since))
        .layer(axum::middleware::from_fn_with_state(
            api_key_state,
            handlers::api_key_auth,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024)) // 2MB default (bulk/import bodies capped at MAX_BULK_SIZE * per-memory limit)
        .layer(CorsLayer::new())
        .with_state(app_state);

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("database: {}", db_path.display());

    // Native TLS (Layer 1): if both --tls-cert and --tls-key are provided,
    // bind via axum-server + rustls. Plain HTTP otherwise — backward
    // compatible with every prior release. The `requires = …` clap
    // attributes prevent the half-configured case.
    if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        // rustls 0.23 needs an explicit CryptoProvider; install ring
        // before any TLS setup. Idempotent — second install is a
        // harmless no-op via ignore.
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Load TLS / mTLS config BEFORE printing the "listening" log
        // so a misconfigured cert / key / allowlist surfaces the error
        // first (red-team #248).
        let tls_config = if let Some(allowlist_path) = &args.mtls_allowlist {
            tracing::info!(
                "mTLS enabled — client certs required. Allowlist: {}",
                allowlist_path.display()
            );
            load_mtls_rustls_config(cert, key, allowlist_path).await?
        } else {
            tracing::warn!(
                "TLS enabled but mTLS NOT configured — sync endpoints \
                 (/api/v1/sync/push, /api/v1/sync/since) accept any client. \
                 Set --mtls-allowlist for production peer-mesh deployments \
                 (red-team #231)."
            );
            load_rustls_config(cert, key).await?
        };
        tracing::info!("ai-memory listening on https://{addr}");
        let socket_addr: std::net::SocketAddr = addr.parse()?;
        // axum-server doesn't have a direct graceful-shutdown on the
        // TLS builder yet; spawn the signal listener on the Handle
        // instead so ctrl_c triggers a graceful shutdown. Window is
        // operator-configurable via --shutdown-grace-secs (default 30,
        // bumped from 10 in v0.6.0 — red-team #233).
        let grace = std::time::Duration::from_secs(args.shutdown_grace_secs);
        let handle = axum_server::Handle::new();
        let handle_clone = handle.clone();
        tokio::spawn(async move {
            shutdown.await;
            handle_clone.graceful_shutdown(Some(grace));
        });
        axum_server::bind_rustls(socket_addr, tls_config)
            .handle(handle)
            .serve(app.into_make_service())
            .await?;
    } else {
        tracing::warn!(
            "TLS NOT enabled — sync endpoints (/api/v1/sync/push, \
             /api/v1/sync/since) accept any caller over plain HTTP. \
             Set --tls-cert + --tls-key + --mtls-allowlist for production \
             peer-mesh deployments (red-team #231)."
        );
        tracing::info!("ai-memory listening on http://{addr}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await?;
    }
    Ok(())
}

/// Load a PEM cert + PEM key (PKCS#8 or RSA) into an `axum-server`
/// rustls config. Returns an error with a specific message for the
/// operator rather than letting rustls' wrapped IO error bubble up —
/// TLS misconfigurations are the #1 new-deploy footgun.
async fn load_rustls_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<axum_server::tls_rustls::RustlsConfig> {
    let cert = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("failed to read TLS cert from {}", cert_path.display()))?;
    let key = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("failed to read TLS key from {}", key_path.display()))?;
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(cert, key)
        .await
        .context(
            "failed to parse TLS cert/key — ensure PEM-encoded (cert may be fullchain; \
                 key must be PKCS#8 or RSA)",
        )?;
    Ok(config)
}

// ---------------------------------------------------------------------------
// Layer 2 — mTLS with SHA-256 fingerprint allowlist.
//
// Builds a rustls ServerConfig that:
//   1. Presents the local cert/key (same as Layer 1).
//   2. Demands a client certificate on every connection.
//   3. Accepts the client cert only if its SHA-256 fingerprint is on the
//      operator-configured allowlist. Any other cert — including ones
//      signed by trusted CAs — is rejected.
//
// This is the fastest path to "only authorised peers can even connect"
// without depending on a PKI/CA ecosystem. Fingerprint pinning is a
// well-understood primitive (HTTP Public Key Pinning, SSH host keys).
// Task 2b (post-v0.6.0) adds fingerprint → agent_id mapping so the
// handler can refuse requests whose `sender_agent_id` doesn't match
// the cert's expected identity.
// ---------------------------------------------------------------------------

/// Load a rustls server config with client-cert-fingerprint verification.
async fn load_mtls_rustls_config(
    cert_path: &Path,
    key_path: &Path,
    allowlist_path: &Path,
) -> Result<axum_server::tls_rustls::RustlsConfig> {
    let allowlist = load_fingerprint_allowlist(allowlist_path).await?;
    if allowlist.is_empty() {
        anyhow::bail!(
            "mTLS allowlist at {} is empty — refuse to start rather than silently accept all peers",
            allowlist_path.display()
        );
    }

    let cert_pem = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("failed to read TLS cert from {}", cert_path.display()))?;
    let key_pem = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("failed to read TLS key from {}", key_path.display()))?;

    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pki_pem_iter_certs(&cert_pem)?;
    let key = rustls_pki_pem_parse_private_key(&key_pem)?;

    let verifier = Arc::new(FingerprintAllowlistVerifier { allowlist });
    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("failed to build rustls ServerConfig for mTLS")?;

    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(server_config),
    ))
}

/// Parse the allowlist file: one SHA-256 fingerprint per line, case-insensitive
/// hex with optional `:` separators. Empty lines and `#` comments are skipped.
async fn load_fingerprint_allowlist(path: &Path) -> Result<std::collections::HashSet<[u8; 32]>> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read mTLS allowlist from {}", path.display()))?;
    let mut set = std::collections::HashSet::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Accept a leading `sha256:` marker for forward-compat with richer formats.
        let hex_part = line.strip_prefix("sha256:").unwrap_or(line);
        // Ultrareview #338: reject any non-hex, non-colon character —
        // including embedded whitespace/tabs. Previously the parser
        // stripped only `:` and relied on the length check to catch
        // whitespace, but silent acceptance of copy-paste artefacts
        // (e.g. soft-wraps producing internal spaces) would produce
        // misleading parse errors further down rather than a clear
        // "whitespace not allowed" signal. Keep it strict.
        if let Some(bad) = hex_part
            .chars()
            .find(|c| !c.is_ascii_hexdigit() && *c != ':')
        {
            anyhow::bail!(
                "mTLS allowlist line {}: unexpected character {:?} — \
                 entries must be 64 hex chars with optional `:` separators",
                lineno + 1,
                bad
            );
        }
        let hex_clean: String = hex_part.chars().filter(|c| *c != ':').collect();
        if hex_clean.len() != 64 {
            anyhow::bail!(
                "mTLS allowlist line {}: expected 64 hex chars (optionally with `:` separators), got {}",
                lineno + 1,
                hex_clean.len()
            );
        }
        let mut bytes = [0u8; 32];
        for i in 0..32 {
            bytes[i] = u8::from_str_radix(&hex_clean[i * 2..i * 2 + 2], 16)
                .with_context(|| format!("mTLS allowlist line {}: invalid hex", lineno + 1))?;
        }
        set.insert(bytes);
    }
    Ok(set)
}

fn rustls_pki_pem_iter_certs(
    pem: &[u8],
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject as _;
    let mut cursor = std::io::Cursor::new(pem);
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_reader_iter(&mut cursor)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse TLS cert PEM")?;
    if certs.is_empty() {
        anyhow::bail!("TLS cert PEM contained no certificates");
    }
    Ok(certs)
}

fn rustls_pki_pem_parse_private_key(
    pem: &[u8],
) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    use rustls::pki_types::pem::PemObject as _;
    let mut cursor = std::io::Cursor::new(pem);
    let key = rustls::pki_types::PrivateKeyDer::from_pem_reader(&mut cursor)
        .context("failed to parse TLS key PEM — expected PKCS#8, RSA, or SEC1")?;
    Ok(key)
}

/// Custom `ClientCertVerifier` that accepts only client certs whose SHA-256
/// DER fingerprint is on the allowlist. Ignores CA chain — fingerprint
/// pinning is the trust anchor here, same model as SSH `known_hosts`.
#[derive(Debug)]
struct FingerprintAllowlistVerifier {
    allowlist: std::collections::HashSet<[u8; 32]>,
}

impl rustls::server::danger::ClientCertVerifier for FingerprintAllowlistVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let fp: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if self.allowlist.contains(&fp) {
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "client cert fingerprint {} not in mTLS allowlist",
                hex_short(&fp)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn hex_short(fp: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(12);
    for b in &fp[..6] {
        let _ = write!(s, "{b:02x}");
    }
    s.push('…');
    s
}

/// Build a rustls `ClientConfig` with client-cert auth and a
/// "dangerously-accept-any-server-cert" verifier. Used by the
/// sync-daemon to present its client cert on every outbound request
/// while connecting to peers with self-signed server certs. Peer
/// authenticity is established on the other direction (they verify
/// us via `--mtls-allowlist`).
async fn build_rustls_client_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<rustls::ClientConfig> {
    let cert_pem = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("failed to read client cert from {}", cert_path.display()))?;
    let key_pem = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("failed to read client key from {}", key_path.display()))?;

    let certs = rustls_pki_pem_iter_certs(&cert_pem)?;
    let key = rustls_pki_pem_parse_private_key(&key_pem)?;

    // SAFETY: we accept any server cert because the server authenticates
    // US via our client cert fingerprint (Layer 2's trust anchor), not
    // via server-cert validation. Server-cert pinning is a Layer 2b
    // refinement tracked in #224.
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousAnyServerVerifier))
        .with_client_auth_cert(certs, key)
        .context("failed to build rustls ClientConfig with client cert")?;
    Ok(config)
}

/// `ServerCertVerifier` that accepts any peer certificate. Safe ONLY when
/// paired with a strong reverse authentication channel — in our case the
/// peer's `--mtls-allowlist` fingerprint-pins our client cert.
#[derive(Debug)]
struct DangerousAnyServerVerifier;

impl rustls::client::danger::ServerCertVerifier for DangerousAnyServerVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// --- CLI ---

#[allow(clippy::too_many_lines)]
fn cmd_store(
    db_path: &Path,
    args: StoreArgs,
    json_out: bool,
    app_config: &config::AppConfig,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    let resolved_ttl = app_config.effective_ttl();
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());
    let tier = Tier::from_str(&args.tier)
        .ok_or_else(|| anyhow::anyhow!("invalid tier: {} (use short, mid, long)", args.tier))?;
    let namespace = args.namespace.unwrap_or_else(auto_namespace);
    let content = if args.content == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        args.content
    };
    let tags: Vec<String> = args
        .tags
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Validate all fields before touching the DB
    validate::validate_title(&args.title)?;
    validate::validate_content(&content)?;
    validate::validate_namespace(&namespace)?;
    validate::validate_source(&args.source)?;
    validate::validate_tags(&tags)?;
    validate::validate_priority(args.priority)?;
    validate::validate_confidence(args.confidence)?;
    validate::validate_expires_at(args.expires_at.as_deref())?;
    validate::validate_ttl_secs(args.ttl_secs)?;

    let now = Utc::now();
    let expires_at = args.expires_at.or_else(|| {
        args.ttl_secs
            .or(resolved_ttl.ttl_for_tier(&tier))
            .map(|s| (now + Duration::seconds(s)).to_rfc3339())
    });
    // Resolve agent_id via the NHI-hardened precedence chain. `cli_agent_id`
    // already reflects `--agent-id` flag or `AI_MEMORY_AGENT_ID` env (clap
    // merges both). When neither is set we fall through to the host/anonymous
    // defaults provided by `crate::identity`.
    let agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
    let mut metadata = models::default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(agent_id.clone()),
        );
    }
    // #151 scope: validate + merge into metadata
    if let Some(ref s) = args.scope {
        validate::validate_scope(s)?;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("scope".to_string(), serde_json::Value::String(s.clone()));
        }
    }

    let mem = models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace,
        title: args.title,
        content,
        tags,
        priority: args.priority.clamp(1, 10),
        confidence: args.confidence.clamp(0.0, 1.0),
        source: args.source,
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
        metadata,
    };

    // Task 1.9: governance enforcement (store-side). Payload is the full
    // Memory so Task 1.10's execute_pending_action can replay it on approval.
    {
        use models::{GovernanceDecision, GovernedAction};
        let payload = serde_json::to_value(&mem).unwrap_or_default();
        match db::enforce_governance(
            &conn,
            GovernedAction::Store,
            &mem.namespace,
            &agent_id,
            None,
            None,
            &payload,
        )? {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                eprintln!("store denied by governance: {reason}");
                std::process::exit(1);
            }
            GovernanceDecision::Pending(pending_id) => {
                if json_out {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "store",
                            "namespace": &mem.namespace,
                        })
                    );
                } else {
                    println!(
                        "store queued for approval: pending_id={pending_id} ns={}",
                        &mem.namespace
                    );
                }
                return Ok(());
            }
        }
    }
    let contradictions =
        db::find_contradictions(&conn, &mem.title, &mem.namespace).unwrap_or_default();
    let actual_id = db::insert(&conn, &mem)?;
    // Exclude self-ID from contradictions (upsert may reuse existing ID)
    let filtered: Vec<&String> = contradictions
        .iter()
        .filter(|c| c.id != mem.id && c.id != actual_id)
        .map(|c| &c.id)
        .collect();
    if json_out {
        let mut j = serde_json::to_value(&mem)?;
        j["id"] = serde_json::json!(actual_id);
        // Exclude self-ID from contradictions (happens on upsert)
        let filtered: Vec<&String> = contradictions
            .iter()
            .filter(|c| c.id != actual_id)
            .map(|c| &c.id)
            .collect();
        if !filtered.is_empty() {
            j["potential_contradictions"] = serde_json::json!(filtered);
        }
        println!("{}", serde_json::to_string(&j)?);
    } else {
        println!(
            "stored: {} [{}] (ns={})",
            actual_id, mem.tier, mem.namespace
        );
        if !filtered.is_empty() {
            eprintln!(
                "warning: {} similar memories found in same namespace (potential contradictions)",
                filtered.len()
            );
        }
    }
    Ok(())
}

fn cmd_update(db_path: &Path, args: &UpdateArgs, json_out: bool) -> Result<()> {
    validate::validate_id(&args.id)?;
    let conn = db::open(db_path)?;
    // Resolve prefix if exact ID not found
    let resolved_id = if db::get(&conn, &args.id)?.is_some() {
        args.id.clone()
    } else if let Some(mem) = db::get_by_prefix(&conn, &args.id)? {
        mem.id
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    };
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let tags: Option<Vec<String>> = args.tags.as_ref().map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    // Validate present fields
    if let Some(ref t) = args.title {
        validate::validate_title(t)?;
    }
    if let Some(ref c) = args.content {
        validate::validate_content(c)?;
    }
    if let Some(ref ns) = args.namespace {
        validate::validate_namespace(ns)?;
    }
    if let Some(ref tags) = tags {
        validate::validate_tags(tags)?;
    }
    if let Some(p) = args.priority {
        validate::validate_priority(p)?;
    }
    if let Some(c) = args.confidence {
        validate::validate_confidence(c)?;
    }
    if let Some(ref ts) = args.expires_at
        && !ts.is_empty()
    {
        validate::validate_expires_at_format(ts)?;
    }
    let (found, _content_changed) = db::update(
        &conn,
        &resolved_id,
        args.title.as_deref(),
        args.content.as_deref(),
        tier.as_ref(),
        args.namespace.as_deref(),
        tags.as_ref(),
        args.priority,
        args.confidence,
        args.expires_at.as_deref(),
        None,
    )?;
    if !found {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    if let Some(mem) = db::get(&conn, &resolved_id)? {
        if json_out {
            println!("{}", serde_json::to_string(&mem)?);
        } else {
            println!("updated: {} [{}]", mem.id, mem.title);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn cmd_recall(
    db_path: &Path,
    args: &RecallArgs,
    json_out: bool,
    app_config: &config::AppConfig,
) -> Result<()> {
    // #151: validate --as-agent namespace
    if let Some(ref a) = args.as_agent {
        validate::validate_namespace(a)?;
    }
    let conn = db::open(db_path)?;
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());

    // Resolve feature tier
    let feature_tier = app_config.effective_tier(args.tier.as_deref());
    let tier_config = feature_tier.config();

    // Initialize embedder if tier supports it
    let embedder = if let Some(ref emb_model) = tier_config.embedding_model {
        let ollama_client = if tier_config.llm_model.is_some() {
            let ollama_url = app_config.effective_ollama_url();
            llm::OllamaClient::new_with_url(ollama_url, "nomic-embed-text")
                .ok()
                .map(Arc::new)
        } else {
            None
        };
        let embed_client = {
            let embed_url = app_config.effective_embed_url();
            let ollama_url = app_config.effective_ollama_url();
            if embed_url == ollama_url {
                ollama_client.clone()
            } else {
                llm::OllamaClient::new_with_url(embed_url, "nomic-embed-text")
                    .ok()
                    .map(Arc::new)
                    .or(ollama_client.clone())
            }
        };
        match embeddings::Embedder::for_model(*emb_model, embed_client) {
            Ok(emb) => {
                eprintln!("ai-memory: embedder loaded ({})", emb.model_description());
                // Backfill embeddings for memories that don't have them
                if let Ok(unembedded) = db::get_unembedded_ids(&conn)
                    && !unembedded.is_empty()
                {
                    eprintln!("ai-memory: backfilling {} memories...", unembedded.len());
                    let mut ok = 0usize;
                    for (id, title, content) in &unembedded {
                        let text = format!("{title} {content}");
                        if let Ok(embedding) = emb.embed(&text)
                            && db::set_embedding(&conn, id, &embedding).is_ok()
                        {
                            ok += 1;
                        }
                    }
                    eprintln!("ai-memory: backfilled {}/{}", ok, unembedded.len());
                }
                Some(emb)
            }
            Err(e) => {
                eprintln!("ai-memory: embedder failed: {e}, falling back to keyword");
                None
            }
        }
    } else {
        None
    };

    // Build HNSW vector index if embedder is available
    let vector_index = if embedder.is_some() {
        match db::get_all_embeddings(&conn) {
            Ok(entries) if !entries.is_empty() => Some(hnsw::VectorIndex::build(entries)),
            _ => Some(hnsw::VectorIndex::empty()),
        }
    } else {
        None
    };

    // Initialize cross-encoder reranker for autonomous tier
    let reranker = if tier_config.cross_encoder {
        Some(reranker::CrossEncoder::new_neural())
    } else {
        None
    };

    let resolved_ttl = app_config.effective_ttl();
    let resolved_scoring = app_config.effective_scoring();

    // Perform recall: hybrid if embedder available, keyword otherwise
    let (results, tokens_used, mode) = if let Some(ref emb) = embedder {
        match emb.embed(&args.context) {
            Ok(primary_emb) => {
                // v0.6.0.0 contextual recall. Fuse the primary query
                // embedding with an embedding over recent conversation
                // tokens (caller-supplied) at 70/30. Fusion is done
                // caller-side so recall_hybrid stays unaware of the bias —
                // the vector it receives is the final query direction.
                let query_emb = match args.context_tokens.as_deref() {
                    Some(tokens) if !tokens.is_empty() => {
                        let joined = tokens.join(" ");
                        match emb.embed(&joined) {
                            Ok(ctx_emb) => embeddings::Embedder::fuse(&primary_emb, &ctx_emb, 0.7),
                            Err(e) => {
                                eprintln!(
                                    "ai-memory: context_tokens embed failed: {e}, using primary only"
                                );
                                primary_emb
                            }
                        }
                    }
                    _ => primary_emb,
                };
                let (results, tokens_used) = db::recall_hybrid(
                    &conn,
                    &args.context,
                    &query_emb,
                    args.namespace.as_deref(),
                    args.limit.min(50),
                    args.tags.as_deref(),
                    args.since.as_deref(),
                    args.until.as_deref(),
                    vector_index.as_ref(),
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                    args.as_agent.as_deref(),
                    args.budget_tokens,
                    &resolved_scoring,
                )?;
                if let Some(ref ce) = reranker {
                    (
                        ce.rerank(&args.context, results),
                        tokens_used,
                        "hybrid+rerank",
                    )
                } else {
                    (results, tokens_used, "hybrid")
                }
            }
            Err(e) => {
                eprintln!("ai-memory: embedding query failed: {e}, falling back to keyword");
                let (results, tokens_used) = db::recall(
                    &conn,
                    &args.context,
                    args.namespace.as_deref(),
                    args.limit,
                    args.tags.as_deref(),
                    args.since.as_deref(),
                    args.until.as_deref(),
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                    args.as_agent.as_deref(),
                    args.budget_tokens,
                )?;
                (results, tokens_used, "keyword")
            }
        }
    } else {
        let (results, tokens_used) = db::recall(
            &conn,
            &args.context,
            args.namespace.as_deref(),
            args.limit,
            args.tags.as_deref(),
            args.since.as_deref(),
            args.until.as_deref(),
            resolved_ttl.short_extend_secs,
            resolved_ttl.mid_extend_secs,
            args.as_agent.as_deref(),
            args.budget_tokens,
        )?;
        (results, tokens_used, "keyword")
    };

    if json_out {
        let scored: Vec<serde_json::Value> = results
            .iter()
            .map(|(m, s)| {
                let mut v = serde_json::to_value(m).unwrap_or_default();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "score".to_string(),
                        serde_json::json!((s * 1000.0).round() / 1000.0),
                    );
                }
                v
            })
            .collect();
        let mut body = serde_json::json!({
            "memories": scored,
            "count": results.len(),
            "mode": mode,
            "tokens_used": tokens_used,
        });
        if let Some(b) = args.budget_tokens {
            body["budget_tokens"] = serde_json::json!(b);
        }
        println!("{}", serde_json::to_string(&body)?);
        return Ok(());
    }
    if results.is_empty() {
        eprintln!("no memories found for: {}", args.context);
        return Ok(());
    }
    for (mem, score) in &results {
        let age = human_age(&mem.updated_at);
        let config = if mem.confidence < 1.0 {
            format!(" conf={:.0}%", mem.confidence * 100.0)
        } else {
            String::new()
        };
        println!(
            "[{}] {} {} score={:.2} (ns={}, {}x, {}{})",
            color::tier_color(
                mem.tier.as_str(),
                &format!("{}/{}", mem.tier, id_short(&mem.id))
            ),
            color::bold(&mem.title),
            color::priority_bar(mem.priority),
            score,
            color::cyan(&mem.namespace),
            mem.access_count,
            color::dim(&age),
            config
        );
        let preview: String = mem.content.chars().take(200).collect();
        println!("  {}\n", color::dim(&preview));
    }
    println!("{} memory(ies) recalled [{}]", results.len(), mode);
    Ok(())
}

fn cmd_search(
    db_path: &Path,
    args: &SearchArgs,
    json_out: bool,
    app_config: &config::AppConfig,
) -> Result<()> {
    // #197: validate agent_id filter values
    if let Some(ref aid) = args.agent_id {
        validate::validate_agent_id(aid)?;
    }
    // #151: validate --as-agent namespace
    if let Some(ref a) = args.as_agent {
        validate::validate_namespace(a)?;
    }
    let conn = db::open(db_path)?;
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::search(
        &conn,
        &args.query,
        args.namespace.as_deref(),
        tier.as_ref(),
        args.limit,
        None,
        args.since.as_deref(),
        args.until.as_deref(),
        args.tags.as_deref(),
        args.agent_id.as_deref(),
        args.as_agent.as_deref(),
    )?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"results": results, "count": results.len()})
            )?
        );
        return Ok(());
    }
    if results.is_empty() {
        eprintln!("no results for: {}", args.query);
        return Ok(());
    }
    for mem in &results {
        let age = human_age(&mem.updated_at);
        println!(
            "[{}/{}] {} (p={}, ns={}, {})",
            mem.tier,
            id_short(&mem.id),
            mem.title,
            mem.priority,
            mem.namespace,
            age
        );
    }
    println!("\n{} result(s)", results.len());
    Ok(())
}

fn cmd_get(db_path: &Path, args: &GetArgs, json_out: bool) -> Result<()> {
    validate::validate_id(&args.id)?;
    let conn = db::open(db_path)?;
    if let Some(mem) = db::resolve_id(&conn, &args.id)? {
        let links = db::get_links(&conn, &mem.id).unwrap_or_default();
        if json_out {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({"memory": mem, "links": links}))?
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&mem)?);
            if !links.is_empty() {
                println!("\nlinks:");
                for l in &links {
                    println!("  {} --[{}]--> {}", l.source_id, l.relation, l.target_id);
                }
            }
        }
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_list(
    db_path: &Path,
    args: &ListArgs,
    json_out: bool,
    app_config: &config::AppConfig,
) -> Result<()> {
    // #197: validate agent_id filter values
    if let Some(ref aid) = args.agent_id {
        validate::validate_agent_id(aid)?;
    }
    let conn = db::open(db_path)?;
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::list(
        &conn,
        args.namespace.as_deref(),
        tier.as_ref(),
        args.limit,
        args.offset,
        None,
        args.since.as_deref(),
        args.until.as_deref(),
        args.tags.as_deref(),
        args.agent_id.as_deref(),
    )?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"memories": results, "count": results.len()})
            )?
        );
        return Ok(());
    }
    if results.is_empty() {
        eprintln!("no memories stored");
        return Ok(());
    }
    for mem in &results {
        let age = human_age(&mem.updated_at);
        println!(
            "[{}/{}] {} (p={}, ns={}, {})",
            mem.tier,
            id_short(&mem.id),
            mem.title,
            mem.priority,
            mem.namespace,
            age
        );
    }
    println!("\n{} memory(ies)", results.len());
    Ok(())
}

fn cmd_delete(
    db_path: &Path,
    args: &DeleteArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    validate::validate_id(&args.id)?;
    let conn = db::open(db_path)?;
    // Resolve the target first for governance owner context.
    let target = db::resolve_id(&conn, &args.id)?;
    let Some(target) = target else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    };

    // Task 1.9: governance enforcement (delete-side)
    {
        use models::{GovernanceDecision, GovernedAction};
        let caller_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = serde_json::json!({"id": target.id, "title": target.title});
        match db::enforce_governance(
            &conn,
            GovernedAction::Delete,
            &target.namespace,
            &caller_agent_id,
            Some(&target.id),
            mem_owner.as_deref(),
            &payload,
        )? {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                eprintln!("delete denied by governance: {reason}");
                std::process::exit(1);
            }
            GovernanceDecision::Pending(pending_id) => {
                if json_out {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "delete",
                            "memory_id": target.id,
                        })
                    );
                } else {
                    println!(
                        "delete queued for approval: pending_id={pending_id} id={}",
                        target.id
                    );
                }
                return Ok(());
            }
        }
    }

    if db::delete(&conn, &target.id)? {
        if json_out {
            println!("{}", serde_json::json!({"deleted": true, "id": target.id}));
        } else {
            println!("deleted: {}", target.id);
        }
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn cmd_promote(
    db_path: &Path,
    args: &PromoteArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    validate::validate_id(&args.id)?;
    if let Some(ref to_ns) = args.to_namespace {
        validate::validate_namespace(to_ns)?;
    }
    let conn = db::open(db_path)?;
    // Resolve target; capture the memory for governance owner context.
    let target = if let Some(m) = db::get(&conn, &args.id)? {
        m
    } else if let Some(m) = db::get_by_prefix(&conn, &args.id)? {
        m
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    };
    let resolved_id = target.id.clone();

    // Task 1.9: governance enforcement (promote-side)
    {
        use models::{GovernanceDecision, GovernedAction};
        let caller_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = serde_json::json!({
            "id": resolved_id,
            "to_namespace": args.to_namespace,
        });
        match db::enforce_governance(
            &conn,
            GovernedAction::Promote,
            &target.namespace,
            &caller_agent_id,
            Some(&resolved_id),
            mem_owner.as_deref(),
            &payload,
        )? {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                eprintln!("promote denied by governance: {reason}");
                std::process::exit(1);
            }
            GovernanceDecision::Pending(pending_id) => {
                if json_out {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "promote",
                            "memory_id": resolved_id,
                        })
                    );
                } else {
                    println!(
                        "promote queued for approval: pending_id={pending_id} id={resolved_id}"
                    );
                }
                return Ok(());
            }
        }
    }

    // Task 1.7: vertical (namespace) promotion when --to-namespace is set
    if let Some(ref to_ns) = args.to_namespace {
        let clone_id = db::promote_to_namespace(&conn, &resolved_id, to_ns)?;
        if json_out {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "promoted": true,
                    "mode": "vertical",
                    "source_id": resolved_id,
                    "clone_id": clone_id,
                    "to_namespace": to_ns,
                }))?
            );
        } else {
            println!(
                "promoted (vertical): {} → {} (clone: {})",
                id_short(&resolved_id),
                to_ns,
                id_short(&clone_id),
            );
        }
        return Ok(());
    }

    let (found, _) = db::update(
        &conn,
        &resolved_id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        Some(""),
        None,
    )?;
    if !found {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    if json_out {
        println!(
            "{}",
            serde_json::json!({"promoted": true, "id": resolved_id, "tier": "long"})
        );
    } else {
        println!("promoted to long-term: {resolved_id}");
    }
    Ok(())
}

fn cmd_forget(db_path: &Path, args: &ForgetArgs, json_out: bool) -> Result<()> {
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let conn = db::open(db_path)?;
    match db::forget(
        &conn,
        args.namespace.as_deref(),
        args.pattern.as_deref(),
        tier.as_ref(),
        true, // always archive from CLI
    ) {
        Ok(n) => {
            if json_out {
                println!("{}", serde_json::json!({"deleted": n}));
            } else {
                println!("forgot {n} memories");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn cmd_link(db_path: &Path, args: &LinkArgs, json_out: bool) -> Result<()> {
    validate::validate_link(&args.source_id, &args.target_id, &args.relation)?;
    let conn = db::open(db_path)?;
    db::create_link(&conn, &args.source_id, &args.target_id, &args.relation)?;
    if json_out {
        println!("{}", serde_json::json!({"linked": true}));
    } else {
        println!(
            "linked: {} --[{}]--> {}",
            args.source_id, args.relation, args.target_id
        );
    }
    Ok(())
}

fn cmd_consolidate(
    db_path: &Path,
    args: ConsolidateArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    let ids: Vec<String> = args
        .ids
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let namespace = args.namespace.unwrap_or_else(auto_namespace);
    validate::validate_consolidate(&ids, &args.title, &args.summary, &namespace)?;
    let conn = db::open(db_path)?;
    let consolidator_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
    let new_id = db::consolidate(
        &conn,
        &ids,
        &args.title,
        &args.summary,
        &namespace,
        &Tier::Long,
        "cli",
        &consolidator_agent_id,
    )?;
    if json_out {
        println!(
            "{}",
            serde_json::json!({"id": new_id, "consolidated": ids.len()})
        );
    } else {
        println!("consolidated {} memories into: {}", ids.len(), new_id);
    }
    Ok(())
}

fn cmd_gc(db_path: &Path, json_out: bool, app_config: &config::AppConfig) -> Result<()> {
    let conn = db::open(db_path)?;
    let count = db::gc(&conn, app_config.effective_archive_on_gc())?;
    if json_out {
        println!("{}", serde_json::json!({"expired_deleted": count}));
    } else {
        println!("expired memories deleted: {count}");
    }
    Ok(())
}

fn cmd_stats(db_path: &Path, json_out: bool) -> Result<()> {
    let conn = db::open(db_path)?;
    let stats = db::stats(&conn, db_path)?;
    if json_out {
        println!("{}", serde_json::to_string(&stats)?);
        return Ok(());
    }
    println!("total memories: {}", stats.total);
    println!("expiring within 1h: {}", stats.expiring_soon);
    println!("links: {}", stats.links_count);
    println!("database size: {} bytes", stats.db_size_bytes);
    println!("\nby tier:");
    for t in &stats.by_tier {
        println!("  {}: {}", t.tier, t.count);
    }
    println!("\nby namespace:");
    for ns in &stats.by_namespace {
        println!("  {}: {}", ns.namespace, ns.count);
    }
    Ok(())
}

fn cmd_namespaces(db_path: &Path, json_out: bool) -> Result<()> {
    let conn = db::open(db_path)?;
    let ns = db::list_namespaces(&conn)?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"namespaces": ns}))?
        );
        return Ok(());
    }
    if ns.is_empty() {
        eprintln!("no namespaces");
    } else {
        for n in &ns {
            println!("  {}: {} memories", n.namespace, n.count);
        }
    }
    Ok(())
}

fn cmd_export(db_path: &Path) -> Result<()> {
    let conn = db::open(db_path)?;
    let memories = db::export_all(&conn)?;
    let links = db::export_links(&conn)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "memories": memories, "links": links, "count": memories.len(),
            "exported_at": Utc::now().to_rfc3339(),
        }))?
    );
    Ok(())
}

fn cmd_import(
    db_path: &Path,
    args: &ImportArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let data: serde_json::Value = serde_json::from_str(&buf)?;
    let memories: Vec<models::Memory> =
        serde_json::from_value(data.get("memories").cloned().unwrap_or_default())?;
    let links: Vec<models::MemoryLink> =
        serde_json::from_value(data.get("links").cloned().unwrap_or_default()).unwrap_or_default();

    // NHI: by default restamp metadata.agent_id with the caller's id so an
    // attacker-crafted JSON file cannot forge provenance. Pass --trust-source
    // to preserve the imported agent_id (use only for trusted backups).
    let caller_id = identity::resolve_agent_id(cli_agent_id, None)?;

    let conn = db::open(db_path)?;
    let mut imported = 0usize;
    let mut restamped = 0usize;
    let mut errors = Vec::new();
    for mut mem in memories {
        if !args.trust_source {
            let original = mem
                .metadata
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
            if let Some(obj) = mem.metadata.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String(caller_id.clone()),
                );
                if let Some(orig) = original.as_ref()
                    && orig.as_str() != caller_id
                {
                    // Preserve the original claim for forensic purposes but not as authoritative id.
                    obj.insert(
                        "imported_from_agent_id".to_string(),
                        serde_json::Value::String(orig.clone()),
                    );
                    restamped += 1;
                }
            }
        }
        if let Err(e) = validate::validate_memory(&mem) {
            errors.push(format!("{}: {}", mem.id, e));
            continue;
        }
        match db::insert(&conn, &mem) {
            Ok(_) => imported += 1,
            Err(e) => errors.push(format!("{}: {}", mem.id, e)),
        }
    }
    for link in links {
        if validate::validate_link(&link.source_id, &link.target_id, &link.relation).is_err() {
            continue;
        }
        let _ = db::create_link(&conn, &link.source_id, &link.target_id, &link.relation);
    }
    if json_out {
        println!(
            "{}",
            serde_json::json!({
                "imported": imported,
                "restamped": restamped,
                "trusted_source": args.trust_source,
                "errors": errors
            })
        );
    } else {
        println!("imported: {imported} (restamped agent_id on {restamped})");
        if args.trust_source {
            eprintln!("warning: --trust-source: agent_id from imported JSON was preserved as-is");
        }
        if !errors.is_empty() {
            for e in &errors {
                eprintln!("  {e}");
            }
        }
    }
    Ok(())
}

fn cmd_resolve(db_path: &Path, args: &ResolveArgs, json_out: bool) -> Result<()> {
    let conn = db::open(db_path)?;
    validate::validate_link(&args.winner_id, &args.loser_id, "supersedes")?;
    db::create_link(&conn, &args.winner_id, &args.loser_id, "supersedes")?;
    let _ = db::update(
        &conn,
        &args.loser_id,
        None,
        None,
        None,
        None,
        None,
        Some(1),
        Some(0.1),
        None,
        None,
    )?;
    db::touch(
        &conn,
        &args.winner_id,
        models::SHORT_TTL_EXTEND_SECS,
        models::MID_TTL_EXTEND_SECS,
    )?;
    if json_out {
        println!(
            "{}",
            serde_json::json!({"resolved": true, "winner": args.winner_id, "loser": args.loser_id})
        );
    } else {
        println!(
            "resolved: {} supersedes {}",
            color::long(&args.winner_id),
            color::dim(&args.loser_id)
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn cmd_shell(db_path: &Path) -> Result<()> {
    let conn = db::open(db_path)?;
    println!(
        "{}",
        color::bold("ai-memory shell — type 'help' for commands, 'quit' to exit")
    );
    let stdin = std::io::stdin();
    loop {
        eprint!("{} ", color::cyan("memory>"));
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        match parts[0] {
            "quit" | "exit" | "q" => break,
            "help" | "h" => {
                println!("  recall <context>    — fuzzy recall");
                println!("  search <query>      — keyword search");
                println!("  list [namespace]    — list memories");
                println!("  get <id>            — show memory details");
                println!("  stats               — show statistics");
                println!("  namespaces          — list namespaces");
                println!("  delete <id>         — delete a memory");
                println!("  quit                — exit shell");
            }
            "recall" | "r" => {
                let ctx = parts[1..].join(" ");
                if ctx.is_empty() {
                    eprintln!("usage: recall <context>");
                    continue;
                }
                match db::recall(
                    &conn,
                    &ctx,
                    None,
                    10,
                    None,
                    None,
                    None,
                    models::SHORT_TTL_EXTEND_SECS,
                    models::MID_TTL_EXTEND_SECS,
                    None,
                    None,
                ) {
                    Ok((results, _tokens_used)) => {
                        for (mem, score) in &results {
                            println!(
                                "  [{}] {} {} score={:.2}",
                                color::tier_color(mem.tier.as_str(), mem.tier.as_str()),
                                color::bold(&mem.title),
                                color::priority_bar(mem.priority),
                                score
                            );
                            let preview: String = mem.content.chars().take(100).collect();
                            println!("    {}", color::dim(&preview));
                        }
                        println!("  {} result(s)", results.len());
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            "search" | "s" => {
                let q = parts[1..].join(" ");
                if q.is_empty() {
                    eprintln!("usage: search <query>");
                    continue;
                }
                match db::search(
                    &conn, &q, None, None, 20, None, None, None, None, None, None,
                ) {
                    Ok(results) => {
                        for mem in &results {
                            println!(
                                "  [{}] {} (p={})",
                                color::tier_color(mem.tier.as_str(), mem.tier.as_str()),
                                mem.title,
                                mem.priority
                            );
                        }
                        println!("  {} result(s)", results.len());
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            "list" | "ls" => {
                let ns = parts.get(1).copied();
                match db::list(&conn, ns, None, 20, 0, None, None, None, None, None) {
                    Ok(results) => {
                        for mem in &results {
                            let age = human_age(&mem.updated_at);
                            println!(
                                "  [{}] {} (ns={}, {})",
                                color::tier_color(mem.tier.as_str(), mem.tier.as_str()),
                                mem.title,
                                mem.namespace,
                                color::dim(&age)
                            );
                        }
                        println!("  {} memory(ies)", results.len());
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            "get" => {
                let id = parts.get(1).unwrap_or(&"");
                if id.is_empty() {
                    eprintln!("usage: get <id>");
                    continue;
                }
                if let Err(e) = validate::validate_id(id) {
                    eprintln!("invalid id: {e}");
                    continue;
                }
                match db::get(&conn, id) {
                    Ok(Some(mem)) => {
                        println!("{}", serde_json::to_string_pretty(&mem).unwrap_or_default());
                    }
                    Ok(None) => eprintln!("not found"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            "stats" => match db::stats(&conn, db_path) {
                Ok(s) => {
                    println!("  total: {}, links: {}", s.total, s.links_count);
                    for t in &s.by_tier {
                        println!("    {}: {}", color::tier_color(&t.tier, &t.tier), t.count);
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            },
            "namespaces" | "ns" => match db::list_namespaces(&conn) {
                Ok(ns) => {
                    for n in &ns {
                        println!("  {}: {}", color::cyan(&n.namespace), n.count);
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            },
            "delete" | "del" | "rm" => {
                let id = parts.get(1).unwrap_or(&"");
                if id.is_empty() {
                    eprintln!("usage: delete <id>");
                    continue;
                }
                if let Err(e) = validate::validate_id(id) {
                    eprintln!("invalid id: {e}");
                    continue;
                }
                match db::delete(&conn, id) {
                    Ok(true) => println!("  deleted"),
                    Ok(false) => eprintln!("  not found"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            _ => eprintln!("unknown command: {}. Type 'help' for commands.", parts[0]),
        }
    }
    println!("goodbye");
    Ok(())
}

/// NHI: restamp `metadata.agent_id` to the caller's id, preserving the original
/// as `imported_from_agent_id` for forensics. Used by `import` and `sync` paths
/// to prevent attacker-supplied JSON/DB from forging provenance.
fn restamp_agent_id(mem: &mut models::Memory, caller_id: &str) {
    let original = mem
        .metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    if let Some(obj) = mem.metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(caller_id.to_string()),
        );
        if let Some(orig) = original
            && orig != caller_id
        {
            obj.insert(
                "imported_from_agent_id".to_string(),
                serde_json::Value::String(orig),
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
/// Phase 3 foundation (issue #224): preview counters for `sync --dry-run`.
/// Classified against today's timestamp-aware merge semantics. Future work
/// (Task 3a.1 CRDT-lite) will replace this with field-level diagnostics.
#[allow(clippy::struct_field_names)] // naming mirrors the JSON response keys
#[derive(Default)]
struct SyncPreview {
    would_pull_new: usize,
    would_pull_update: usize,
    would_pull_noop: usize,
    would_push_new: usize,
    would_push_update: usize,
    would_push_noop: usize,
    would_pull_links: usize,
    would_push_links: usize,
}

impl SyncPreview {
    fn classify(local: Option<&models::Memory>, remote: &models::Memory) -> MergeOutcome {
        match local {
            None => MergeOutcome::New,
            Some(existing) => {
                if remote.updated_at > existing.updated_at {
                    MergeOutcome::Update
                } else {
                    MergeOutcome::Noop
                }
            }
        }
    }
}

enum MergeOutcome {
    New,
    Update,
    Noop,
}

#[allow(clippy::too_many_lines)] // pull/push/merge variants kept inline for locality
fn cmd_sync(
    db_path: &Path,
    args: &SyncArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    let local_conn = db::open(db_path)?;
    let remote_conn = db::open(&args.remote_db)?;
    // NHI: unless the caller opts into --trust-source, restamp any incoming
    // memories with the caller's id so an attacker-controlled remote DB can't
    // inject arbitrary agent_ids into the local store (and vice versa on push).
    let caller_id = identity::resolve_agent_id(cli_agent_id, None)?;

    if args.dry_run {
        return cmd_sync_dry_run(&local_conn, &remote_conn, &args.direction, json_out);
    }

    match args.direction.as_str() {
        "pull" => {
            let mems = db::export_all(&remote_conn)?;
            let links = db::export_links(&remote_conn)?;
            let mut n = 0;
            for mem in &mems {
                let mut owned = mem.clone();
                if !args.trust_source {
                    restamp_agent_id(&mut owned, &caller_id);
                }
                if let Err(e) = validate::validate_memory(&owned) {
                    tracing::warn!("sync: skipping invalid memory {}: {}", owned.id, e);
                    continue;
                }
                if db::insert(&local_conn, &owned).is_ok() {
                    n += 1;
                }
            }
            for link in &links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &local_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({"direction": "pull", "imported": n})
                );
            } else {
                println!("pulled {n} memories from remote");
            }
        }
        "push" => {
            let mems = db::export_all(&local_conn)?;
            let links = db::export_links(&local_conn)?;
            let mut n = 0;
            for mem in &mems {
                if let Err(e) = validate::validate_memory(mem) {
                    tracing::warn!("sync: skipping invalid memory {}: {}", mem.id, e);
                    continue;
                }
                if db::insert(&remote_conn, mem).is_ok() {
                    n += 1;
                }
            }
            for link in &links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &remote_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({"direction": "push", "exported": n})
                );
            } else {
                println!("pushed {n} memories to remote");
            }
        }
        "merge" => {
            let r_mems = db::export_all(&remote_conn)?;
            let r_links = db::export_links(&remote_conn)?;
            let l_mems = db::export_all(&local_conn)?;
            let l_links = db::export_links(&local_conn)?;
            let (mut pulled, mut pushed) = (0, 0);
            // Use timestamp-aware insert so newer version wins on conflict.
            // NHI: restamp incoming remote memories with caller's agent_id
            // (unless --trust-source) to prevent forged provenance via merge.
            for mem in &r_mems {
                let mut owned = mem.clone();
                if !args.trust_source {
                    restamp_agent_id(&mut owned, &caller_id);
                }
                if validate::validate_memory(&owned).is_err() {
                    continue;
                }
                if db::insert_if_newer(&local_conn, &owned).is_ok() {
                    pulled += 1;
                }
            }
            for link in &r_links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &local_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            for mem in &l_mems {
                if validate::validate_memory(mem).is_err() {
                    continue;
                }
                if db::insert_if_newer(&remote_conn, mem).is_ok() {
                    pushed += 1;
                }
            }
            for link in &l_links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &remote_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({"direction": "merge", "pulled": pulled, "pushed": pushed})
                );
            } else {
                println!("merged: pulled {pulled}, pushed {pushed}");
            }
        }
        _ => anyhow::bail!(
            "invalid direction: {} (use pull, push, merge)",
            args.direction
        ),
    }
    Ok(())
}

/// Phase 3 foundation (issue #224) — `sync --dry-run` implementation.
///
/// Classifies what WOULD happen without writing anything. Uses today's
/// timestamp-aware merge rules (`updated_at > existing.updated_at → update`,
/// otherwise no-op). The richer field-level CRDT preview lands with
/// #224 Task 3a.1.
fn cmd_sync_dry_run(
    local_conn: &rusqlite::Connection,
    remote_conn: &rusqlite::Connection,
    direction: &str,
    json_out: bool,
) -> Result<()> {
    let l_mems = db::export_all(local_conn)?;
    let r_mems = db::export_all(remote_conn)?;
    let l_links = db::export_links(local_conn)?;
    let r_links = db::export_links(remote_conn)?;

    let local_by_id: std::collections::HashMap<&str, &models::Memory> =
        l_mems.iter().map(|m| (m.id.as_str(), m)).collect();
    let remote_by_id: std::collections::HashMap<&str, &models::Memory> =
        r_mems.iter().map(|m| (m.id.as_str(), m)).collect();

    let mut preview = SyncPreview::default();

    let classify_pull = direction != "push";
    let classify_push = direction != "pull";

    if classify_pull {
        for mem in &r_mems {
            match SyncPreview::classify(local_by_id.get(mem.id.as_str()).copied(), mem) {
                MergeOutcome::New => preview.would_pull_new += 1,
                MergeOutcome::Update => preview.would_pull_update += 1,
                MergeOutcome::Noop => preview.would_pull_noop += 1,
            }
        }
        preview.would_pull_links = r_links.len();
    }

    if classify_push {
        for mem in &l_mems {
            match SyncPreview::classify(remote_by_id.get(mem.id.as_str()).copied(), mem) {
                MergeOutcome::New => preview.would_push_new += 1,
                MergeOutcome::Update => preview.would_push_update += 1,
                MergeOutcome::Noop => preview.would_push_noop += 1,
            }
        }
        preview.would_push_links = l_links.len();
    }

    if json_out {
        println!(
            "{}",
            serde_json::json!({
                "dry_run": true,
                "direction": direction,
                "pull": {
                    "new": preview.would_pull_new,
                    "update": preview.would_pull_update,
                    "noop": preview.would_pull_noop,
                    "links": preview.would_pull_links,
                },
                "push": {
                    "new": preview.would_push_new,
                    "update": preview.would_push_update,
                    "noop": preview.would_push_noop,
                    "links": preview.would_push_links,
                }
            })
        );
    } else {
        println!("DRY RUN — no changes written. Direction: {direction}");
        if classify_pull {
            println!(
                "  pull: {} new, {} update, {} noop, {} links",
                preview.would_pull_new,
                preview.would_pull_update,
                preview.would_pull_noop,
                preview.would_pull_links
            );
        }
        if classify_push {
            println!(
                "  push: {} new, {} update, {} noop, {} links",
                preview.would_push_new,
                preview.would_push_update,
                preview.would_push_noop,
                preview.would_push_links
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 3 Task 3b.1 (issue #224) — auto background sync daemon.
//
// Continuous peer-to-peer knowledge mesh. Two laptops running
// `ai-memory sync-daemon --peers <other>` form a live memory exchange:
// - every `interval` seconds, pull each peer's memories newer than the
//   last-seen watermark (GET /api/v1/sync/since)
// - push local memories newer than the last-pushed watermark
//   (POST /api/v1/sync/push)
// - advance sync_state watermarks atomically
//
// Zero cloud. Zero login. Zero SaaS. This is the capability that takes
// ai-memory from "persistent memory store" to "distributed fleet brain."
// ---------------------------------------------------------------------------

async fn cmd_sync_daemon(
    db_path: &Path,
    args: SyncDaemonArgs,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    if args.peers.is_empty() {
        anyhow::bail!("at least one --peers URL is required");
    }
    let interval = args.interval.max(1);
    let batch_size = args.batch_size.max(1);
    let local_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;

    // Tracing subscriber — same config as `serve`. Only init once; if we
    // were launched after another tokio command already initialised it,
    // the second init is a harmless no-op.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("ai_memory=info".parse()?)
                .add_directive("tower_http=info".parse()?),
        )
        .try_init();

    // Layer 2: if client cert is configured, build a rustls ClientConfig
    // with client auth and hand it to reqwest via `use_preconfigured_tls`.
    // reqwest's `from_pkcs8_pem` Identity is native-tls-only; we stay on
    // rustls to keep a single TLS stack across the binary.
    //
    // Self-signed peer certs are common in the local-mesh story. The
    // ClientConfig installs a dangerous "accept any server cert"
    // verifier when mTLS is active — the peer's authentication of US
    // (via our client cert fingerprint in their --mtls-allowlist) is
    // the trust anchor, so fingerprint pinning of the peer's server
    // cert is a Layer 2b refinement tracked in #224.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Ultrareview #336: --insecure-skip-server-verify must be gated
    // behind a compensating control. When server-cert verification is
    // disabled, require the daemon to present a client cert so at
    // least the peer authenticates US via its mTLS allowlist. Without
    // either side of the handshake verified, the connection is an
    // open MITM surface.
    if args.insecure_skip_server_verify && (args.client_cert.is_none() || args.client_key.is_none())
    {
        anyhow::bail!(
            "sync-daemon: --insecure-skip-server-verify requires both --client-cert \
             and --client-key as a compensating mTLS control. Running with neither side \
             of the TLS handshake verified is an open MITM surface and is refused."
        );
    }

    let client = if let (Some(cert_path), Some(key_path)) = (&args.client_cert, &args.client_key) {
        // mTLS path — daemon presents client cert; the peer's
        // FingerprintAllowlistVerifier authenticates us. Server-cert
        // pinning on this side is Layer 2b (post-v0.6.0).
        let rustls_config = build_rustls_client_config(cert_path, key_path).await?;
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .use_preconfigured_tls(rustls_config);
        if args.insecure_skip_server_verify {
            tracing::warn!(
                "sync-daemon: --insecure-skip-server-verify set with --client-cert — \
                 peer server certificates will NOT be validated; peer authenticates us \
                 via mTLS allowlist (compensating control). Do NOT use in production."
            );
            builder = builder.danger_accept_invalid_certs(true);
        }
        builder.build()?
    } else {
        // No client cert — server cert verification is the only
        // remaining trust anchor. Default to system trust roots
        // (the secure path). --insecure-skip-server-verify without
        // mTLS was refused above.
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?
    };

    tracing::info!(
        "sync-daemon: local_agent_id={local_agent_id} peers={peers:?} interval={interval}s",
        peers = args.peers
    );

    // Graceful shutdown: ctrl_c wakes up the loop.
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    let db_path_owned: Arc<Path> = Arc::from(db_path);
    let local_agent_id_arc: Arc<str> = Arc::from(local_agent_id.as_str());
    let api_key_arc: Option<Arc<str>> = args.api_key.as_deref().map(Arc::from);
    let peers_arc: Vec<Arc<str>> = args.peers.iter().map(|s| Arc::from(s.as_str())).collect();
    loop {
        // v0.6.0: reconcile peers in parallel. A slow or unreachable
        // peer used to block every other peer behind it for the full
        // cycle — now each peer runs on its own task and a single
        // stuck peer only delays the deadline for the group, not the
        // other peers' work. With N peers this cuts full-cycle latency
        // from N×per-peer to max(per-peer).
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for peer_url in &peers_arc {
            let client = client.clone();
            let db_path = db_path_owned.clone();
            let local_agent_id = local_agent_id_arc.clone();
            let peer_url = peer_url.clone();
            let api_key = api_key_arc.clone();
            set.spawn(async move {
                if let Err(e) = sync_cycle_once(
                    &client,
                    &db_path,
                    &local_agent_id,
                    &peer_url,
                    api_key.as_deref(),
                    batch_size,
                )
                .await
                {
                    tracing::warn!("sync-daemon: peer {peer_url} cycle failed: {e}");
                }
            });
        }
        while set.join_next().await.is_some() {}

        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
            _ = &mut shutdown => {
                tracing::info!("sync-daemon: shutdown signal received");
                return Ok(());
            }
        }
    }
}

/// One pull+push cycle against a single peer. Writes local db updates
/// synchronously via a fresh connection (avoid holding open connections
/// across await points). Any per-cycle failure is logged and the caller
/// moves to the next peer — we never crash the daemon on a transient
/// network error.
async fn sync_cycle_once(
    client: &reqwest::Client,
    db_path: &Path,
    local_agent_id: &str,
    peer_url: &str,
    api_key: Option<&str>,
    batch_size: usize,
) -> Result<()> {
    let peer_url = peer_url.trim_end_matches('/');

    // --- PULL --------------------------------------------------------
    let since = {
        let conn = db::open(db_path)?;
        db::sync_state_load(&conn, local_agent_id)?
            .entries
            .get(peer_url)
            .cloned()
    };

    let mut pull_url = format!(
        "{peer_url}/api/v1/sync/since?limit={batch_size}&peer={}",
        urlencoding_minimal(local_agent_id)
    );
    if let Some(ref s) = since {
        pull_url.push_str("&since=");
        pull_url.push_str(&urlencoding_minimal(s));
    }

    let mut req = client.get(&pull_url).header("x-agent-id", local_agent_id);
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("sync-daemon: pull status {}", resp.status());
    }
    let pulled: SyncSinceResponse = resp.json().await?;
    let pull_count = pulled.memories.len();
    let latest_pulled = pulled.memories.last().map(|m| m.updated_at.clone());

    {
        let conn = db::open(db_path)?;
        for mem in &pulled.memories {
            if validate::validate_memory(mem).is_ok() {
                let _ = db::insert_if_newer(&conn, mem);
            }
        }
        if let Some(ref at) = latest_pulled {
            db::sync_state_observe(&conn, local_agent_id, peer_url, at)?;
        }
    }

    // --- PUSH --------------------------------------------------------
    let last_pushed = {
        let conn = db::open(db_path)?;
        db::sync_state_last_pushed(&conn, local_agent_id, peer_url)
    };
    let outgoing = {
        let conn = db::open(db_path)?;
        db::memories_updated_since(&conn, last_pushed.as_deref(), batch_size)?
    };
    let push_count = outgoing.len();
    let latest_pushed = outgoing.last().map(|m| m.updated_at.clone());

    if !outgoing.is_empty() {
        let body = serde_json::json!({
            "sender_agent_id": local_agent_id,
            "sender_clock": { "entries": {} },
            "memories": outgoing,
            "dry_run": false,
        });
        let mut req = client
            .post(format!("{peer_url}/api/v1/sync/push"))
            .header("x-agent-id", local_agent_id)
            .header("content-type", "application/json")
            .json(&body);
        if let Some(key) = api_key {
            req = req.header("x-api-key", key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("sync-daemon: push status {}", resp.status());
        }
        if let Some(at) = latest_pushed {
            let conn = db::open(db_path)?;
            db::sync_state_record_push(&conn, local_agent_id, peer_url, &at)?;
        }
    }

    tracing::info!("sync-daemon: peer={peer_url} pulled={pull_count} pushed={push_count}");
    Ok(())
}

/// Minimal URL-component encoder — only the characters the sync-daemon
/// queries actually emit (RFC3339 timestamps with `:` and `+`, and
/// agent ids with `:`/`@`/`/`). Avoids pulling in a whole URL crate for
/// a dozen callsites.
fn urlencoding_minimal(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[derive(serde::Deserialize)]
struct SyncSinceResponse {
    #[allow(dead_code)]
    count: usize,
    #[allow(dead_code)]
    limit: usize,
    memories: Vec<models::Memory>,
}

#[allow(clippy::too_many_lines)]
fn cmd_auto_consolidate(
    db_path: &Path,
    args: &AutoConsolidateArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    let consolidator_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
    let tier_filter = if args.short_only {
        Some(Tier::Short)
    } else {
        None
    };
    let namespaces = if let Some(ref ns) = args.namespace {
        vec![models::NamespaceCount {
            namespace: ns.clone(),
            count: 0,
        }]
    } else {
        db::list_namespaces(&conn)?
    };

    let mut total = 0;
    let mut groups = Vec::new();

    for ns in &namespaces {
        let memories = db::list(
            &conn,
            Some(&ns.namespace),
            tier_filter.as_ref(),
            200,
            0,
            None,
            None,
            None,
            None,
            None,
        )?;
        if memories.len() < args.min_count {
            continue;
        }

        // Group by all tags (each memory appears in every tag group it belongs to)
        let mut tag_groups: std::collections::HashMap<String, Vec<&models::Memory>> =
            std::collections::HashMap::new();
        for mem in &memories {
            if mem.tags.is_empty() {
                tag_groups
                    .entry("_untagged".to_string())
                    .or_default()
                    .push(mem);
            } else {
                for tag in &mem.tags {
                    tag_groups.entry(tag.clone()).or_default().push(mem);
                }
            }
        }

        let mut consolidated_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (tag, group) in &tag_groups {
            // Skip memories already consolidated in another tag group
            let group: Vec<&&models::Memory> = group
                .iter()
                .filter(|m| !consolidated_ids.contains(&m.id))
                .collect();
            if group.len() < args.min_count {
                continue;
            }
            let ids: Vec<String> = group.iter().map(|m| m.id.clone()).collect();
            if args.dry_run {
                let titles: Vec<&str> = group.iter().map(|m| m.title.as_str()).collect();
                groups.push(serde_json::json!({"namespace": ns.namespace, "tag": tag, "count": group.len(), "titles": titles}));
            } else {
                let title = format!(
                    "Consolidated: {} ({} memories)",
                    if tag == "_untagged" {
                        &ns.namespace
                    } else {
                        tag
                    },
                    group.len()
                );
                let content: String = group
                    .iter()
                    .map(|m| format!("- {}: {}", m.title, &m.content[..m.content.len().min(200)]))
                    .collect::<Vec<_>>()
                    .join("\n");
                db::consolidate(
                    &conn,
                    &ids,
                    &title,
                    &content,
                    &ns.namespace,
                    &Tier::Long,
                    "auto-consolidate",
                    &consolidator_agent_id,
                )?;
                consolidated_ids.extend(ids);
                total += group.len();
            }
        }
    }

    if json_out {
        if args.dry_run {
            println!("{}", serde_json::json!({"dry_run": true, "groups": groups}));
        } else {
            println!("{}", serde_json::json!({"consolidated": total}));
        }
    } else if args.dry_run {
        println!("dry run — would consolidate:");
        for g in &groups {
            println!(
                "  {} [{}]: {} memories",
                g["namespace"], g["tag"], g["count"]
            );
        }
    } else {
        println!("auto-consolidated {total} memories");
    }
    Ok(())
}

fn cmd_archive(db_path: &Path, args: ArchiveArgs, json_out: bool) -> Result<()> {
    let conn = db::open(db_path)?;
    match args.action {
        ArchiveAction::List {
            namespace,
            limit,
            offset,
        } => {
            let items = db::list_archived(&conn, namespace.as_deref(), limit, offset)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({"archived": items, "count": items.len()})
                );
            } else if items.is_empty() {
                println!("no archived memories");
            } else {
                for item in &items {
                    println!(
                        "[{}] {} (archived: {})",
                        id_short(item["id"].as_str().unwrap_or("")),
                        item["title"].as_str().unwrap_or(""),
                        item["archived_at"].as_str().unwrap_or("")
                    );
                }
                println!("{} archived memories", items.len());
            }
        }
        ArchiveAction::Restore { id } => {
            validate::validate_id(&id)?;
            let restored = db::restore_archived(&conn, &id)?;
            if json_out {
                println!("{}", serde_json::json!({"restored": restored, "id": id}));
            } else if restored {
                println!("restored: {}", id_short(&id));
            } else {
                eprintln!("not found in archive: {id}");
                std::process::exit(1);
            }
        }
        ArchiveAction::Purge { older_than_days } => {
            let purged = db::purge_archive(&conn, older_than_days)?;
            if json_out {
                println!("{}", serde_json::json!({"purged": purged}));
            } else {
                println!("purged {purged} archived memories");
            }
        }
        ArchiveAction::Stats => {
            let stats = db::archive_stats(&conn)?;
            if json_out {
                println!("{stats}");
            } else {
                println!("archived: {} total", stats["archived_total"]);
                if let Some(by_ns) = stats["by_namespace"].as_array() {
                    for ns in by_ns {
                        println!(
                            "  {}: {}",
                            ns["namespace"].as_str().unwrap_or(""),
                            ns["count"]
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn cmd_agents(db_path: &Path, args: AgentsArgs, json_out: bool) -> Result<()> {
    let conn = db::open(db_path)?;
    match args.action.unwrap_or(AgentsAction::List) {
        AgentsAction::List => {
            let agents = db::list_agents(&conn)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({"count": agents.len(), "agents": agents})
                );
            } else if agents.is_empty() {
                println!("no registered agents");
            } else {
                for a in &agents {
                    let caps = if a.capabilities.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", a.capabilities.join(","))
                    };
                    println!(
                        "{}  type={}  registered={}  last_seen={}{}",
                        a.agent_id, a.agent_type, a.registered_at, a.last_seen_at, caps
                    );
                }
                println!("{} registered agents", agents.len());
            }
        }
        AgentsAction::Register {
            agent_id,
            agent_type,
            capabilities,
        } => {
            validate::validate_agent_id(&agent_id)?;
            validate::validate_agent_type(&agent_type)?;
            let caps: Vec<String> = capabilities
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            validate::validate_capabilities(&caps)?;
            let id = db::register_agent(&conn, &agent_id, &agent_type, &caps)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({
                        "registered": true,
                        "id": id,
                        "agent_id": agent_id,
                        "agent_type": agent_type,
                        "capabilities": caps,
                    })
                );
            } else {
                println!(
                    "registered {agent_id} (type={agent_type}, capabilities={})",
                    if caps.is_empty() {
                        "-".to_string()
                    } else {
                        caps.join(",")
                    }
                );
            }
        }
    }
    Ok(())
}

fn cmd_pending(
    db_path: &Path,
    args: PendingArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    match args.action {
        PendingAction::List { status, limit } => {
            let items = db::list_pending_actions(&conn, status.as_deref(), limit)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({"count": items.len(), "pending": items})
                );
            } else if items.is_empty() {
                println!("no pending actions");
            } else {
                for item in &items {
                    println!(
                        "[{}] {} ns={} action={} by={} ({})",
                        id_short(&item.id),
                        item.status,
                        item.namespace,
                        item.action_type,
                        item.requested_by,
                        item.requested_at
                    );
                }
                println!("{} pending action(s)", items.len());
            }
        }
        PendingAction::Approve { id } => {
            use db::ApproveOutcome;
            validate::validate_id(&id)?;
            let agent = identity::resolve_agent_id(cli_agent_id, None)?;
            match db::approve_with_approver_type(&conn, &id, &agent)? {
                ApproveOutcome::Approved => {
                    let executed = db::execute_pending_action(&conn, &id)?;
                    if json_out {
                        println!(
                            "{}",
                            serde_json::json!({
                                "approved": true,
                                "id": id,
                                "decided_by": agent,
                                "executed": true,
                                "memory_id": executed,
                            })
                        );
                    } else {
                        println!("approved + executed: {id} (by {agent})");
                    }
                }
                ApproveOutcome::Pending { votes, quorum } => {
                    if json_out {
                        println!(
                            "{}",
                            serde_json::json!({
                                "approved": false,
                                "status": "pending",
                                "id": id,
                                "votes": votes,
                                "quorum": quorum,
                                "reason": "consensus threshold not yet reached",
                            })
                        );
                    } else {
                        println!(
                            "approval recorded: {id} ({votes}/{quorum} consensus, not yet met)"
                        );
                    }
                }
                ApproveOutcome::Rejected(reason) => {
                    eprintln!("approve rejected: {reason}");
                    std::process::exit(1);
                }
            }
        }
        PendingAction::Reject { id } => {
            validate::validate_id(&id)?;
            let agent = identity::resolve_agent_id(cli_agent_id, None)?;
            let ok = db::decide_pending_action(&conn, &id, false, &agent)?;
            if !ok {
                eprintln!("pending action not found or already decided: {id}");
                std::process::exit(1);
            }
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({"rejected": true, "id": id, "decided_by": agent})
                );
            } else {
                println!("rejected: {id} (by {agent})");
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn cmd_mine(
    db_path: &Path,
    args: MineArgs,
    json_out: bool,
    app_config: &config::AppConfig,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    // NHI: the caller (who ran `ai-memory mine`) is the attributable party for
    // every mined memory. Without this, mined memories would be orphaned from
    // all agent_id filters and governance checks.
    let miner_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
    let format = mine::Format::from_str(&args.format).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid format: {} (use claude, chatgpt, slack)",
            args.format
        )
    })?;
    let tier = Tier::from_str(&args.tier)
        .ok_or_else(|| anyhow::anyhow!("invalid tier: {} (use short, mid, long)", args.tier))?;
    let namespace = args.namespace.unwrap_or_else(|| match format {
        mine::Format::Claude => "claude-export".to_string(),
        mine::Format::ChatGpt => "chatgpt-export".to_string(),
        mine::Format::Slack => "slack-export".to_string(),
    });

    let path = std::path::Path::new(&args.path);

    // Parse conversations
    let conversations = match format {
        mine::Format::Claude => mine::parse_claude(path)?,
        mine::Format::ChatGpt => mine::parse_chatgpt(path)?,
        mine::Format::Slack => mine::parse_slack(path)?,
    };

    // Filter by minimum message count
    let filtered: Vec<_> = conversations
        .iter()
        .filter(|c| c.messages.len() >= args.min_messages)
        .collect();

    if args.dry_run {
        if json_out {
            let items: Vec<serde_json::Value> = filtered
                .iter()
                .filter_map(|c| {
                    mine::conversation_to_memory(c, format).map(|m| {
                        serde_json::json!({
                            "title": m.title,
                            "content_length": m.content.len(),
                            "messages": c.messages.len(),
                            "source": m.source_format,
                        })
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "total_conversations": conversations.len(),
                    "filtered": filtered.len(),
                    "would_import": items.len(),
                    "namespace": namespace,
                    "tier": tier.as_str(),
                    "memories": items,
                }))?
            );
        } else {
            println!("Dry run — no memories will be stored\n");
            println!("Total conversations found: {}", conversations.len());
            println!(
                "After filter (>={} messages): {}",
                args.min_messages,
                filtered.len()
            );
            println!("Namespace: {namespace}");
            println!("Tier: {tier}\n");
            for c in &filtered {
                if let Some(m) = mine::conversation_to_memory(c, format) {
                    println!(
                        "  {} ({} msgs, {} bytes)",
                        m.title,
                        c.messages.len(),
                        m.content.len()
                    );
                }
            }
        }
        return Ok(());
    }

    // Store memories
    let conn = db::open(db_path)?;
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());
    let now = Utc::now();

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;

    // Use a transaction for bulk performance
    conn.execute_batch("BEGIN")?;

    for conv in &filtered {
        let Some(mined) = mine::conversation_to_memory(conv, format) else {
            skipped += 1;
            continue;
        };

        let expires_at = app_config
            .effective_ttl()
            .ttl_for_tier(&tier)
            .map(|s| (now + Duration::seconds(s)).to_rfc3339());

        let mut metadata = models::default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "agent_id".to_string(),
                serde_json::Value::String(miner_agent_id.clone()),
            );
            obj.insert(
                "mined_from".to_string(),
                serde_json::Value::String(format.source_tag().to_string()),
            );
        }
        let mem = models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: tier.clone(),
            namespace: namespace.clone(),
            title: mined.title,
            content: mined.content,
            tags: vec![format.source_tag().to_string()],
            priority: 5,
            confidence: 0.8,
            source: mined.source_format,
            access_count: 0,
            created_at: mined.created_at.unwrap_or_else(|| now.to_rfc3339()),
            updated_at: now.to_rfc3339(),
            last_accessed_at: None,
            expires_at,
            metadata,
        };

        match db::insert(&conn, &mem) {
            Ok(_) => imported += 1,
            Err(e) => {
                errors += 1;
                eprintln!("warning: failed to store '{}': {}", mem.title, e);
            }
        }

        // Commit in batches of 100
        if imported.is_multiple_of(100) && imported > 0 {
            conn.execute_batch("COMMIT")?;
            conn.execute_batch("BEGIN")?;
        }
    }

    conn.execute_batch("COMMIT")?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "imported": imported,
                "skipped": skipped,
                "errors": errors,
                "total_conversations": conversations.len(),
                "namespace": namespace,
                "tier": tier.as_str(),
            }))?
        );
    } else {
        println!(
            "Imported {} memories from {} conversations (skipped: {}, errors: {})",
            imported,
            conversations.len(),
            skipped,
            errors
        );
        println!("Namespace: {namespace}, Tier: {tier}");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// v0.6.0.0 — backup / restore
// ---------------------------------------------------------------------------

/// Timestamp format used for snapshot filenames. RFC3339-compatible but
/// filesystem-safe: no colons, no slashes.
const BACKUP_TS_FMT: &str = "%Y-%m-%dT%H%M%SZ";

#[derive(serde::Serialize, serde::Deserialize)]
struct BackupManifest {
    snapshot: String,
    sha256: String,
    bytes: u64,
    source_db: String,
    version: String,
    created_at: String,
}

fn cmd_backup(db_path: &Path, args: &BackupArgs, json_out: bool) -> Result<()> {
    use std::io::Read;
    std::fs::create_dir_all(&args.to)
        .with_context(|| format!("creating backup dir {}", args.to.display()))?;
    // SQLite VACUUM INTO is hot-backup-safe and produces a defragmented
    // file. Equivalent to `sqlite3 source '.backup dest'` in effect but
    // runs in-process via our existing connection.
    let conn = db::open(db_path).context("opening source DB for backup")?;
    let ts = chrono::Utc::now().format(BACKUP_TS_FMT).to_string();
    let snapshot_name = format!("ai-memory-{ts}.db");
    let snapshot_path = args.to.join(&snapshot_name);
    if snapshot_path.exists() {
        anyhow::bail!(
            "refusing to overwrite existing snapshot {}",
            snapshot_path.display()
        );
    }
    conn.execute(
        "VACUUM INTO ?1",
        rusqlite::params![snapshot_path.to_string_lossy()],
    )
    .context("VACUUM INTO failed")?;
    drop(conn);

    let bytes = std::fs::metadata(&snapshot_path)?.len();
    let sha = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        let mut f = std::fs::File::open(&snapshot_path)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        format!("{:x}", hasher.finalize())
    };

    let manifest = BackupManifest {
        snapshot: snapshot_name.clone(),
        sha256: sha.clone(),
        bytes,
        source_db: db_path.to_string_lossy().into_owned(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let manifest_path = args.to.join(format!("ai-memory-{ts}.manifest.json"));
    let manifest_text = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&manifest_path, manifest_text.as_bytes())?;

    // Rotation — newest-first listing, drop everything past `keep`.
    if args.keep > 0 {
        prune_old_snapshots(&args.to, args.keep)?;
    }

    if json_out {
        println!("{}", serde_json::to_string(&manifest)?);
    } else {
        println!("Snapshot: {}", snapshot_path.display());
        println!("Manifest: {}", manifest_path.display());
        println!("SHA-256 : {sha}");
        println!("Bytes   : {bytes}");
    }
    Ok(())
}

/// Enumerate existing `ai-memory-*.db` snapshot files newest-first and
/// delete everything past `keep`. Also deletes the matching manifest
/// for each removed snapshot.
fn prune_old_snapshots(dir: &Path, keep: usize) -> Result<()> {
    let mut snaps: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_owned();
            let is_snapshot = name.starts_with("ai-memory-")
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("db"));
            if is_snapshot {
                let mtime = entry.metadata().ok()?.modified().ok()?;
                Some((mtime, path))
            } else {
                None
            }
        })
        .collect();
    snaps.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (_, path) in snaps.into_iter().skip(keep) {
        let _ = std::fs::remove_file(&path);
        // Matching manifest (same stem, .manifest.json extension pattern)
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let manifest = dir.join(format!("{stem}.manifest.json"));
            let _ = std::fs::remove_file(manifest);
        }
    }
    Ok(())
}

fn cmd_restore(db_path: &Path, args: &RestoreArgs, json_out: bool) -> Result<()> {
    use std::io::Read;
    let (snapshot_path, manifest_path) = if args.from.is_dir() {
        // Pick the newest snapshot in the directory.
        let mut snaps: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&args.from)?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_name()?.to_str()?.to_owned();
                let is_snapshot = name.starts_with("ai-memory-")
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("db"));
                if is_snapshot {
                    let mtime = entry.metadata().ok()?.modified().ok()?;
                    Some((mtime, path))
                } else {
                    None
                }
            })
            .collect();
        snaps.sort_by_key(|b| std::cmp::Reverse(b.0));
        let snap = snaps
            .into_iter()
            .next()
            .map(|(_, p)| p)
            .ok_or_else(|| anyhow::anyhow!("no snapshots found in {}", args.from.display()))?;
        let stem = snap.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let manifest = args.from.join(format!("{stem}.manifest.json"));
        (snap, manifest)
    } else {
        // File path supplied directly.
        let snap = args.from.clone();
        let stem = snap.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let parent = snap.parent().unwrap_or_else(|| Path::new("."));
        let manifest = parent.join(format!("{stem}.manifest.json"));
        (snap, manifest)
    };

    if !snapshot_path.exists() {
        anyhow::bail!("snapshot {} does not exist", snapshot_path.display());
    }

    // SHA-256 verification against manifest.
    if !args.skip_verify {
        if !manifest_path.exists() {
            anyhow::bail!(
                "manifest {} not found; pass --skip-verify to restore anyway",
                manifest_path.display()
            );
        }
        let manifest_text = std::fs::read_to_string(&manifest_path)?;
        let manifest: BackupManifest = serde_json::from_str(&manifest_text)
            .with_context(|| format!("parsing manifest {}", manifest_path.display()))?;
        let observed = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            let mut f = std::fs::File::open(&snapshot_path)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            format!("{:x}", hasher.finalize())
        };
        if observed != manifest.sha256 {
            anyhow::bail!(
                "sha256 mismatch — manifest says {}, snapshot is {}",
                manifest.sha256,
                observed
            );
        }
    }

    // Move current DB aside as a safety net (only if it exists).
    if db_path.exists() {
        let ts = chrono::Utc::now().format(BACKUP_TS_FMT).to_string();
        let aside = db_path.with_extension(format!("pre-restore-{ts}.db"));
        std::fs::rename(db_path, &aside)
            .with_context(|| format!("moving current DB aside to {}", aside.display()))?;
        if !json_out {
            println!("Previous DB moved to {}", aside.display());
        }
    }

    std::fs::copy(&snapshot_path, db_path)
        .with_context(|| format!("copying snapshot to {}", db_path.display()))?;

    if json_out {
        println!(
            "{}",
            serde_json::json!({
                "status": "restored",
                "from": snapshot_path.to_string_lossy(),
                "to": db_path.to_string_lossy(),
            })
        );
    } else {
        println!(
            "Restored {} → {}",
            snapshot_path.display(),
            db_path.display()
        );
    }
    Ok(())
}

async fn cmd_curator(
    db_path: &Path,
    args: &CuratorArgs,
    app_config: &config::AppConfig,
) -> Result<()> {
    if args.rollback.is_some() || args.rollback_last.is_some() {
        return cmd_curator_rollback(db_path, args);
    }

    if !args.once && !args.daemon {
        anyhow::bail!("curator requires --once, --daemon, --rollback <id>, or --rollback-last N");
    }

    let cfg = curator::CuratorConfig {
        interval_secs: args.interval_secs,
        max_ops_per_cycle: args.max_ops,
        dry_run: args.dry_run,
        include_namespaces: args.include_namespaces.clone(),
        exclude_namespaces: args.exclude_namespaces.clone(),
    };

    let feature_tier = app_config.effective_tier(None);
    let llm = build_curator_llm(feature_tier);

    if args.once {
        let conn = db::open(db_path)?;
        let report = curator::run_once(&conn, llm.as_ref(), &cfg)?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_curator_report(&report);
        }
        return Ok(());
    }

    // Daemon mode. Install a tokio ctrl_c watcher that flips the shutdown
    // flag; the daemon loop polls it between cycles so SIGINT / SIGTERM
    // land cleanly on the next wake-up.
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_for_signal = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_for_signal.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let db_owned = db_path.to_path_buf();
    let llm_arc = llm.map(std::sync::Arc::new);
    tokio::task::spawn_blocking(move || {
        curator::run_daemon(db_owned, llm_arc, cfg, shutdown);
    })
    .await
    .map_err(|e| anyhow::anyhow!("curator daemon join: {e}"))?;
    Ok(())
}

fn cmd_curator_rollback(db_path: &Path, args: &CuratorArgs) -> Result<()> {
    let conn = db::open(db_path)?;

    if let Some(id) = &args.rollback {
        let Some(mem) = db::get(&conn, id)? else {
            anyhow::bail!("rollback entry {id} not found");
        };
        let entry: autonomy::RollbackEntry = serde_json::from_str(&mem.content)
            .context("rollback entry content is not a valid RollbackEntry JSON")?;
        let applied = autonomy::reverse_rollback_entry(&conn, &entry)?;
        // Mark the log entry as reversed by appending a tag. We don't
        // delete the log memory — its history is the audit trail.
        let mut tags = mem.tags.clone();
        if !tags.iter().any(|t| t == "_reversed") {
            tags.push("_reversed".to_string());
            db::update(
                &conn,
                &mem.id,
                None,
                None,
                None,
                None,
                Some(&tags),
                None,
                None,
                None,
                None,
            )?;
        }
        println!(
            "rollback {id}: {}",
            if applied { "applied" } else { "no-op" }
        );
        return Ok(());
    }

    if let Some(n) = args.rollback_last {
        let log = db::list(
            &conn,
            Some("_curator/rollback"),
            None,
            n.max(1),
            0,
            None,
            None,
            None,
            None,
            None,
        )?;
        let mut reversed = 0usize;
        for mem in &log {
            if mem.tags.iter().any(|t| t == "_reversed") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<autonomy::RollbackEntry>(&mem.content) else {
                continue;
            };
            let applied = autonomy::reverse_rollback_entry(&conn, &entry)?;
            if applied {
                reversed += 1;
                let mut tags = mem.tags.clone();
                tags.push("_reversed".to_string());
                db::update(
                    &conn,
                    &mem.id,
                    None,
                    None,
                    None,
                    None,
                    Some(&tags),
                    None,
                    None,
                    None,
                    None,
                )?;
            }
        }
        println!("reversed {reversed} rollback entries");
        return Ok(());
    }

    unreachable!("cmd_curator_rollback entered without --rollback or --rollback-last");
}

fn build_curator_llm(tier: config::FeatureTier) -> Option<llm::OllamaClient> {
    // The curator currently shares the default Ollama endpoint with the
    // interactive `auto_tag` / `detect_contradiction` tools. A dedicated
    // model override lives on `config.curator.model` in the v0.7 track.
    let llm_model = tier.config().llm_model?;
    let model = llm_model.ollama_model_id().to_string();
    llm::OllamaClient::new(&model).ok()
}

fn print_curator_report(r: &curator::CuratorReport) {
    println!("curator cycle report");
    println!("  started_at:        {}", r.started_at);
    println!("  completed_at:      {}", r.completed_at);
    println!("  duration_ms:       {}", r.cycle_duration_ms);
    println!("  memories_scanned:  {}", r.memories_scanned);
    println!("  memories_eligible: {}", r.memories_eligible);
    println!("  operations:        {}", r.operations_attempted);
    println!("  auto_tagged:       {}", r.auto_tagged);
    println!("  contradictions:    {}", r.contradictions_found);
    println!("  skipped (cap):     {}", r.operations_skipped_cap);
    println!("  errors:            {}", r.errors.len());
    println!("  dry_run:           {}", r.dry_run);
    for e in &r.errors {
        println!("    - {e}");
    }
}

#[cfg(feature = "sal")]
async fn cmd_migrate(args: &MigrateArgs) -> Result<()> {
    let src = migrate::open_store(&args.from)
        .await
        .context("open source store")?;
    let dst = migrate::open_store(&args.to)
        .await
        .context("open destination store")?;
    let report = migrate::migrate(
        src.as_ref(),
        dst.as_ref(),
        args.batch,
        args.namespace.clone(),
        args.dry_run,
    )
    .await;
    if args.json {
        let value = serde_json::json!({
            "from_url": args.from,
            "to_url": args.to,
            "memories_read": report.memories_read,
            "memories_written": report.memories_written,
            "batches": report.batches,
            "errors": report.errors,
            "dry_run": report.dry_run,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("migration report");
        println!("  from:              {}", args.from);
        println!("  to:                {}", args.to);
        println!("  memories_read:     {}", report.memories_read);
        println!("  memories_written:  {}", report.memories_written);
        println!("  batches:           {}", report.batches);
        println!("  dry_run:           {}", report.dry_run);
        println!("  errors:            {}", report.errors.len());
        for e in &report.errors {
            println!("    - {e}");
        }
    }
    if !report.errors.is_empty() {
        anyhow::bail!("migration completed with {} error(s)", report.errors.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_short_truncates() {
        assert_eq!(id_short("abcdefghijklmnop"), "abcdefgh");
    }

    #[test]
    fn id_short_short_input() {
        assert_eq!(id_short("abc"), "abc");
    }

    #[test]
    fn id_short_empty() {
        assert_eq!(id_short(""), "");
    }

    #[test]
    fn human_age_just_now() {
        let now = chrono::Utc::now().to_rfc3339();
        assert_eq!(human_age(&now), "just now");
    }

    #[test]
    fn human_age_minutes() {
        let past = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let age = human_age(&past);
        assert!(age.contains("m ago"), "got: {age}");
    }

    #[test]
    fn human_age_hours() {
        let past = (chrono::Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
        let age = human_age(&past);
        assert!(age.contains("h ago"), "got: {age}");
    }

    #[test]
    fn human_age_days() {
        let past = (chrono::Utc::now() - chrono::Duration::days(5)).to_rfc3339();
        let age = human_age(&past);
        assert!(age.contains("d ago"), "got: {age}");
    }

    #[test]
    fn human_age_invalid_returns_input() {
        assert_eq!(human_age("not-a-date"), "not-a-date");
    }

    #[test]
    fn auto_namespace_returns_nonempty() {
        let ns = auto_namespace();
        assert!(!ns.is_empty());
    }
}
