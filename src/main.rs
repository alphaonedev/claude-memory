mod db;
mod errors;
mod handlers;
mod mcp;
mod models;
mod validate;

use anyhow::Result;
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use chrono::{Duration, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::models::Tier;

const DEFAULT_DB: &str = "claude-memory.db";
const DEFAULT_PORT: u16 = 9077;
const GC_INTERVAL_SECS: u64 = 1800;

#[derive(Parser)]
#[command(
    name = "claude-memory",
    about = "Persistent memory daemon for Claude Code — short, mid, and long-term recall"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[arg(long, env = "CLAUDE_MEMORY_DB", default_value = DEFAULT_DB, global = true)]
    db: PathBuf,
    /// Output as JSON (machine-parseable)
    #[arg(long, global = true, default_value_t = false)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP memory daemon
    Serve(ServeArgs),
    /// Run as an MCP (Model Context Protocol) tool server over stdio
    Mcp,
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
    Import,
    /// Generate shell completions
    Completions(CompletionsArgs),
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
    #[arg(long, short = 'T')]
    title: String,
    /// Content (use - to read from stdin)
    #[arg(long, short)]
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
}

#[derive(Args)]
struct UpdateArgs {
    id: String,
    #[arg(long, short = 'T')]
    title: Option<String>,
    #[arg(long, short)]
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
}

#[derive(Args)]
struct RecallArgs {
    context: String,
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long, default_value_t = 10)]
    limit: usize,
    #[arg(long)]
    tags: Option<String>,
    #[arg(long)]
    since: Option<String>,
}

#[derive(Args)]
struct SearchArgs {
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
    #[arg(long, short = 'T')]
    title: String,
    #[arg(long, short = 's')]
    summary: String,
    #[arg(long, short)]
    namespace: Option<String>,
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
    let cli = Cli::parse();
    let j = cli.json;
    match cli.command {
        Command::Serve(a) => serve(cli.db, a).await,
        Command::Mcp => {
            mcp::run_mcp_server(&cli.db)?;
            Ok(())
        }
        Command::Store(a) => cmd_store(cli.db, a, j),
        Command::Update(a) => cmd_update(cli.db, a, j),
        Command::Recall(a) => cmd_recall(cli.db, a, j),
        Command::Search(a) => cmd_search(cli.db, a, j),
        Command::Get(a) => cmd_get(cli.db, a, j),
        Command::List(a) => cmd_list(cli.db, a, j),
        Command::Delete(a) => cmd_delete(cli.db, a, j),
        Command::Promote(a) => cmd_promote(cli.db, a, j),
        Command::Forget(a) => cmd_forget(cli.db, a, j),
        Command::Link(a) => cmd_link(cli.db, a, j),
        Command::Consolidate(a) => cmd_consolidate(cli.db, a, j),
        Command::Gc => cmd_gc(cli.db, j),
        Command::Stats => cmd_stats(cli.db, j),
        Command::Namespaces => cmd_namespaces(cli.db, j),
        Command::Export => cmd_export(cli.db),
        Command::Import => cmd_import(cli.db, j),
        Command::Completions(a) => {
            generate(
                a.shell,
                &mut Cli::command(),
                "claude-memory",
                &mut std::io::stdout(),
            );
            Ok(())
        }
    }
}

async fn serve(db_path: PathBuf, args: ServeArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("claude_memory=info".parse()?)
                .add_directive("tower_http=info".parse()?),
        )
        .init();

    let conn = db::open(&db_path)?;
    let state: handlers::Db = Arc::new(Mutex::new((conn, db_path.clone())));

    // Automatic GC
    let gc_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(GC_INTERVAL_SECS)).await;
            let lock = gc_state.lock().await;
            match db::gc(&lock.0) {
                Ok(n) if n > 0 => tracing::info!("gc: expired {n} memories"),
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

    let app = Router::new()
        .route("/api/v1/health", get(handlers::health))
        .route("/api/v1/memories", get(handlers::list_memories))
        .route("/api/v1/memories", post(handlers::create_memory))
        .route("/api/v1/memories/bulk", post(handlers::bulk_create))
        .route("/api/v1/memories/{id}", get(handlers::get_memory))
        .route("/api/v1/memories/{id}", put(handlers::update_memory))
        .route("/api/v1/memories/{id}", delete(handlers::delete_memory))
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
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("claude-memory listening on {addr}");
    tracing::info!("database: {}", db_path.display());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

// --- CLI ---

fn cmd_store(db_path: PathBuf, args: StoreArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
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

    let now = Utc::now();
    let expires_at = tier
        .default_ttl_secs()
        .map(|s| (now + Duration::seconds(s)).to_rfc3339());
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
    };
    let contradictions =
        db::find_contradictions(&conn, &mem.title, &mem.namespace).unwrap_or_default();
    let actual_id = db::insert(&conn, &mem)?;
    if json_out {
        let mut j = serde_json::to_value(&mem)?;
        j["id"] = serde_json::json!(actual_id);
        if !contradictions.is_empty() {
            j["potential_contradictions"] =
                serde_json::json!(contradictions.iter().map(|c| &c.id).collect::<Vec<_>>());
        }
        println!("{}", serde_json::to_string(&j)?);
    } else {
        println!(
            "stored: {} [{}] (ns={})",
            actual_id, mem.tier, mem.namespace
        );
        if !contradictions.is_empty() {
            eprintln!(
                "warning: {} similar memories found in same namespace (potential contradictions)",
                contradictions.len()
            );
        }
    }
    Ok(())
}

fn cmd_update(db_path: PathBuf, args: UpdateArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
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
    let updated = db::update(
        &conn,
        &args.id,
        args.title.as_deref(),
        args.content.as_deref(),
        tier.as_ref(),
        args.namespace.as_deref(),
        tags.as_ref(),
        args.priority,
        args.confidence,
        None,
    )?;
    if !updated {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    if let Some(mem) = db::get(&conn, &args.id)? {
        if json_out {
            println!("{}", serde_json::to_string(&mem)?);
        } else {
            println!("updated: {} [{}]", mem.id, mem.title);
        }
    }
    Ok(())
}

fn cmd_recall(db_path: PathBuf, args: RecallArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let results = db::recall(
        &conn,
        &args.context,
        args.namespace.as_deref(),
        args.limit,
        args.tags.as_deref(),
        args.since.as_deref(),
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
        eprintln!("no memories found for: {}", args.context);
        return Ok(());
    }
    for mem in &results {
        let age = human_age(&mem.updated_at);
        let conf = if mem.confidence < 1.0 {
            format!(" conf={:.0}%", mem.confidence * 100.0)
        } else {
            String::new()
        };
        println!(
            "[{}/{}] {} (p={}, ns={}, {}x, {}{})",
            mem.tier,
            &mem.id[..8],
            mem.title,
            mem.priority,
            mem.namespace,
            mem.access_count,
            age,
            conf
        );
        let preview: String = mem.content.chars().take(200).collect();
        println!("  {}\n", preview);
    }
    println!("{} memory(ies) recalled", results.len());
    Ok(())
}

fn cmd_search(db_path: PathBuf, args: SearchArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
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
            &mem.id[..8],
            mem.title,
            mem.priority,
            mem.namespace,
            age
        );
    }
    println!("\n{} result(s)", results.len());
    Ok(())
}

fn cmd_get(db_path: PathBuf, args: GetArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    match db::get(&conn, &args.id)? {
        Some(mem) => {
            let links = db::get_links(&conn, &args.id).unwrap_or_default();
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
        }
        None => {
            eprintln!("not found: {}", args.id);
            std::process::exit(1);
        }
    }
    Ok(())
}

fn cmd_list(db_path: PathBuf, args: ListArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::list(
        &conn,
        args.namespace.as_deref(),
        tier.as_ref(),
        args.limit,
        0,
        None,
        args.since.as_deref(),
        args.until.as_deref(),
        args.tags.as_deref(),
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
            &mem.id[..8],
            mem.title,
            mem.priority,
            mem.namespace,
            age
        );
    }
    println!("\n{} memory(ies)", results.len());
    Ok(())
}

fn cmd_delete(db_path: PathBuf, args: DeleteArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    if db::delete(&conn, &args.id)? {
        if json_out {
            println!("{}", serde_json::json!({"deleted": true, "id": args.id}));
        } else {
            println!("deleted: {}", args.id);
        }
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_promote(db_path: PathBuf, args: PromoteArgs, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let updated = db::update(
        &conn,
        &args.id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        Some(""),
    )?;
    if !updated {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    // Clear expires_at for long-term
    conn.execute(
        "UPDATE memories SET expires_at = NULL WHERE id = ?1",
        rusqlite::params![args.id],
    )?;
    if json_out {
        println!(
            "{}",
            serde_json::json!({"promoted": true, "id": args.id, "tier": "long"})
        );
    } else {
        println!("promoted to long-term: {}", args.id);
    }
    Ok(())
}

fn cmd_forget(db_path: PathBuf, args: ForgetArgs, json_out: bool) -> Result<()> {
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let conn = db::open(&db_path)?;
    match db::forget(
        &conn,
        args.namespace.as_deref(),
        args.pattern.as_deref(),
        tier.as_ref(),
    ) {
        Ok(n) => {
            if json_out {
                println!("{}", serde_json::json!({"deleted": n}));
            } else {
                println!("forgot {} memories", n);
            }
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
    Ok(())
}

fn cmd_link(db_path: PathBuf, args: LinkArgs, json_out: bool) -> Result<()> {
    validate::validate_link(&args.source_id, &args.target_id, &args.relation)?;
    let conn = db::open(&db_path)?;
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

fn cmd_consolidate(db_path: PathBuf, args: ConsolidateArgs, json_out: bool) -> Result<()> {
    let ids: Vec<String> = args
        .ids
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let namespace = args.namespace.unwrap_or_else(auto_namespace);
    validate::validate_consolidate(&ids, &args.title, &args.summary, &namespace)?;
    let conn = db::open(&db_path)?;
    let new_id = db::consolidate(
        &conn,
        &ids,
        &args.title,
        &args.summary,
        &namespace,
        &Tier::Long,
        "cli",
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

fn cmd_gc(db_path: PathBuf, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let count = db::gc(&conn)?;
    if json_out {
        println!("{}", serde_json::json!({"expired_deleted": count}));
    } else {
        println!("expired memories deleted: {}", count);
    }
    Ok(())
}

fn cmd_stats(db_path: PathBuf, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let stats = db::stats(&conn, &db_path)?;
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

fn cmd_namespaces(db_path: PathBuf, json_out: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
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

fn cmd_export(db_path: PathBuf) -> Result<()> {
    let conn = db::open(&db_path)?;
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

fn cmd_import(db_path: PathBuf, json_out: bool) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let data: serde_json::Value = serde_json::from_str(&buf)?;
    let memories: Vec<models::Memory> =
        serde_json::from_value(data.get("memories").cloned().unwrap_or_default())?;
    let links: Vec<models::MemoryLink> =
        serde_json::from_value(data.get("links").cloned().unwrap_or_default()).unwrap_or_default();
    let conn = db::open(&db_path)?;
    let mut imported = 0usize;
    let mut errors = Vec::new();
    for mem in memories {
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
            serde_json::json!({"imported": imported, "errors": errors})
        );
    } else {
        println!("imported: {}", imported);
        if !errors.is_empty() {
            for e in &errors {
                eprintln!("  {}", e);
            }
        }
    }
    Ok(())
}
