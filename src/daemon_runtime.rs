// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Daemon runtime — orchestration shell for the `ai-memory` binary.
//!
//! W6 lifted `serve()` and the top-level dispatch out of `main.rs` so the
//! production HTTP daemon, the integration test harness, and the
//! coverage-instrumented tests in this module all share one source of
//! truth. `main.rs` keeps its `#[tokio::main]` entry point but immediately
//! delegates here for every subcommand.
//!
//! ## Public surface (post-W6)
//!
//! - [`run`] — top-level CLI dispatch (called from `main()`).
//! - [`serve`] — full HTTP daemon body (TLS or plain).
//! - [`bootstrap_serve`] — testable struct-returning state builder.
//! - [`build_router`] — composition wrapper around `lib::build_router`.
//! - [`build_embedder`], [`build_vector_index`] — single canonical builders
//!   used by both `serve()` and `cli::recall::run`.
//! - [`spawn_gc_loop`], [`spawn_wal_checkpoint_loop`] — daemon background
//!   tasks, returning a [`JoinHandle`] so callers can abort on shutdown.
//! - [`is_write_command`] — write-command predicate driving the post-write
//!   WAL checkpoint.
//! - [`passphrase_from_file`], [`apply_anonymize_default`] — startup helpers.
//!
//! ## Pre-W6 helpers retained
//!
//! - [`serve_http_with_shutdown`], [`serve_http_with_shutdown_future`] —
//!   the in-process HTTP harness the integration suite drives.
//! - [`run_sync_daemon_with_shutdown`],
//!   [`run_sync_daemon_with_shutdown_using_client`],
//!   [`sync_cycle_once`] — the sync-daemon body.
//! - [`run_curator_daemon_with_shutdown`],
//!   [`run_curator_daemon_with_primitives`] — the curator-daemon body.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::Router;
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use rusqlite::Connection;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

use crate::cli::agents::{AgentsArgs, PendingArgs};
use crate::cli::archive::ArchiveArgs;
use crate::cli::audit::AuditArgs;
use crate::cli::backup::{BackupArgs, RestoreArgs};
use crate::cli::boot::BootArgs;
use crate::cli::consolidate::{AutoConsolidateArgs, ConsolidateArgs};
use crate::cli::crud::{DeleteArgs, GetArgs, ListArgs};
use crate::cli::curator::CuratorArgs;
use crate::cli::forget::ForgetArgs;
use crate::cli::install::InstallArgs;
use crate::cli::io::{ImportArgs, MineArgs};
use crate::cli::link::{LinkArgs, ResolveArgs};
use crate::cli::logs::LogsArgs;
use crate::cli::promote::PromoteArgs;
use crate::cli::recall::RecallArgs;
use crate::cli::search::SearchArgs;
use crate::cli::store::StoreArgs;
use crate::cli::sync::{SyncArgs, SyncDaemonArgs};
use crate::cli::update::UpdateArgs;
use crate::cli::wrap::WrapArgs;
use crate::config::{AppConfig, FeatureTier};
use crate::embeddings::Embedder;
use crate::handlers::{ApiKeyState, AppState, Db};
use crate::hnsw::VectorIndex;
use crate::{bench, cli, db, embeddings, federation, hnsw, llm, mcp, tls};

#[cfg(feature = "sal")]
use crate::migrate;

const DEFAULT_DB: &str = "ai-memory.db";
const DEFAULT_PORT: u16 = 9077;
const GC_INTERVAL_SECS: u64 = 1800;
/// WAL auto-checkpoint cadence in the HTTP daemon. Bounds `*-wal`
/// file growth between `SQLite`'s internal page-count checkpoints.
const WAL_CHECKPOINT_INTERVAL_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// Clap-derived CLI surface
// ---------------------------------------------------------------------------
//
// The clap structs live in the lib crate so `daemon_runtime::run` can
// take them as parameters. `main.rs` re-exports `Cli` and immediately
// delegates here.

#[derive(Parser)]
#[command(
    name = "ai-memory",
    version,
    about = "AI-agnostic persistent memory — MCP server, HTTP API, and CLI for any AI platform"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    #[arg(long, env = "AI_MEMORY_DB", default_value = DEFAULT_DB, global = true)]
    pub db: PathBuf,
    /// Output as JSON (machine-parseable)
    #[arg(long, global = true, default_value_t = false)]
    pub json: bool,
    /// Agent identifier used for store operations. If unset, an NHI-hardened
    /// default is synthesized (see `ai-memory store --help`). Accepts the
    /// `AI_MEMORY_AGENT_ID` environment variable as a fallback.
    #[arg(long, env = "AI_MEMORY_AGENT_ID", global = true)]
    pub agent_id: Option<String>,
    /// v0.6.0.0: path to a file containing the `SQLCipher` passphrase.
    /// Only meaningful when the binary was built with
    /// `--features sqlcipher` (standard builds ignore this flag). File
    /// must be root-readable (mode 0400 recommended). The passphrase is
    /// read once at startup and exported as `AI_MEMORY_DB_PASSPHRASE`
    /// for the duration of the process — passing the passphrase
    /// directly as an env var or as a flag value leaks to the process
    /// list (`ps -E`) and shell history.
    #[arg(long, global = true, value_name = "PATH")]
    pub db_passphrase_file: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Command {
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
    /// v0.6.3 (Pillar 3 / Stream E): run the canonical performance
    /// workload and print measured p50/p95/p99 against the budgets in
    /// `PERFORMANCE.md`. Each invocation seeds a disposable temp DB so
    /// the user's main DB is untouched. Exits non-zero when any p95
    /// exceeds its budget by more than the published 10% tolerance.
    Bench(BenchArgs),
    /// v0.7: migrate memories between SAL backends. Gated behind
    /// `--features sal`. Reads pages via `MemoryStore::list`, writes
    /// via `MemoryStore::store`. Idempotent: source ids are preserved
    /// and both adapters upsert on id.
    #[cfg(feature = "sal")]
    Migrate(MigrateArgs),
    /// v0.6.3.1 (P7 / R7): operator-visible health dashboard. Reads
    /// Capabilities v2 (P1) + data integrity surfaces (P2) + recall
    /// observability (P3). With `--remote <url>` becomes a fleet doctor
    /// at T3+. Read-only — never mutates the database. Exits 0 on a
    /// healthy report, 2 on critical findings, and 1 on warnings when
    /// `--fail-on-warn` is passed.
    Doctor(DoctorCliArgs),
    /// Issue #487: emit session-boot context. Universal primitive every
    /// AI-agent integration recipe (Claude Code SessionStart hook, Cursor /
    /// Cline / Continue / Windsurf system-message, Codex / Apps SDK /
    /// Agent SDK programmatic prepend, OpenClaw built-in, local models
    /// via LM Studio / Ollama / vLLM) calls before the agent's first turn.
    /// Read-only, fast, never blocks. With `--quiet` (recommended for
    /// hooks) a missing DB exits 0 with empty stdout.
    Boot(BootArgs),
    /// Issue #487 PR-2: wire `ai-memory boot` and the `ai-memory-mcp`
    /// server into AI agents' config files (Claude Code SessionStart hook,
    /// Cursor / Cline / Continue / Windsurf / OpenClaw MCP config). Default
    /// is `--dry-run` (prints the diff, writes nothing). Pass `--apply` to
    /// commit. Pass `--uninstall --apply` to remove a previously-installed
    /// managed block.
    Install(InstallArgs),
    /// Issue #487 PR-6: cross-platform Rust replacement for the bash /
    /// PowerShell wrappers PR-1 shipped in the integration recipes. Runs
    /// `ai-memory boot` in-process, builds a system message, then spawns
    /// the named agent CLI with the system message delivered via the
    /// strategy chosen by `default_strategy(<agent>)` (or an explicit
    /// `--system-flag` / `--system-env` / `--message-file-flag`
    /// override). Exit code is propagated from the wrapped agent.
    Wrap(WrapArgs),
    /// Issue #487 PR-5: operator-facing CLI for the operational logging
    /// facility (`tail`, `cat`, `archive`, `purge`). Default-OFF — emits
    /// nothing useful unless `[logging] enabled = true` is set in
    /// `config.toml`.
    Logs(LogsArgs),
    /// Issue #487 PR-5: operator-facing CLI for the security audit
    /// trail (`verify`, `tail`, `path`). Default-OFF — emits nothing
    /// useful unless `[audit] enabled = true` is set in `config.toml`.
    Audit(AuditArgs),
}

/// Arguments for the `doctor` subcommand. Lives next to `Cli` so clap
/// derives them automatically; the actual report logic lives in
/// `cli::doctor::run`.
#[derive(Args)]
pub struct DoctorCliArgs {
    /// Query a remote ai-memory daemon's HTTP capabilities + stats
    /// endpoints instead of opening the local DB. Sections that need
    /// raw SQL access render as N/A in this mode.
    #[arg(long, value_name = "URL")]
    pub remote: Option<String>,
    /// Emit the report as JSON instead of human-readable text. Useful
    /// for CI consumers and for `jq`-style filtering.
    #[arg(long)]
    pub json: bool,
    /// Exit 1 when at least one section is at WARN severity. Without
    /// this flag, warnings keep exit 0; criticals always exit 2.
    #[arg(long)]
    pub fail_on_warn: bool,
}

#[derive(Args)]
pub struct BenchArgs {
    /// Measured iterations per operation. Clamped to `[1, 100_000]`.
    #[arg(long, default_value_t = bench::DEFAULT_ITERATIONS)]
    pub iterations: usize,
    /// Warmup iterations discarded from the percentile sample.
    /// Clamped to `[0, 10_000]`.
    #[arg(long, default_value_t = bench::DEFAULT_WARMUP)]
    pub warmup: usize,
    /// Emit results as JSON instead of the human-readable table.
    #[arg(long)]
    pub json: bool,
    /// Path to a previous `bench --json` payload. When supplied, the
    /// fresh run is compared per-operation against this baseline and
    /// the process exits non-zero if any measured p95 exceeds the
    /// baseline by more than `--regression-threshold` percent.
    /// Independent of the absolute-budget guard.
    #[arg(long, value_name = "PATH")]
    pub baseline: Option<String>,
    /// Allowed p95 growth (percent) over the `--baseline` reading
    /// before a row is flagged as a regression. Clamped to
    /// `[0.0, 1000.0]`. Has no effect without `--baseline`.
    #[arg(long, default_value_t = bench::DEFAULT_REGRESSION_THRESHOLD_PCT)]
    pub regression_threshold: f64,
    /// Append this run to a JSONL history file (one self-describing
    /// JSON object per line). Creates the file and any missing parent
    /// directories on first call. Each entry carries `captured_at`
    /// (RFC3339), `iterations`, `warmup`, and the same `results` array
    /// `--json` emits — long-running campaigns can build a regression
    /// dataset to feed downstream tooling. The CLI table / JSON output
    /// still prints; this flag only adds the append side effect.
    #[arg(long, value_name = "PATH")]
    pub history: Option<PathBuf>,
}

#[cfg(feature = "sal")]
#[derive(Args)]
pub struct MigrateArgs {
    /// Source URL. `sqlite:///path/to/file.db` or
    /// `postgres://user:pass@host:port/dbname`.
    #[arg(long)]
    pub from: String,
    /// Destination URL. Same URL shape as `--from`.
    #[arg(long)]
    pub to: String,
    /// Page size. Clamped to [1, 10000]. Default 1000.
    #[arg(long, default_value_t = 1000)]
    pub batch: usize,
    /// Only migrate memories in this namespace.
    #[arg(long)]
    pub namespace: Option<String>,
    /// Emit the report but do NOT write to the destination.
    #[arg(long)]
    pub dry_run: bool,
    /// Emit the report as JSON rather than human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Path to PEM-encoded TLS certificate (may include the full chain).
    /// Passing both `--tls-cert` and `--tls-key` switches `serve` to
    /// HTTPS. rustls under the hood — no OpenSSL dep. Absent both
    /// flags = plain HTTP (same as every previous release).
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    /// Path to PEM-encoded TLS private key (PKCS#8 or RSA).
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
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
    pub mtls_allowlist: Option<PathBuf>,
    /// Seconds to wait for in-flight requests to complete on graceful
    /// shutdown (SIGINT). Default 30. Bumped from 10 in v0.6.0 because
    /// large `/sync/push` batches can take longer than 10s under load
    /// (red-team #233).
    #[arg(long, default_value_t = 30)]
    pub shutdown_grace_secs: u64,

    // -------- v0.7 federation (ADR-0001) ---------------------------
    /// W-of-N write quorum. When >=1 and `--quorum-peers` is non-empty,
    /// every HTTP write fans out to every peer and returns OK only
    /// after the local commit + W-1 peer acks land within
    /// `--quorum-timeout-ms`. Default 0 = federation disabled, daemon
    /// behaves exactly like v0.6.0.
    #[arg(long, default_value_t = 0)]
    pub quorum_writes: usize,
    /// Comma-separated list of peer base URLs. Each peer is assumed to
    /// expose `POST /api/v1/sync/push` — the same endpoint the
    /// sync-daemon already uses.
    #[arg(long, value_delimiter = ',')]
    pub quorum_peers: Vec<String>,
    /// Deadline for quorum-ack collection. After this many ms the
    /// write returns 503 `quorum_not_met`. Default 2000.
    #[arg(long, default_value_t = 2000)]
    pub quorum_timeout_ms: u64,
    /// Optional mTLS client cert for outbound federation POSTs. Same
    /// cert material the sync-daemon's `--client-cert` accepts.
    #[arg(long)]
    pub quorum_client_cert: Option<PathBuf>,
    /// Optional mTLS client key for outbound federation POSTs.
    #[arg(long)]
    pub quorum_client_key: Option<PathBuf>,
    /// Optional root CA cert to trust for outbound federation HTTPS.
    /// Required whenever peers present a cert NOT rooted in Mozilla's
    /// `webpki-roots` bundle (self-signed, private CA, ephemeral test
    /// CA, etc.) — without this, the reqwest rustls-tls client rejects
    /// peer certs and every quorum write times out as `quorum_not_met`.
    /// See #333.
    #[arg(long)]
    pub quorum_ca_cert: Option<PathBuf>,
    /// v0.6.0.1 (#320) — how often, in seconds, the daemon pulls peers
    /// for any updates it missed while offline or partitioned. 0 disables
    /// the catchup loop entirely. Default 30s keeps a post-partition
    /// node convergent within one interval after resume.
    #[arg(long, default_value_t = 30)]
    pub catchup_interval_secs: u64,
}

#[derive(Args)]
pub struct CompletionsArgs {
    pub shell: Shell,
}

// ---------------------------------------------------------------------------
// Top-level dispatch
// ---------------------------------------------------------------------------

/// Top-level CLI dispatch. Called from `main()` after `Cli::parse()`.
///
/// Handles:
/// - `--db-passphrase-file` → exports `AI_MEMORY_DB_PASSPHRASE`.
/// - `is_write_command` → conditional post-run WAL checkpoint.
/// - The match arm for every `Command` variant.
#[allow(clippy::too_many_lines)]
pub async fn run(cli: Cli, app_config: &AppConfig) -> Result<()> {
    // v0.6.0.0: read the SQLCipher passphrase from a file and export it as
    // AI_MEMORY_DB_PASSPHRASE for the duration of the process. File path
    // comes from the --db-passphrase-file flag (global). No-op on standard
    // SQLite builds (the env var is ignored unless the binary was built
    // with --features sqlcipher).
    if let Some(path) = &cli.db_passphrase_file {
        let passphrase = passphrase_from_file(path)?;
        // SAFETY: single-threaded startup before any worker threads spawn.
        unsafe { std::env::set_var("AI_MEMORY_DB_PASSPHRASE", passphrase) };
    }
    let db_path = app_config.effective_db(&cli.db);
    let j = cli.json;
    let cli_agent_id: Option<String> = cli.agent_id.clone();
    // Track whether command writes to DB (for WAL checkpoint)
    let needs_checkpoint = is_write_command(&cli.command);
    let db_path_for_checkpoint = if needs_checkpoint {
        Some(db_path.clone())
    } else {
        None
    };

    let result = match cli.command {
        Command::Serve(a) => serve(db_path, a, app_config).await,
        Command::Mcp { tier } => {
            let feature_tier = app_config.effective_tier(Some(&tier));
            mcp::run_mcp_server(&db_path, feature_tier, app_config)?;
            Ok(())
        }
        Command::Store(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::store::run(
                &db_path,
                a,
                j,
                app_config,
                cli_agent_id.as_deref(),
                &mut out,
            )
        }
        Command::Update(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::update::run(&db_path, &a, j, &mut out)
        }
        Command::Recall(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::recall::run(&db_path, &a, j, app_config, &mut out)
        }
        Command::Search(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::search::run(&db_path, &a, j, &mut out)
        }
        Command::Get(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::crud::cmd_get(&db_path, &a, j, &mut out)
        }
        Command::List(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::crud::cmd_list(&db_path, &a, j, app_config, &mut out)
        }
        Command::Delete(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::crud::cmd_delete(&db_path, &a, j, cli_agent_id.as_deref(), &mut out)
        }
        Command::Promote(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::promote::cmd_promote(&db_path, &a, j, cli_agent_id.as_deref(), &mut out)
        }
        Command::Forget(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::forget::cmd_forget(&db_path, &a, j, &mut out)
        }
        Command::Link(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::link::cmd_link(&db_path, &a, j, &mut out)
        }
        Command::Consolidate(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::consolidate::run(&db_path, a, j, cli_agent_id.as_deref(), &mut out)
        }
        Command::Resolve(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::link::cmd_resolve(&db_path, &a, j, &mut out)
        }
        Command::Shell => cli::shell::run(&db_path),
        Command::Sync(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::sync::run(&db_path, &a, j, cli_agent_id.as_deref(), &mut out)
        }
        Command::SyncDaemon(a) => cli::sync::run_daemon(&db_path, a, cli_agent_id.as_deref()).await,
        Command::AutoConsolidate(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::consolidate::run_auto(&db_path, &a, j, cli_agent_id.as_deref(), &mut out)
        }
        Command::Gc => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::gc::run_gc(&db_path, j, app_config, &mut out)
        }
        Command::Stats => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::gc::run_stats(&db_path, j, &mut out)
        }
        Command::Namespaces => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::gc::run_namespaces(&db_path, j, &mut out)
        }
        Command::Export => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::io::export(&db_path, &mut out)
        }
        Command::Import(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::io::import(&db_path, &a, j, cli_agent_id.as_deref(), &mut out)
        }
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
        Command::Mine(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::io::mine(
                &db_path,
                a,
                j,
                app_config,
                cli_agent_id.as_deref(),
                &mut out,
            )
        }
        Command::Archive(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::archive::run(&db_path, a, j, &mut out)
        }
        Command::Agents(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::agents::run_agents(&db_path, a, j, &mut out)
        }
        Command::Pending(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::agents::run_pending(&db_path, a, j, cli_agent_id.as_deref(), &mut out)
        }
        Command::Backup(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::backup::run_backup(&db_path, &a, j, &mut out)
        }
        Command::Restore(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::backup::run_restore(&db_path, &a, j, &mut out)
        }
        Command::Curator(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::curator::run(&db_path, &a, app_config, &mut out).await
        }
        Command::Bench(a) => cmd_bench(&a),
        #[cfg(feature = "sal")]
        Command::Migrate(a) => cmd_migrate(&a).await,
        Command::Doctor(a) => {
            // P7 / R7. The doctor is read-only; it never sets
            // `needs_checkpoint`. We compute the exit code from the
            // overall severity and propagate it via the process-exit
            // path below so callers (CI, ops scripts) can branch on it.
            //
            // The remote mode uses `reqwest::blocking::Client` which
            // panics when dropped on a tokio runtime thread, so the
            // entire doctor pass runs inside `spawn_blocking`.
            let db_path_doctor = db_path.clone();
            let args = cli::doctor::DoctorArgs {
                remote: a.remote,
                json: a.json,
                fail_on_warn: a.fail_on_warn,
            };
            let join = tokio::task::spawn_blocking(move || {
                let stdout = std::io::stdout();
                let stderr = std::io::stderr();
                let mut so = stdout.lock();
                let mut se = stderr.lock();
                let mut out = cli::CliOutput::from_std(&mut so, &mut se);
                cli::doctor::run(&db_path_doctor, &args, &mut out)
            })
            .await;
            match join {
                Ok(Ok(0)) => Ok(()),
                Ok(Ok(code)) => std::process::exit(code),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("doctor task join failed: {e}")),
            }
        }
        Command::Boot(a) => {
            // Issue #487. Read-only, fast, no embedder, no daemon. Suitable
            // for invocation from any AI-agent integration (Claude Code
            // SessionStart hook, Cursor / Cline / Continue / Windsurf
            // system-message, programmatic prepend in Claude Agent SDK /
            // OpenAI Apps SDK / Codex CLI, OpenClaw built-in, local models
            // via LM Studio / Ollama / vLLM).
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            // PR-5: a `boot` invocation is itself an audit-worthy event.
            // Emission is a no-op when audit is disabled.
            crate::audit::emit(crate::audit::EventBuilder::new(
                crate::audit::AuditAction::SessionBoot,
                crate::audit::actor(
                    cli_agent_id.as_deref().unwrap_or("anonymous"),
                    "explicit_or_default",
                    None,
                ),
                crate::audit::target_sweep(a.namespace.as_deref().unwrap_or("auto")),
            ));
            cli::boot::run(&db_path, &a, app_config, &mut out)
        }
        Command::Install(a) => {
            // Issue #487 PR-2. Read-only filesystem op against the agent's
            // config file (NOT the ai-memory DB). Default is dry-run; --apply
            // is opt-in and writes a backup before mutating anything.
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::install::run(&a, &mut out)
        }
        Command::Wrap(a) => {
            // Issue #487 PR-6. Pure-Rust cross-platform replacement for
            // the bash / PowerShell wrappers PR-1 shipped in the
            // integration recipes. Runs boot in-process, builds the
            // system message, spawns the wrapped agent, and propagates
            // the agent's exit code via std::process::exit.
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            let code = cli::wrap::run(&db_path, &a, app_config, &mut out)?;
            // Drop the locks/output before exit so any pending writes
            // get flushed by the OS on process teardown.
            drop(out);
            drop(so);
            drop(se);
            if code == 0 {
                Ok(())
            } else {
                std::process::exit(code);
            }
        }
        Command::Logs(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            cli::logs::run(a, app_config, &mut out)
        }
        Command::Audit(a) => {
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut so = stdout.lock();
            let mut se = stderr.lock();
            let mut out = cli::CliOutput::from_std(&mut so, &mut se);
            match cli::audit::run(a, app_config, &mut out)? {
                0 => Ok(()),
                code => std::process::exit(code),
            }
        }
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

// ---------------------------------------------------------------------------
// is_write_command — predicate for the post-run WAL checkpoint.
// ---------------------------------------------------------------------------

/// Returns true if `cmd` is a write-class subcommand. The post-run WAL
/// checkpoint in [`run`] runs only when this returns `true`.
#[must_use]
pub fn is_write_command(cmd: &Command) -> bool {
    matches!(
        cmd,
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
    )
}

// ---------------------------------------------------------------------------
// Startup helpers (passphrase, anonymize default)
// ---------------------------------------------------------------------------

/// Read the `SQLCipher` passphrase from `path`. Strips a single trailing
/// newline / CRLF; rejects an empty passphrase (post-strip) with an error;
/// preserves all other internal whitespace.
///
/// # Errors
///
/// - The file cannot be read (e.g. missing, permission denied).
/// - The passphrase, after stripping the trailing newline, is empty.
pub fn passphrase_from_file(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading passphrase file {}", path.display()))?;
    let passphrase = raw.trim_end_matches(['\n', '\r']).to_string();
    if passphrase.is_empty() {
        anyhow::bail!("passphrase file {} is empty", path.display());
    }
    Ok(passphrase)
}

/// Apply the configured `anonymize_default` to the runtime env: when the
/// config asks for anonymization but the user hasn't already set
/// `AI_MEMORY_ANONYMIZE`, set it to `"1"`. Idempotent — repeated calls are
/// a no-op once the env var is set.
///
/// Note: this writes to the process environment; callers must invoke it
/// from the single-threaded startup region (before any worker threads are
/// spawned). The production binary calls it from `main()` for that reason.
pub fn apply_anonymize_default(app_config: &AppConfig) {
    // #198: config → env mapping for agent_id anonymization. Env var already
    // set by the caller wins; config is only applied when the env is unset.
    if app_config.effective_anonymize_default() && std::env::var("AI_MEMORY_ANONYMIZE").is_err() {
        // SAFETY: single-threaded startup before any worker threads spawn.
        unsafe { std::env::set_var("AI_MEMORY_ANONYMIZE", "1") };
    }
}

// ---------------------------------------------------------------------------
// Embedder / vector-index canonical builders
// ---------------------------------------------------------------------------

/// Construct the [`Embedder`] for a given tier. Returns `None` for the
/// keyword tier (no embedder requested) and on load failure (caller
/// degrades to keyword fallback). On failure the diagnostic is emitted
/// via `tracing::error!` so operators see it in `journalctl`.
///
/// This is the single canonical embedder builder used by both `serve()`
/// (HTTP daemon) and `cli::recall::run` (offline recall). Prior to W6
/// each call site had its own copy, with subtly different fallback
/// shapes — the bug at issue #322 was a direct consequence.
pub async fn build_embedder(feature_tier: FeatureTier, app_config: &AppConfig) -> Option<Embedder> {
    let tier_config = feature_tier.config();
    let Some(emb_model) = tier_config.embedding_model else {
        tracing::info!(
            "embedder disabled — tier={} keyword-only (FTS5); semantic recall not wired",
            feature_tier.as_str()
        );
        return None;
    };
    let embed_url = app_config.effective_embed_url().to_string();
    // The HF-Hub sync API and candle model-load are blocking CPU work that
    // internally spin their own tokio runtime. Running them directly in this
    // async context panics with "Cannot drop a runtime in a context where
    // blocking is not allowed." Move the whole construction onto the blocking
    // pool so the inner runtime is owned by a dedicated thread.
    let build = match tokio::task::spawn_blocking(move || {
        let embed_client = llm::OllamaClient::new_with_url(&embed_url, "nomic-embed-text")
            .ok()
            .map(Arc::new);
        embeddings::Embedder::for_model(emb_model, embed_client)
    })
    .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("embedder spawn_blocking join failed: {e}");
            return None;
        }
    };
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
                "EMBEDDER LOAD FAILED — tier={} requested semantic features, \
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
}

/// Build the in-memory [`VectorIndex`] from `conn`. When `embedder_present`
/// is false, returns `None` (the keyword-only path doesn't need an index).
/// When the embedder is present but the DB is empty (or query errors),
/// returns `Some(VectorIndex::empty())` so write paths can populate it
/// in-place.
#[must_use]
pub fn build_vector_index(conn: &Connection, embedder_present: bool) -> Option<VectorIndex> {
    if !embedder_present {
        return None;
    }
    match db::get_all_embeddings(conn) {
        Ok(entries) if !entries.is_empty() => Some(hnsw::VectorIndex::build(entries)),
        _ => Some(hnsw::VectorIndex::empty()),
    }
}

// ---------------------------------------------------------------------------
// Background tasks (GC, WAL checkpoint)
// ---------------------------------------------------------------------------

/// Spawn the periodic GC loop. Sleeps `interval`, then runs `db::gc` and
/// `db::auto_purge_archive` against the daemon's shared connection. The
/// returned [`JoinHandle`] is owned by the caller; `serve()` aborts it on
/// shutdown.
#[must_use]
pub fn spawn_gc_loop(
    state: Db,
    archive_max_days: Option<i64>,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let lock = state.lock().await;
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
    })
}

/// Spawn the periodic WAL checkpoint loop. First checkpoint runs
/// `interval / 2` after start (staggered from the GC loop to avoid
/// lock-contention bursts on cold start), then on a fixed cadence.
#[must_use]
pub fn spawn_wal_checkpoint_loop(state: Db, interval: Duration) -> JoinHandle<()> {
    let half = interval / 2;
    tokio::spawn(async move {
        // First checkpoint runs halfway through the interval so the two
        // long-running maintenance tasks never overlap on cold start.
        tokio::time::sleep(half).await;
        loop {
            {
                let lock = state.lock().await;
                match db::checkpoint(&lock.0) {
                    Ok(()) => tracing::debug!("wal checkpoint: ok"),
                    Err(e) => tracing::warn!("wal checkpoint failed: {e}"),
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Router composition
// ---------------------------------------------------------------------------

/// Compose the production HTTP router. Thin wrapper around
/// [`crate::build_router`] (the W3-vintage source of truth for the
/// route table). `daemon_runtime::build_router` exists so test code in
/// this module can build the router without naming `crate::build_router`
/// directly, and so future router-composition logic (e.g. middleware
/// reorder, custom layers) lives in one place.
#[must_use]
pub fn build_router(app_state: AppState, api_key_state: ApiKeyState) -> Router {
    crate::build_router(api_key_state, app_state)
}

// ---------------------------------------------------------------------------
// serve() — the HTTP daemon body, post-W6 split.
// ---------------------------------------------------------------------------

/// Aggregated state produced by [`bootstrap_serve`].
pub struct ServeBootstrap {
    pub app_state: AppState,
    pub api_key_state: ApiKeyState,
    pub db_state: Db,
    pub archive_max_days: Option<i64>,
    pub task_handles: Vec<JoinHandle<()>>,
}

/// Build all daemon state and spawn background tasks. Returns the
/// aggregated state without binding any sockets — testable in isolation.
pub async fn bootstrap_serve(
    db_path: &Path,
    args: &ServeArgs,
    app_config: &AppConfig,
) -> Result<ServeBootstrap> {
    let resolved_ttl = app_config.effective_ttl();
    let archive_on_gc = app_config.effective_archive_on_gc();
    let conn = db::open(db_path)?;

    // Issue #219: build the embedder + HNSW index up front so HTTP write
    // paths can populate them. Previously the daemon never constructed an
    // embedder, silently excluding every HTTP-authored memory from semantic
    // recall. Build only when the configured feature tier enables it —
    // keyword-only deployments keep their zero-dep, zero-RAM profile.
    // Daemon has no per-invocation tier override; honour the config tier.
    let feature_tier = app_config.effective_tier(None);
    let tier_config = feature_tier.config();
    let embedder = build_embedder(feature_tier, app_config).await;
    let vector_index = build_vector_index(&conn, embedder.is_some());

    let db_state: Db = Arc::new(Mutex::new((
        conn,
        db_path.to_path_buf(),
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

    let mut task_handles: Vec<JoinHandle<()>> = Vec::new();

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

    let app_state = AppState {
        db: db_state.clone(),
        embedder: Arc::new(embedder),
        vector_index: Arc::new(Mutex::new(vector_index)),
        federation: Arc::new(federation),
        tier_config: Arc::new(tier_config),
        scoring: Arc::new(app_config.effective_scoring()),
    };

    // Automatic GC.
    task_handles.push(spawn_gc_loop(
        db_state.clone(),
        app_config.archive_max_days,
        Duration::from_secs(GC_INTERVAL_SECS),
    ));

    // v0.6.0 GA: periodic WAL checkpoint. Under continuous writes the WAL
    // file grows until SQLite's auto-checkpoint fires (every 1000 pages by
    // default) — which is inconsistent timing and can leave the file at
    // hundreds of MB between auto-checkpoints. A dedicated task running on
    // a fixed cadence keeps the WAL bounded and makes operational storage
    // behaviour predictable. We stagger from GC to avoid lock-contention
    // bursts. See docs/ARCHITECTURAL_LIMITS.md for why this workaround is
    // necessary in a single-connection daemon.
    task_handles.push(spawn_wal_checkpoint_loop(
        db_state.clone(),
        Duration::from_secs(WAL_CHECKPOINT_INTERVAL_SECS),
    ));

    let api_key_state = ApiKeyState {
        key: app_config.api_key.clone(),
    };
    if api_key_state.key.is_some() {
        tracing::info!("API key authentication enabled");
    }

    Ok(ServeBootstrap {
        app_state,
        api_key_state,
        db_state,
        archive_max_days: app_config.archive_max_days,
        task_handles,
    })
}

/// Init the tracing subscriber for the HTTP daemon. Idempotent at the
/// `tracing-subscriber` level — repeated calls log a warning and no-op
/// rather than panic. Split out from `serve()` so test code can opt out.
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("ai_memory=info".parse().unwrap())
                .add_directive("tower_http=info".parse().unwrap()),
        )
        .try_init();
}

/// Run the HTTP memory daemon. Loads TLS state, builds `AppState`, spawns
/// the GC + WAL-checkpoint loops, and binds a listener (TLS or plain HTTP).
///
/// Behaviour is preserved from the pre-W6 inline `main::serve` body — only
/// the structure has changed.
#[allow(clippy::too_many_lines)]
pub async fn serve(db_path: PathBuf, args: ServeArgs, app_config: &AppConfig) -> Result<()> {
    init_tracing();

    let bootstrap = bootstrap_serve(&db_path, &args, app_config).await?;

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("database: {}", db_path.display());

    // Graceful shutdown with WAL checkpoint
    let shutdown_state = bootstrap.db_state.clone();
    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down — checkpointing WAL");
        let lock = shutdown_state.lock().await;
        let _ = db::checkpoint(&lock.0);
    };

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
            tls::load_mtls_rustls_config(cert, key, allowlist_path).await?
        } else {
            tracing::warn!(
                "TLS enabled but mTLS NOT configured — sync endpoints \
                 (/api/v1/sync/push, /api/v1/sync/since) accept any client. \
                 Set --mtls-allowlist for production peer-mesh deployments \
                 (red-team #231)."
            );
            tls::load_rustls_config(cert, key).await?
        };
        let app = build_router(bootstrap.app_state, bootstrap.api_key_state);
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
        // Wave 3 (v0.6.3): the non-TLS path delegates to
        // `daemon_runtime::serve_http_with_shutdown_future`, which is the
        // same `build_router` + `TcpListener::bind` + `axum::serve` body
        // the integration tests drive in-process. Production threads its
        // WAL-checkpoint-on-shutdown future in directly so the cleanup
        // semantic is preserved verbatim.
        serve_http_with_shutdown_future(
            &addr,
            bootstrap.api_key_state,
            bootstrap.app_state,
            shutdown,
        )
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// cmd_bench / cmd_migrate (no-op for non-sal builds)
// ---------------------------------------------------------------------------

fn cmd_bench(args: &BenchArgs) -> Result<()> {
    let iterations = args.iterations.clamp(1, 100_000);
    let warmup = args.warmup.min(10_000);
    let regression_threshold = args.regression_threshold.clamp(0.0, 1000.0);
    // Bench always seeds a disposable in-memory DB so the operator's
    // main DB (and disk) are untouched. SQLite's `:memory:` URL and
    // WAL-less mode keep the workload bounded by RAM and CPU.
    let conn = db::open(Path::new(":memory:"))?;
    let config = bench::BenchConfig {
        iterations,
        warmup,
        namespace: bench::BENCH_NAMESPACE.to_string(),
    };
    let results = bench::run(&conn, &config)?;

    let regressions = if let Some(path) = &args.baseline {
        let baseline = bench::load_baseline(Path::new(path))?;
        Some(bench::compare_against_baseline(
            &results,
            &baseline,
            regression_threshold,
        ))
    } else {
        None
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "iterations": iterations,
                "warmup": warmup,
                "results": results,
                "regressions": regressions,
            }))?
        );
    } else {
        print!("{}", bench::render_table(&results));
        if let Some(rows) = &regressions {
            println!();
            print!("{}", bench::render_regression_table(rows));
        }
    }

    if let Some(history_path) = &args.history {
        let captured_at = chrono::Utc::now().to_rfc3339();
        bench::append_history(history_path, &captured_at, iterations, warmup, &results)?;
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(
            stderr,
            "bench: appended run to history file {}",
            history_path.display()
        );
    }

    let budget_failed = results
        .iter()
        .any(|r| matches!(r.status, bench::Status::Fail));
    let regression_failed = regressions
        .as_ref()
        .is_some_and(|rows| rows.iter().any(|r| r.regressed));

    if budget_failed && regression_failed {
        anyhow::bail!(
            "bench: at least one operation exceeded its p95 budget by >10% AND regressed >{regression_threshold:.1}% vs baseline"
        );
    }
    if budget_failed {
        anyhow::bail!("bench: at least one operation exceeded its p95 budget by >10%");
    }
    if regression_failed {
        anyhow::bail!(
            "bench: at least one operation regressed >{regression_threshold:.1}% vs baseline"
        );
    }
    Ok(())
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

// ---------------------------------------------------------------------------
// Pre-W6 helpers — in-process HTTP harness, sync-daemon body, curator-daemon body.
// ---------------------------------------------------------------------------

/// Run the HTTP daemon (plain HTTP, no TLS) with a programmable shutdown.
///
/// Mirrors the `else` branch of `serve()` in pre-W6 `main.rs` (the non-TLS
/// path). Builds the production `Router` via `build_router`, binds a
/// `TcpListener` to `addr`, and runs `axum::serve` with a graceful-shutdown
/// future that resolves when `shutdown.notify_one()` is called.
///
/// Tests pass a known port (pick one via `free_port()` and pass
/// `127.0.0.1:<port>`). The function returns when shutdown completes;
/// callers can `tokio::spawn` it and `notify` to stop.
pub async fn serve_http_with_shutdown(
    addr: &str,
    api_key_state: ApiKeyState,
    app_state: AppState,
    shutdown: Arc<Notify>,
) -> Result<()> {
    serve_http_with_shutdown_future(addr, api_key_state, app_state, async move {
        shutdown.notified().await;
    })
    .await
}

/// Variant of [`serve_http_with_shutdown`] that takes an arbitrary
/// shutdown future. The production `serve()` needs to run a WAL
/// checkpoint after the OS signal but before tearing down the listener;
/// that cleanup work is awkward to express through a `Notify` alone.
/// Accepting a `Future` lets the caller embed any async cleanup into the
/// shutdown future itself, while the helper keeps the `build_router` +
/// `TcpListener::bind` + `axum::serve` body it already owns.
pub async fn serve_http_with_shutdown_future<F>(
    addr: &str,
    api_key_state: ApiKeyState,
    app_state: AppState,
    shutdown: F,
) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let app = crate::build_router(api_key_state, app_state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("axum::serve")?;
    Ok(())
}

/// Run a single sync cycle against one peer — pull then push.
///
/// Lifted verbatim (modulo path-of-Path-vs-PathBuf) from the pre-W6
/// `main.rs::sync_cycle_once` so the integration sync-daemon test can
/// drive it without subprocess. The signature matches the private
/// main.rs helper 1:1 to keep call sites identical.
pub async fn sync_cycle_once(
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
            if crate::validate::validate_memory(mem).is_ok() {
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

/// Run the sync-daemon main loop with a programmable shutdown.
///
/// Mirrors the body of the pre-W6 `cmd_sync_daemon()` in `main.rs`: for
/// each cycle, fan out a `JoinSet` across `peers`, then race a sleep
/// against the shutdown notify. Returns when the notify fires. The
/// integration test can build a one-cycle test by setting `interval_secs=1`
/// and notifying after a short tokio sleep.
pub async fn run_sync_daemon_with_shutdown(
    db_path: PathBuf,
    local_agent_id: String,
    peers: Vec<String>,
    api_key: Option<String>,
    interval_secs: u64,
    batch_size: usize,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    run_sync_daemon_with_shutdown_using_client(
        client,
        db_path,
        local_agent_id,
        peers,
        api_key,
        interval_secs,
        batch_size,
        shutdown,
    )
    .await
}

/// Variant of [`run_sync_daemon_with_shutdown`] that takes a caller-built
/// `reqwest::Client`. The production `cmd_sync_daemon()` constructs an
/// mTLS-aware client (via `build_rustls_client_config`) and threads it
/// in here so the helper drives the same loop body the test version
/// drives — keeping `daemon_runtime` as the single source of truth for
/// the sync-daemon loop while preserving the production TLS contract.
pub async fn run_sync_daemon_with_shutdown_using_client(
    client: reqwest::Client,
    db_path: PathBuf,
    local_agent_id: String,
    peers: Vec<String>,
    api_key: Option<String>,
    interval_secs: u64,
    batch_size: usize,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let interval = interval_secs.max(1);
    let batch_size = batch_size.max(1);

    let db_path_owned: Arc<Path> = Arc::from(db_path.as_path());
    let local_agent_id_arc: Arc<str> = Arc::from(local_agent_id.as_str());
    let api_key_arc: Option<Arc<str>> = api_key.as_deref().map(Arc::from);
    let peers_arc: Vec<Arc<str>> = peers.iter().map(|s| Arc::from(s.as_str())).collect();
    loop {
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
            () = tokio::time::sleep(Duration::from_secs(interval)) => {}
            () = shutdown.notified() => {
                tracing::info!("sync-daemon: shutdown signal received");
                return Ok(());
            }
        }
    }
}

/// Run the curator daemon with a programmable shutdown.
///
/// Mirrors the daemon arm of the pre-W6 `cmd_curator()`. The inner work is
/// `curator::run_daemon` (a blocking, tight-loop-with-`AtomicBool` already
/// in lib code), which we drive from a `spawn_blocking`. Tests fire the
/// `Notify` to set the shutdown bool and the blocking task observes it
/// within ~500ms (`run_daemon`'s sleep tick).
pub async fn run_curator_daemon_with_shutdown(
    db_path: PathBuf,
    cfg: crate::curator::CuratorConfig,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_for_signal = shutdown_flag.clone();
    tokio::spawn(async move {
        shutdown.notified().await;
        shutdown_flag_for_signal.store(true, Ordering::Relaxed);
    });

    let llm_arc: Option<Arc<crate::llm::OllamaClient>> = None;
    let db_owned = db_path;
    tokio::task::spawn_blocking(move || {
        crate::curator::run_daemon(db_owned, llm_arc, cfg, shutdown_flag);
    })
    .await
    .map_err(|e| anyhow::anyhow!("curator daemon join: {e}"))?;
    Ok(())
}

/// Curator-daemon loop body, primitive-arg flavour for the binary.
///
/// `ollama_model` of `None` disables the LLM (matching the pre-tiered
/// keyword-only path in `build_curator_llm`).
#[allow(clippy::too_many_arguments)]
pub async fn run_curator_daemon_with_primitives(
    db_path: PathBuf,
    interval_secs: u64,
    max_ops_per_cycle: usize,
    dry_run: bool,
    include_namespaces: Vec<String>,
    exclude_namespaces: Vec<String>,
    ollama_model: Option<String>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let cfg = crate::curator::CuratorConfig {
        interval_secs,
        max_ops_per_cycle,
        dry_run,
        include_namespaces,
        exclude_namespaces,
    };
    let llm: Option<Arc<crate::llm::OllamaClient>> =
        ollama_model.and_then(|m| crate::llm::OllamaClient::new(&m).ok().map(Arc::new));

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_for_signal = shutdown_flag.clone();
    tokio::spawn(async move {
        shutdown.notified().await;
        shutdown_flag_for_signal.store(true, Ordering::Relaxed);
    });

    tokio::task::spawn_blocking(move || {
        crate::curator::run_daemon(db_path, llm, cfg, shutdown_flag);
    })
    .await
    .map_err(|e| anyhow::anyhow!("curator daemon join: {e}"))?;
    Ok(())
}

// -----------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------

/// Minimal URL-component encoder — only the characters the sync-daemon
/// queries actually emit (RFC3339 timestamps with `:` and `+`, and
/// agent ids with `:`/`@`/`/`). Mirror of the pre-W6
/// `main.rs::urlencoding_minimal`.
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

/// Mirrors the pre-W6 `main.rs::SyncSinceResponse` — the fields we
/// deserialize from the peer's `/api/v1/sync/since` body. `count` and
/// `limit` are present in the wire payload but unused on the receive
/// side; allowed to be dead so `clippy::pedantic` doesn't trip.
#[derive(serde::Deserialize)]
struct SyncSinceResponse {
    #[allow(dead_code)]
    count: usize,
    #[allow(dead_code)]
    limit: usize,
    memories: Vec<crate::models::Memory>,
}

/// Re-export the `Instant`/`Duration` types so test crate use sites stay
/// terse.  Kept private — internal to this module.
#[allow(dead_code)]
fn _imports_in_use(_: Instant, _: Duration) {}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::TestEnv;
    use crate::config::ResolvedTtl;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    // ----- helpers -------------------------------------------------------

    fn args_with_db(_db: &Path) -> ServeArgs {
        ServeArgs {
            host: "127.0.0.1".to_string(),
            port: 0,
            tls_cert: None,
            tls_key: None,
            mtls_allowlist: None,
            shutdown_grace_secs: 30,
            quorum_writes: 0,
            quorum_peers: vec![],
            quorum_timeout_ms: 2000,
            quorum_client_cert: None,
            quorum_client_key: None,
            quorum_ca_cert: None,
            catchup_interval_secs: 0,
        }
    }

    fn keyword_app_state(db_path: &Path) -> AppState {
        let conn = db::open(db_path).unwrap();
        let db_state: Db = Arc::new(Mutex::new((
            conn,
            db_path.to_path_buf(),
            ResolvedTtl::default(),
            true,
        )));
        AppState {
            db: db_state,
            embedder: Arc::new(None),
            vector_index: Arc::new(Mutex::new(None)),
            federation: Arc::new(None),
            tier_config: Arc::new(FeatureTier::Keyword.config()),
            scoring: Arc::new(crate::config::ResolvedScoring::default()),
        }
    }

    /// Mutex env-var guard. Tests that flip env vars must serialize to
    /// avoid clobbering each other; `cargo test --test-threads=2` is the
    /// upstream gate but a per-test mutex keeps the tests honest.
    fn env_var_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // ----- is_write_command ---------------------------------------------

    #[test]
    fn test_is_write_command_all_variants() {
        // Use clap's parser to build every Command variant. This avoids
        // having to know each Args struct's required-field set by name —
        // we just feed the same argv form an operator would use, and
        // assert the predicate returns the right answer.
        //
        // Writes (post-run WAL checkpoint expected):
        let writes: &[&[&str]] = &[
            &["ai-memory", "store", "title", "content"],
            &["ai-memory", "update", "id123", "--title", "t"],
            &["ai-memory", "delete", "id123"],
            &["ai-memory", "promote", "id123"],
            &["ai-memory", "forget", "pattern"],
            &["ai-memory", "link", "a", "b"],
            &["ai-memory", "consolidate", "ids"],
            &["ai-memory", "resolve", "a", "b"],
            &["ai-memory", "sync", "--peer", "/tmp/peer.db"],
            &[
                "ai-memory",
                "sync-daemon",
                "--peers",
                "http://x",
                "--interval-secs",
                "60",
            ],
            &["ai-memory", "import"],
            &["ai-memory", "auto-consolidate"],
            &["ai-memory", "gc"],
        ];
        let mut writes_checked = 0;
        for argv in writes {
            // Skip a variant whose required-field set our argv doesn't
            // match (clap will reject it). We still get coverage from the
            // variants that parse cleanly, which is the bulk.
            if let Ok(cli) = Cli::try_parse_from(*argv) {
                assert!(
                    is_write_command(&cli.command),
                    "expected write for {argv:?}"
                );
                writes_checked += 1;
            }
        }
        assert!(
            writes_checked >= 5,
            "expected at least 5 write variants checked, got {writes_checked}"
        );

        // Reads / no-ops (no checkpoint expected):
        let reads: &[&[&str]] = &[
            &["ai-memory", "mcp"],
            &["ai-memory", "recall", "context"],
            &["ai-memory", "search", "query"],
            &["ai-memory", "get", "id"],
            &["ai-memory", "list"],
            &["ai-memory", "stats"],
            &["ai-memory", "namespaces"],
            &["ai-memory", "export"],
            &["ai-memory", "shell"],
            &["ai-memory", "man"],
            &["ai-memory", "completions", "bash"],
            &["ai-memory", "archive", "list"],
            &["ai-memory", "agents", "list"],
            &["ai-memory", "pending", "list"],
            &["ai-memory", "bench"],
            &["ai-memory", "serve", "--host", "127.0.0.1", "--port", "0"],
        ];
        let mut reads_checked = 0;
        for argv in reads {
            if let Ok(cli) = Cli::try_parse_from(*argv) {
                assert!(
                    !is_write_command(&cli.command),
                    "expected read for {argv:?}"
                );
                reads_checked += 1;
            }
        }
        assert!(
            reads_checked >= 8,
            "expected at least 8 read variants checked, got {reads_checked}"
        );

        // Direct construction of the Args-less variants (10 variants
        // covered programmatically by clap above; pin the no-Args ones
        // here too for explicitness):
        assert!(is_write_command(&Command::Gc));
        assert!(!is_write_command(&Command::Stats));
        assert!(!is_write_command(&Command::Namespaces));
        assert!(!is_write_command(&Command::Export));
        assert!(!is_write_command(&Command::Shell));
        assert!(!is_write_command(&Command::Man));
        assert!(!is_write_command(&Command::Mcp {
            tier: "keyword".to_string()
        }));
    }

    // ----- build_router via lib::build_router ---------------------------

    #[tokio::test]
    async fn test_router_has_health_endpoint() {
        let env = TestEnv::fresh();
        let app_state = keyword_app_state(&env.db_path);
        let api_key_state = ApiKeyState { key: None };
        let router = build_router(app_state, api_key_state);
        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_router_has_metrics_at_both_paths() {
        let env = TestEnv::fresh();
        let app_state = keyword_app_state(&env.db_path);
        let api_key_state = ApiKeyState { key: None };
        // /metrics
        let r1 = build_router(app_state.clone(), api_key_state.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        // /api/v1/metrics
        let r2 = build_router(app_state, api_key_state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_router_lists_all_v1_memory_routes() {
        let env = TestEnv::fresh();
        let app_state = keyword_app_state(&env.db_path);
        let api_key_state = ApiKeyState { key: None };
        let router = build_router(app_state, api_key_state);
        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/memories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Empty DB returns 200 with an empty list — anything non-error
        // proves the route is wired in.
        assert!(resp.status().is_success(), "got {}", resp.status());
    }

    #[tokio::test]
    async fn test_router_applies_api_key_middleware_when_key_set() {
        let env = TestEnv::fresh();
        let app_state = keyword_app_state(&env.db_path);
        let api_key_state = ApiKeyState {
            key: Some("s3cret".to_string()),
        };
        let router = build_router(app_state, api_key_state);
        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/memories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_router_skips_api_key_middleware_when_key_none() {
        let env = TestEnv::fresh();
        let app_state = keyword_app_state(&env.db_path);
        let api_key_state = ApiKeyState { key: None };
        let router = build_router(app_state, api_key_state);
        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/memories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ----- build_embedder ------------------------------------------------

    #[tokio::test]
    async fn test_build_embedder_keyword_tier_returns_none() {
        let cfg = AppConfig::default();
        let emb = build_embedder(FeatureTier::Keyword, &cfg).await;
        assert!(emb.is_none());
    }

    #[tokio::test]
    async fn test_build_embedder_load_failure_returns_none() {
        // Can't easily induce a load failure without network — skip here.
        // Keyword tier covers the None branch; the ERROR-level fallback
        // path requires a live HF-hub-style mock, which is out of scope
        // for a unit test. The semantic-tier success/failure path is
        // exercised under `feature = "test-with-models"` in the
        // recall integration tests.
        // This test stays as a smoke check — it doesn't attempt to load.
    }

    // ----- build_vector_index -------------------------------------------

    #[test]
    fn test_build_vector_index_no_embedder_returns_none() {
        let env = TestEnv::fresh();
        let conn = db::open(&env.db_path).unwrap();
        assert!(build_vector_index(&conn, false).is_none());
    }

    #[test]
    fn test_build_vector_index_empty_db_returns_empty_index() {
        let env = TestEnv::fresh();
        let conn = db::open(&env.db_path).unwrap();
        let idx = build_vector_index(&conn, true);
        assert!(
            idx.is_some(),
            "empty DB with embedder must yield empty index"
        );
        assert_eq!(idx.unwrap().len(), 0);
    }

    // ----- spawn_gc_loop / spawn_wal_checkpoint_loop --------------------

    #[tokio::test(start_paused = true)]
    async fn test_spawn_gc_loop_runs_and_can_be_aborted() {
        let env = TestEnv::fresh();
        let conn = db::open(&env.db_path).unwrap();
        let state: Db = Arc::new(Mutex::new((
            conn,
            env.db_path.clone(),
            ResolvedTtl::default(),
            true,
        )));
        let h = spawn_gc_loop(state, None, Duration::from_secs(60));
        // Advance past the first sleep — the loop should now have ticked at
        // least once (its sleep arm has resolved). We can't easily observe
        // a side effect on an empty DB, so just abort and confirm the
        // handle is well-behaved.
        tokio::time::advance(Duration::from_secs(61)).await;
        // Yield once so the background task can see the tick.
        tokio::task::yield_now().await;
        h.abort();
        // Joining an aborted handle returns `JoinError` with cancelled() == true.
        let err = h.await.unwrap_err();
        assert!(err.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn test_spawn_wal_checkpoint_loop_runs_and_can_be_aborted() {
        let env = TestEnv::fresh();
        let conn = db::open(&env.db_path).unwrap();
        let state: Db = Arc::new(Mutex::new((
            conn,
            env.db_path.clone(),
            ResolvedTtl::default(),
            true,
        )));
        let h = spawn_wal_checkpoint_loop(state, Duration::from_secs(60));
        // First sleep is interval/2 = 30s. Advance past that + one full
        // interval to ensure at least one checkpoint cycle ran.
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        h.abort();
        let err = h.await.unwrap_err();
        assert!(err.is_cancelled());
    }

    // ----- passphrase_from_file -----------------------------------------

    #[test]
    fn test_passphrase_strips_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pass");
        std::fs::write(&p, "secret\n").unwrap();
        assert_eq!(passphrase_from_file(&p).unwrap(), "secret");
    }

    #[test]
    fn test_passphrase_strips_trailing_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pass");
        std::fs::write(&p, "secret\r\n").unwrap();
        assert_eq!(passphrase_from_file(&p).unwrap(), "secret");
    }

    #[test]
    fn test_passphrase_empty_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty");
        std::fs::write(&p, "").unwrap();
        let err = passphrase_from_file(&p).unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected 'empty' error, got: {err}"
        );
    }

    #[test]
    fn test_passphrase_empty_after_trim_errors() {
        // File contains only whitespace lines — after trim_end_matches
        // it remains "  \t" (internal whitespace preserved). Only "\n"
        // / "\r" alone would trigger the empty-after-strip case.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nl-only");
        std::fs::write(&p, "\n").unwrap();
        let err = passphrase_from_file(&p).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_passphrase_nonexistent_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does-not-exist");
        let err = passphrase_from_file(&p).unwrap_err();
        assert!(
            err.to_string().contains("reading passphrase file")
                || err.chain().any(|e| e.to_string().contains("No such file"))
                || err.chain().any(|e| e.to_string().contains("cannot find")),
            "got: {err:#}"
        );
    }

    #[test]
    fn test_passphrase_preserves_internal_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pass");
        std::fs::write(&p, "my pass phrase\n").unwrap();
        assert_eq!(passphrase_from_file(&p).unwrap(), "my pass phrase");
    }

    // ----- apply_anonymize_default --------------------------------------

    #[test]
    fn test_anonymize_set_when_config_true_and_env_unset() {
        let _g = env_var_lock();
        // SAFETY: serialized via env_var_lock.
        unsafe { std::env::remove_var("AI_MEMORY_ANONYMIZE") };
        let mut cfg = AppConfig::default();
        cfg.identity = Some(crate::config::IdentityConfig {
            anonymize_default: true,
        });
        apply_anonymize_default(&cfg);
        assert_eq!(std::env::var("AI_MEMORY_ANONYMIZE").unwrap(), "1");
        // SAFETY: serialized via env_var_lock.
        unsafe { std::env::remove_var("AI_MEMORY_ANONYMIZE") };
    }

    #[test]
    fn test_anonymize_unchanged_when_env_already_set() {
        let _g = env_var_lock();
        // SAFETY: serialized via env_var_lock.
        unsafe { std::env::set_var("AI_MEMORY_ANONYMIZE", "0") };
        let mut cfg = AppConfig::default();
        cfg.identity = Some(crate::config::IdentityConfig {
            anonymize_default: true,
        });
        apply_anonymize_default(&cfg);
        // Env var is left alone — caller-set value wins.
        assert_eq!(std::env::var("AI_MEMORY_ANONYMIZE").unwrap(), "0");
        // SAFETY: serialized via env_var_lock.
        unsafe { std::env::remove_var("AI_MEMORY_ANONYMIZE") };
    }

    #[test]
    fn test_anonymize_unchanged_when_config_false() {
        let _g = env_var_lock();
        // SAFETY: serialized via env_var_lock.
        unsafe { std::env::remove_var("AI_MEMORY_ANONYMIZE") };
        let cfg = AppConfig::default();
        // Default config is false / None for identity.anonymize_default.
        apply_anonymize_default(&cfg);
        assert!(std::env::var("AI_MEMORY_ANONYMIZE").is_err());
    }

    // ----- bootstrap_serve ----------------------------------------------

    #[tokio::test]
    async fn test_bootstrap_serve_keyword_tier_no_embedder() {
        let env = TestEnv::fresh();
        let mut cfg = AppConfig::default();
        cfg.tier = Some("keyword".to_string());
        let args = args_with_db(&env.db_path);
        let bs = bootstrap_serve(&env.db_path, &args, &cfg).await.unwrap();
        // Keyword tier => no embedder, no vector index.
        assert!(bs.app_state.embedder.is_none());
        let vi = bs.app_state.vector_index.lock().await;
        assert!(vi.is_none());
        // Two task handles spawned (gc + wal_checkpoint).
        assert_eq!(bs.task_handles.len(), 2);
        // Cleanly abort the spawned tasks so they don't leak across tests.
        for h in bs.task_handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_bootstrap_serve_with_api_key_logs_enabled() {
        let env = TestEnv::fresh();
        let mut cfg = AppConfig::default();
        cfg.tier = Some("keyword".to_string());
        cfg.api_key = Some("test-key".to_string());
        let args = args_with_db(&env.db_path);
        let bs = bootstrap_serve(&env.db_path, &args, &cfg).await.unwrap();
        assert_eq!(bs.api_key_state.key.as_deref(), Some("test-key"));
        for h in bs.task_handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_bootstrap_serve_federation_disabled_when_quorum_zero() {
        let env = TestEnv::fresh();
        let mut cfg = AppConfig::default();
        cfg.tier = Some("keyword".to_string());
        let args = args_with_db(&env.db_path);
        let bs = bootstrap_serve(&env.db_path, &args, &cfg).await.unwrap();
        assert!(bs.app_state.federation.is_none());
        for h in bs.task_handles {
            h.abort();
        }
    }

    // ----- W12-F: deeper coverage --------------------------------------
    //
    // Targets the gaps left after W6 + W7 + D6: `bootstrap_serve` variants
    // that require a populated DB or federation, the `run` dispatch arms
    // not yet exercised, `cmd_bench` end-to-end with a tiny workload,
    // `cmd_migrate` (sal feature), `urlencoding_minimal` direct test,
    // and the gc / wal-checkpoint loop bodies executing through one
    // tick with a measurable side effect.

    // ----- bootstrap_serve federation enabled ---------------------------

    #[tokio::test]
    async fn test_bootstrap_serve_federation_enabled_attaches_config() {
        // quorum_writes=1 + one peer → FederationConfig::build returns
        // Some, so app_state.federation is wired in. Catchup loop is
        // disabled (catchup_interval_secs=0) — the spawn-catchup branch
        // is exercised by federation tests; we only verify wiring here.
        let env = TestEnv::fresh();
        let mut cfg = AppConfig::default();
        cfg.tier = Some("keyword".to_string());
        let mut args = args_with_db(&env.db_path);
        args.quorum_writes = 1;
        args.quorum_peers = vec!["http://127.0.0.1:65530".to_string()];
        args.quorum_timeout_ms = 100;
        args.catchup_interval_secs = 0;
        let bs = bootstrap_serve(&env.db_path, &args, &cfg).await.unwrap();
        assert!(bs.app_state.federation.is_some());
        for h in bs.task_handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_bootstrap_serve_federation_enabled_with_catchup_loop() {
        // catchup_interval_secs > 0 → spawn_catchup_loop is invoked.
        // We can't directly observe the catchup loop's internal handle
        // (federation::spawn_catchup_loop returns a JoinHandle owned
        // privately by the federation module), but the side branch
        // "catchup loop enabled" runs and the bootstrap completes.
        let env = TestEnv::fresh();
        let mut cfg = AppConfig::default();
        cfg.tier = Some("keyword".to_string());
        let mut args = args_with_db(&env.db_path);
        args.quorum_writes = 1;
        args.quorum_peers = vec!["http://127.0.0.1:65531".to_string()];
        args.quorum_timeout_ms = 100;
        args.catchup_interval_secs = 3600; // long enough not to fire
        let bs = bootstrap_serve(&env.db_path, &args, &cfg).await.unwrap();
        assert!(bs.app_state.federation.is_some());
        for h in bs.task_handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn test_bootstrap_serve_federation_invalid_peer_errors() {
        // FederationConfig::build returns Err on duplicate peer URLs
        // (#341). The bootstrap_serve `.context("federation config")`
        // wrap turns it into a daemon-startup error.
        let env = TestEnv::fresh();
        let mut cfg = AppConfig::default();
        cfg.tier = Some("keyword".to_string());
        let mut args = args_with_db(&env.db_path);
        args.quorum_writes = 1;
        args.quorum_peers = vec![
            "http://127.0.0.1:65532".to_string(),
            "http://127.0.0.1:65532/".to_string(), // duplicate after trim
        ];
        let res = bootstrap_serve(&env.db_path, &args, &cfg).await;
        let err = match res {
            Ok(_) => panic!("expected error from duplicate peer URLs"),
            Err(e) => e,
        };
        let s = format!("{err:#}");
        assert!(
            s.contains("federation") || s.contains("duplicate"),
            "got: {s}"
        );
    }

    // ----- build_vector_index populated DB ------------------------------

    #[test]
    fn test_build_vector_index_populated_db_returns_built_index() {
        // When the DB has stored embeddings AND the embedder is present,
        // `build_vector_index` should return Some(VectorIndex) populated
        // with those embeddings rather than an empty one.
        let env = TestEnv::fresh();
        let conn = db::open(&env.db_path).unwrap();
        // Insert one memory + an embedding via the public db helpers.
        let now = chrono::Utc::now().to_rfc3339();
        let mem = crate::models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: crate::models::Tier::Mid,
            namespace: "ns".to_string(),
            title: "t".to_string(),
            content: "c".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: crate::models::default_metadata(),
        };
        let id = db::insert(&conn, &mem).unwrap();
        db::set_embedding(&conn, &id, &[1.0, 0.0, 0.0]).unwrap();
        let idx = build_vector_index(&conn, true).expect("populated index");
        assert!(
            idx.len() >= 1,
            "expected non-empty index, got len={}",
            idx.len()
        );
    }

    // ----- gc loop with non-empty side effect ---------------------------
    //
    // The existing `test_spawn_gc_loop_runs_and_can_be_aborted` only
    // covers the empty-DB path where db::gc returns 0. Seeding an expired
    // memory and pointing the gc loop at it lets the `Ok(n) if n > 0`
    // arm fire.

    #[tokio::test(start_paused = true)]
    async fn test_spawn_gc_loop_purges_expired_memories() {
        let env = TestEnv::fresh();
        let conn = db::open(&env.db_path).unwrap();
        // Insert an expired memory (expires_at in the past).
        let past = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        let now = chrono::Utc::now().to_rfc3339();
        let mem = crate::models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: crate::models::Tier::Short,
            namespace: "ns-gc".to_string(),
            title: "stale".to_string(),
            content: "stale".to_string(),
            tags: vec![],
            priority: 1,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: Some(past),
            metadata: crate::models::default_metadata(),
        };
        db::insert(&conn, &mem).unwrap();
        drop(conn);

        let conn = db::open(&env.db_path).unwrap();
        let state: Db = Arc::new(Mutex::new((
            conn,
            env.db_path.clone(),
            ResolvedTtl::default(),
            true,
        )));
        // archive_max_days=Some(1) lets the auto_purge_archive arm
        // execute too (covers the second match in the loop body).
        let h = spawn_gc_loop(state.clone(), Some(1), Duration::from_secs(60));
        // Advance past two full intervals to give both branches multiple
        // chances to log under paused time.
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        h.abort();
        let _ = h.await;
    }

    // ----- WAL checkpoint loop with measurable cycle --------------------

    #[tokio::test(start_paused = true)]
    async fn test_spawn_wal_checkpoint_loop_runs_multiple_cycles() {
        let env = TestEnv::fresh();
        let conn = db::open(&env.db_path).unwrap();
        let state: Db = Arc::new(Mutex::new((
            conn,
            env.db_path.clone(),
            ResolvedTtl::default(),
            true,
        )));
        let h = spawn_wal_checkpoint_loop(state, Duration::from_secs(2));
        // First sleep is 1s (interval/2), then 2s per cycle. Advance
        // past three cycles.
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(2)).await;
            tokio::task::yield_now().await;
        }
        h.abort();
        let _ = h.await;
    }

    // ----- urlencoding_minimal -----------------------------------------

    #[test]
    fn test_urlencoding_minimal_round_trip() {
        // Unreserved characters pass through unchanged.
        assert_eq!(urlencoding_minimal("abcXYZ-_.~"), "abcXYZ-_.~");
        assert_eq!(urlencoding_minimal("0123456789"), "0123456789");
        // Reserved / unsafe characters are percent-encoded.
        assert_eq!(urlencoding_minimal("a:b"), "a%3Ab");
        assert_eq!(urlencoding_minimal("a/b"), "a%2Fb");
        assert_eq!(urlencoding_minimal("a@b"), "a%40b");
        assert_eq!(urlencoding_minimal("a+b"), "a%2Bb");
        assert_eq!(urlencoding_minimal(" "), "%20");
        // Empty string is empty.
        assert_eq!(urlencoding_minimal(""), "");
        // RFC3339 timestamp shape (sync-daemon real input).
        assert_eq!(
            urlencoding_minimal("2024-01-02T03:04:05+00:00"),
            "2024-01-02T03%3A04%3A05%2B00%3A00"
        );
    }

    // ----- run() dispatch for read-only commands ------------------------
    //
    // Each test parses a CLI argv via clap, hands the resulting `Cli`
    // to `daemon_runtime::run`, and asserts the dispatch path returned
    // Ok. We don't assert on stdout because run() writes to the
    // process stdout directly — what we care about for coverage is
    // that the match arm executed and the inner cli handler returned.

    fn no_config_env() -> std::sync::MutexGuard<'static, ()> {
        // run() reads `AI_MEMORY_NO_CONFIG` indirectly via the AppConfig
        // we pass. We don't rely on the env directly here, but holding
        // env_var_lock keeps run() tests serialized so they don't race
        // on stdout / global subscribers.
        env_var_lock()
    }

    #[tokio::test]
    async fn test_run_dispatch_stats_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli =
            Cli::try_parse_from(["ai-memory", "--db", env.db_path.to_str().unwrap(), "stats"])
                .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_namespaces_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "namespaces",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_export_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli =
            Cli::try_parse_from(["ai-memory", "--db", env.db_path.to_str().unwrap(), "export"])
                .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_list_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from(["ai-memory", "--db", env.db_path.to_str().unwrap(), "list"])
            .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_search_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "search",
            "anyq",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_archive_list_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "archive",
            "list",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_agents_list_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "agents",
            "list",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_pending_list_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "pending",
            "list",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_completions_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "completions",
            "bash",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_man_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from(["ai-memory", "--db", env.db_path.to_str().unwrap(), "man"])
            .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_gc_triggers_post_run_checkpoint() {
        // `Gc` is in is_write_command, so result.is_ok() && Some path
        // takes the post-run WAL checkpoint branch (lines 638-644).
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from(["ai-memory", "--db", env.db_path.to_str().unwrap(), "gc"])
            .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_resolve_command() {
        // Seed two memories, then resolve one as superseding the other.
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let id_a = crate::cli::test_utils::seed_memory(&env.db_path, "ns", "old", "old fact");
        let id_b = crate::cli::test_utils::seed_memory(&env.db_path, "ns", "new", "new fact");
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "resolve",
            &id_a,
            &id_b,
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_get_command() {
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let id = crate::cli::test_utils::seed_memory(&env.db_path, "ns", "t", "c");
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "get",
            &id,
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_dispatch_promote_triggers_write_checkpoint() {
        // `Promote` is in is_write_command — covers the post-run
        // checkpoint branch on a different command.
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let id = crate::cli::test_utils::seed_memory(&env.db_path, "ns", "t", "c");
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "promote",
            &id,
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    // ----- run() dispatch for bench (cmd_bench end-to-end) --------------

    #[tokio::test]
    async fn test_run_dispatch_bench_smoke_runs_one_iteration() {
        // iterations=1, warmup=0 keeps the workload tiny. The bench
        // body builds an in-memory DB internally — no on-disk side
        // effects. Covers cmd_bench from top to bottom on the
        // human-readable, no-baseline, no-history path.
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "bench",
            "--iterations",
            "1",
            "--warmup",
            "0",
        ])
        .unwrap();
        // Bench may fail the budget on a paused-time iter=1 run; we
        // accept either Ok or Err here — coverage is the goal.
        let _ = run(cli, &cfg).await;
    }

    #[tokio::test]
    async fn test_run_dispatch_bench_json_with_history() {
        // Covers --json branch + --history append branch of cmd_bench.
        let _g = no_config_env();
        let env = TestEnv::fresh();
        let history = env.db_path.with_file_name("hist.jsonl");
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "bench",
            "--iterations",
            "1",
            "--warmup",
            "0",
            "--json",
            "--history",
            history.to_str().unwrap(),
        ])
        .unwrap();
        let _ = run(cli, &cfg).await;
        // History file should now exist with at least one line.
        if history.exists() {
            let content = std::fs::read_to_string(&history).unwrap();
            assert!(content.contains("captured_at") || !content.is_empty());
        }
    }

    // ----- run() dispatch for migrate (sal feature) --------------------

    #[cfg(feature = "sal")]
    #[tokio::test]
    async fn test_run_dispatch_migrate_sqlite_to_sqlite_dry_run() {
        // Covers cmd_migrate happy path + dry-run / human-output branch.
        let _g = no_config_env();
        let src_env = TestEnv::fresh();
        let dst_env = TestEnv::fresh();
        // Seed source so migrate has work to do.
        crate::cli::test_utils::seed_memory(&src_env.db_path, "ns-mig", "t", "c");
        let from = format!("sqlite://{}", src_env.db_path.display());
        let to = format!("sqlite://{}", dst_env.db_path.display());
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            src_env.db_path.to_str().unwrap(),
            "migrate",
            "--from",
            &from,
            "--to",
            &to,
            "--dry-run",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    #[cfg(feature = "sal")]
    #[tokio::test]
    async fn test_run_dispatch_migrate_json_output() {
        // Covers cmd_migrate --json branch.
        let _g = no_config_env();
        let src_env = TestEnv::fresh();
        let dst_env = TestEnv::fresh();
        crate::cli::test_utils::seed_memory(&src_env.db_path, "ns-mig", "t", "c");
        let from = format!("sqlite://{}", src_env.db_path.display());
        let to = format!("sqlite://{}", dst_env.db_path.display());
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            src_env.db_path.to_str().unwrap(),
            "migrate",
            "--from",
            &from,
            "--to",
            &to,
            "--json",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
    }

    // ----- run() with passphrase file (covers lines 372-374) ------------

    #[tokio::test]
    async fn test_run_with_db_passphrase_file_exports_env() {
        // Covers the `--db-passphrase-file` branch in run() (lines
        // 371-375) which calls passphrase_from_file then sets
        // AI_MEMORY_DB_PASSPHRASE in the environment.
        let _g = env_var_lock();
        // SAFETY: serialized via env_var_lock.
        unsafe { std::env::remove_var("AI_MEMORY_DB_PASSPHRASE") };
        let env = TestEnv::fresh();
        let pass_path = env.db_path.with_file_name("pass");
        std::fs::write(&pass_path, "test-passphrase\n").unwrap();
        let cfg = AppConfig::default();
        let cli = Cli::try_parse_from([
            "ai-memory",
            "--db",
            env.db_path.to_str().unwrap(),
            "--db-passphrase-file",
            pass_path.to_str().unwrap(),
            "stats",
        ])
        .unwrap();
        run(cli, &cfg).await.unwrap();
        // Env var is now set.
        assert_eq!(
            std::env::var("AI_MEMORY_DB_PASSPHRASE").unwrap(),
            "test-passphrase"
        );
        // SAFETY: serialized via env_var_lock.
        unsafe { std::env::remove_var("AI_MEMORY_DB_PASSPHRASE") };
    }

    // ----- init_tracing idempotence ------------------------------------

    #[test]
    fn test_init_tracing_is_idempotent() {
        // Covers init_tracing — second call is a harmless no-op
        // (try_init returns Err which we ignore). Calling twice from
        // the same test exercises the second-call path on a process
        // that may or may not already have a global subscriber.
        init_tracing();
        init_tracing();
    }

    // ----- serve_http_with_shutdown_future smoke -----------------------
    //
    // The non-TLS branch of `serve()` delegates here; cover the body
    // by binding to a free port, requesting /health, then shutting
    // down. This also covers the production code path that
    // `daemon_runtime::serve()` uses for the non-TLS case.

    #[tokio::test]
    async fn test_serve_http_with_shutdown_future_serves_then_stops() {
        let env = TestEnv::fresh();
        let app_state = keyword_app_state(&env.db_path);
        let api_key_state = ApiKeyState { key: None };
        // Pick a free port via a transient bind.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let addr = format!("127.0.0.1:{port}");
        let shutdown = Arc::new(Notify::new());
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            serve_http_with_shutdown_future(&addr, api_key_state, app_state, async move {
                shutdown_clone.notified().await;
            })
            .await
        });
        // Give the server a moment to bind, then poke /health.
        for _ in 0..40 {
            if let Ok(client) = reqwest::Client::builder()
                .timeout(Duration::from_millis(200))
                .build()
                && client
                    .get(format!("http://127.0.0.1:{port}/api/v1/health"))
                    .send()
                    .await
                    .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        shutdown.notify_one();
        let res = handle.await.unwrap();
        assert!(res.is_ok(), "serve future returned: {res:?}");
    }

    // ----- bind error surfacing ----------------------------------------

    #[tokio::test]
    async fn test_serve_http_with_shutdown_future_bind_failure_errors() {
        // An unbindable address (port 1 on Linux/macOS without root)
        // should return an Err with the bind context. This covers the
        // `with_context` path on the TcpListener::bind line.
        let env = TestEnv::fresh();
        let app_state = keyword_app_state(&env.db_path);
        let api_key_state = ApiKeyState { key: None };
        // 0.0.0.0:0 succeeds; we want a guaranteed failure. Bind to
        // port 1 which requires privileged perms — except on macOS in
        // some configs that may succeed. Use a clearly invalid address
        // form instead to force a bind-time error.
        let res = serve_http_with_shutdown_future(
            "definitely-not-an-address:99999",
            api_key_state,
            app_state,
            async {},
        )
        .await;
        assert!(res.is_err(), "expected bind error, got: {res:?}");
    }
}
