// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

mod color;
mod config;
mod db;
mod embeddings;
mod errors;
mod handlers;
mod hnsw;
mod identity;
mod llm;
mod mcp;
mod mine;
mod models;
mod reranker;
mod toon;
mod validate;

use anyhow::Result;
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
        /// Agent type (ai:claude-opus-4.6, ai:claude-opus-4.7, ai:codex-5.4, ai:grok-4.2, human, system)
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
        Command::Delete(a) => cmd_delete(&db_path, &a, j),
        Command::Promote(a) => cmd_promote(&db_path, &a, j),
        Command::Forget(a) => cmd_forget(&db_path, &a, j),
        Command::Link(a) => cmd_link(&db_path, &a, j),
        Command::Consolidate(a) => cmd_consolidate(&db_path, a, j, cli_agent_id.as_deref()),
        Command::Resolve(a) => cmd_resolve(&db_path, &a, j),
        Command::Shell => cmd_shell(&db_path),
        Command::Sync(a) => cmd_sync(&db_path, &a, j, cli_agent_id.as_deref()),
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
    let state: handlers::Db = Arc::new(Mutex::new((
        conn,
        db_path.clone(),
        resolved_ttl,
        archive_on_gc,
    )));

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
        .route("/api/v1/links", post(handlers::create_link))
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
        .layer(axum::middleware::from_fn_with_state(
            api_key_state,
            handlers::api_key_auth,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024)) // 2MB default (bulk/import bodies capped at MAX_BULK_SIZE * per-memory limit)
        .layer(CorsLayer::new())
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("ai-memory listening on {addr}");
    tracing::info!("database: {}", db_path.display());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

// --- CLI ---

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
        obj.insert("agent_id".to_string(), serde_json::Value::String(agent_id));
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

    // Perform recall: hybrid if embedder available, keyword otherwise
    let (results, mode) = if let Some(ref emb) = embedder {
        match emb.embed(&args.context) {
            Ok(query_emb) => {
                let results = db::recall_hybrid(
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
                )?;
                if let Some(ref ce) = reranker {
                    (ce.rerank(&args.context, results), "hybrid+rerank")
                } else {
                    (results, "hybrid")
                }
            }
            Err(e) => {
                eprintln!("ai-memory: embedding query failed: {e}, falling back to keyword");
                let results = db::recall(
                    &conn,
                    &args.context,
                    args.namespace.as_deref(),
                    args.limit,
                    args.tags.as_deref(),
                    args.since.as_deref(),
                    args.until.as_deref(),
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                )?;
                (results, "keyword")
            }
        }
    } else {
        let results = db::recall(
            &conn,
            &args.context,
            args.namespace.as_deref(),
            args.limit,
            args.tags.as_deref(),
            args.since.as_deref(),
            args.until.as_deref(),
            resolved_ttl.short_extend_secs,
            resolved_ttl.mid_extend_secs,
        )?;
        (results, "keyword")
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
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"memories": scored, "count": results.len(), "mode": mode})
            )?
        );
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

fn cmd_delete(db_path: &Path, args: &DeleteArgs, json_out: bool) -> Result<()> {
    validate::validate_id(&args.id)?;
    let conn = db::open(db_path)?;
    // Try exact delete first; if not found, resolve prefix to get the full ID
    if db::delete(&conn, &args.id)? {
        if json_out {
            println!("{}", serde_json::json!({"deleted": true, "id": args.id}));
        } else {
            println!("deleted: {}", args.id);
        }
    } else if let Some(mem) = db::get_by_prefix(&conn, &args.id)? {
        let full_id = mem.id.clone();
        if db::delete(&conn, &full_id)? {
            if json_out {
                println!("{}", serde_json::json!({"deleted": true, "id": full_id}));
            } else {
                println!("deleted: {full_id}");
            }
        }
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_promote(db_path: &Path, args: &PromoteArgs, json_out: bool) -> Result<()> {
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
                ) {
                    Ok(results) => {
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
                match db::search(&conn, &q, None, None, 20, None, None, None, None, None) {
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
