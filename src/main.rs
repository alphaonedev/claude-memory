mod db;
mod handlers;
mod models;

use anyhow::Result;
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use chrono::{Duration, Utc};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::models::{Tier, MAX_CONTENT_SIZE};

const DEFAULT_DB: &str = "claude-memory.db";
const DEFAULT_PORT: u16 = 9077;
const GC_INTERVAL_SECS: u64 = 1800; // 30 minutes

#[derive(Parser)]
#[command(name = "claude-memory", about = "Persistent memory daemon for Claude Code — short, mid, and long-term recall")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to SQLite database
    #[arg(long, env = "CLAUDE_MEMORY_DB", default_value = DEFAULT_DB, global = true)]
    db: PathBuf,

    /// Output as JSON (for machine consumption)
    #[arg(long, global = true, default_value_t = false)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP memory daemon
    Serve(ServeArgs),
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
    /// Delete a memory
    Delete(DeleteArgs),
    /// Run garbage collection on expired memories
    Gc,
    /// Show statistics
    Stats,
    /// List all namespaces
    Namespaces,
    /// Export all memories as JSON
    Export,
    /// Import memories from JSON (stdin)
    Import,
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
    /// Memory tier: short, mid, long
    #[arg(long, short, default_value = "mid")]
    tier: String,
    /// Namespace (project name, repo, topic)
    #[arg(long, short, default_value = "global")]
    namespace: String,
    /// Title
    #[arg(long, short = 'T')]
    title: String,
    /// Content (use - to read from stdin)
    #[arg(long, short)]
    content: String,
    /// Comma-separated tags
    #[arg(long, default_value = "")]
    tags: String,
    /// Priority (1-10)
    #[arg(long, short, default_value_t = 5)]
    priority: i32,
}

#[derive(Args)]
struct UpdateArgs {
    /// Memory ID
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
}

#[derive(Args)]
struct RecallArgs {
    /// What are you trying to remember?
    context: String,
    /// Namespace filter
    #[arg(long, short)]
    namespace: Option<String>,
    /// Max results
    #[arg(long, default_value_t = 10)]
    limit: usize,
}

#[derive(Args)]
struct SearchArgs {
    /// Search query
    query: String,
    #[arg(long, short)]
    namespace: Option<String>,
    #[arg(long, short)]
    tier: Option<String>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
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
}

#[derive(Args)]
struct DeleteArgs {
    id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let json_output = cli.json;

    match cli.command {
        Command::Serve(args) => serve(cli.db, args).await,
        Command::Store(args) => cmd_store(cli.db, args, json_output),
        Command::Update(args) => cmd_update(cli.db, args, json_output),
        Command::Recall(args) => cmd_recall(cli.db, args, json_output),
        Command::Search(args) => cmd_search(cli.db, args, json_output),
        Command::Get(args) => cmd_get(cli.db, args, json_output),
        Command::List(args) => cmd_list(cli.db, args, json_output),
        Command::Delete(args) => cmd_delete(cli.db, args, json_output),
        Command::Gc => cmd_gc(cli.db, json_output),
        Command::Stats => cmd_stats(cli.db, json_output),
        Command::Namespaces => cmd_namespaces(cli.db, json_output),
        Command::Export => cmd_export(cli.db),
        Command::Import => cmd_import(cli.db, json_output),
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

    // Spawn automatic GC task
    let gc_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(GC_INTERVAL_SECS)).await;
            let lock = gc_state.lock().await;
            match db::gc(&lock.0) {
                Ok(n) if n > 0 => tracing::info!("gc: expired {n} memories"),
                Ok(_) => {}
                Err(e) => tracing::warn!("gc error: {e}"),
            }
        }
    });

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
    tracing::info!("automatic gc every {}s", GC_INTERVAL_SECS);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// --- CLI commands ---

fn cmd_store(db_path: PathBuf, args: StoreArgs, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = Tier::from_str(&args.tier)
        .ok_or_else(|| anyhow::anyhow!("invalid tier: {} (use short, mid, long)", args.tier))?;

    let content = if args.content == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        args.content
    };

    if content.len() > MAX_CONTENT_SIZE {
        anyhow::bail!("content exceeds max size of {} bytes", MAX_CONTENT_SIZE);
    }

    let tags: Vec<String> = args.tags.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let now = Utc::now();
    let expires_at = tier.default_ttl_secs().map(|secs| (now + Duration::seconds(secs)).to_rfc3339());
    let mem = models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace: args.namespace,
        title: args.title,
        content,
        tags,
        priority: args.priority.clamp(1, 10),
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
    };
    db::insert(&conn, &mem)?;
    if json_output {
        println!("{}", serde_json::to_string(&mem)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&mem)?);
    }
    Ok(())
}

fn cmd_update(db_path: PathBuf, args: UpdateArgs, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let tags: Option<Vec<String>> = args.tags.as_ref().map(|t| {
        t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    });

    if let Some(ref c) = args.content {
        if c.len() > MAX_CONTENT_SIZE {
            anyhow::bail!("content exceeds max size of {} bytes", MAX_CONTENT_SIZE);
        }
    }

    let updated = db::update(
        &conn, &args.id,
        args.title.as_deref(), args.content.as_deref(),
        tier.as_ref(), args.namespace.as_deref(),
        tags.as_ref(), args.priority, None,
    )?;
    if !updated {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    let mem = db::get(&conn, &args.id)?;
    if let Some(mem) = mem {
        if json_output {
            println!("{}", serde_json::to_string(&mem)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&mem)?);
        }
    }
    Ok(())
}

fn cmd_recall(db_path: PathBuf, args: RecallArgs, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let results = db::recall(&conn, &args.context, args.namespace.as_deref(), args.limit)?;
    if json_output {
        println!("{}", serde_json::to_string(&serde_json::json!({"memories": results, "count": results.len()}))?);
        return Ok(());
    }
    if results.is_empty() {
        eprintln!("no memories found for: {}", args.context);
    } else {
        for mem in &results {
            println!("[{}/{}] {} (p={}, ns={}, accessed={}x)",
                mem.tier, mem.id, mem.title, mem.priority, mem.namespace, mem.access_count);
            let preview: String = mem.content.chars().take(200).collect();
            println!("  {}", preview);
            println!();
        }
        println!("{} memory(ies) recalled", results.len());
    }
    Ok(())
}

fn cmd_search(db_path: PathBuf, args: SearchArgs, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::search(&conn, &args.query, args.namespace.as_deref(), tier.as_ref(), args.limit, None)?;
    if json_output {
        println!("{}", serde_json::to_string(&serde_json::json!({"results": results, "count": results.len()}))?);
        return Ok(());
    }
    if results.is_empty() {
        eprintln!("no results for: {}", args.query);
    } else {
        for mem in &results {
            println!("[{}/{}] {} (p={}, ns={})", mem.tier, mem.id, mem.title, mem.priority, mem.namespace);
        }
        println!("\n{} result(s)", results.len());
    }
    Ok(())
}

fn cmd_get(db_path: PathBuf, args: GetArgs, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    match db::get(&conn, &args.id)? {
        Some(mem) => {
            if json_output {
                println!("{}", serde_json::to_string(&mem)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&mem)?);
            }
        }
        None => { eprintln!("not found: {}", args.id); std::process::exit(1); }
    }
    Ok(())
}

fn cmd_list(db_path: PathBuf, args: ListArgs, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::list(&conn, args.namespace.as_deref(), tier.as_ref(), args.limit, 0, None)?;
    if json_output {
        println!("{}", serde_json::to_string(&serde_json::json!({"memories": results, "count": results.len()}))?);
        return Ok(());
    }
    if results.is_empty() {
        eprintln!("no memories stored");
    } else {
        for mem in &results {
            println!("[{}/{}] {} (p={}, ns={})", mem.tier, mem.id, mem.title, mem.priority, mem.namespace);
        }
        println!("\n{} memory(ies)", results.len());
    }
    Ok(())
}

fn cmd_delete(db_path: PathBuf, args: DeleteArgs, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    if db::delete(&conn, &args.id)? {
        if json_output {
            println!("{}", serde_json::to_string(&serde_json::json!({"deleted": true, "id": args.id}))?);
        } else {
            println!("deleted: {}", args.id);
        }
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_gc(db_path: PathBuf, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let count = db::gc(&conn)?;
    if json_output {
        println!("{}", serde_json::to_string(&serde_json::json!({"expired_deleted": count}))?);
    } else {
        println!("expired memories deleted: {}", count);
    }
    Ok(())
}

fn cmd_stats(db_path: PathBuf, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let stats = db::stats(&conn, &db_path)?;
    if json_output {
        println!("{}", serde_json::to_string(&stats)?);
        return Ok(());
    }
    println!("total memories: {}", stats.total);
    println!("expiring within 1h: {}", stats.expiring_soon);
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

fn cmd_namespaces(db_path: PathBuf, json_output: bool) -> Result<()> {
    let conn = db::open(&db_path)?;
    let namespaces = db::list_namespaces(&conn)?;
    if json_output {
        println!("{}", serde_json::to_string(&serde_json::json!({"namespaces": namespaces}))?);
        return Ok(());
    }
    if namespaces.is_empty() {
        eprintln!("no namespaces");
    } else {
        for ns in &namespaces {
            println!("  {}: {} memories", ns.namespace, ns.count);
        }
    }
    Ok(())
}

fn cmd_export(db_path: PathBuf) -> Result<()> {
    let conn = db::open(&db_path)?;
    let memories = db::export_all(&conn)?;
    let export = serde_json::json!({
        "memories": memories,
        "count": memories.len(),
        "exported_at": Utc::now().to_rfc3339(),
    });
    println!("{}", serde_json::to_string_pretty(&export)?);
    Ok(())
}

fn cmd_import(db_path: PathBuf, json_output: bool) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;

    let data: serde_json::Value = serde_json::from_str(&buf)?;
    let memories: Vec<models::Memory> = serde_json::from_value(
        data.get("memories").cloned().unwrap_or(serde_json::Value::Array(vec![]))
    )?;

    let conn = db::open(&db_path)?;
    let mut imported = 0usize;
    let mut errors = Vec::new();
    for mem in memories {
        match db::insert(&conn, &mem) {
            Ok(()) => imported += 1,
            Err(e) => errors.push(format!("{}: {}", mem.id, e)),
        }
    }
    if json_output {
        println!("{}", serde_json::to_string(&serde_json::json!({"imported": imported, "errors": errors}))?);
    } else {
        println!("imported: {}", imported);
        if !errors.is_empty() {
            eprintln!("errors: {}", errors.len());
            for e in &errors {
                eprintln!("  {}", e);
            }
        }
    }
    Ok(())
}
