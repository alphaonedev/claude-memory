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
use tracing_subscriber::EnvFilter;

use crate::models::Tier;

const DEFAULT_DB: &str = "claude-memory.db";
const DEFAULT_PORT: u16 = 9077;

#[derive(Parser)]
#[command(name = "claude-memory", about = "Persistent memory daemon for Claude Code — short, mid, and long-term recall")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to SQLite database
    #[arg(long, env = "CLAUDE_MEMORY_DB", default_value = DEFAULT_DB, global = true)]
    db: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP memory daemon
    Serve(ServeArgs),
    /// Store a new memory
    Store(StoreArgs),
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
    /// Content
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

    match cli.command {
        Command::Serve(args) => serve(cli.db, args).await,
        Command::Store(args) => cmd_store(cli.db, args),
        Command::Recall(args) => cmd_recall(cli.db, args),
        Command::Search(args) => cmd_search(cli.db, args),
        Command::Get(args) => cmd_get(cli.db, args),
        Command::List(args) => cmd_list(cli.db, args),
        Command::Delete(args) => cmd_delete(cli.db, args),
        Command::Gc => cmd_gc(cli.db),
        Command::Stats => cmd_stats(cli.db),
    }
}

async fn serve(db_path: PathBuf, args: ServeArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("claude_memory=info".parse()?))
        .init();

    let conn = db::open(&db_path)?;
    let state: handlers::Db = Arc::new(Mutex::new((conn, db_path.clone())));

    let app = Router::new()
        .route("/api/v1/health", get(handlers::health))
        .route("/api/v1/memories", get(handlers::list_memories))
        .route("/api/v1/memories", post(handlers::create_memory))
        .route("/api/v1/memories/bulk", post(handlers::bulk_create))
        .route("/api/v1/memories/{id}", get(handlers::get_memory))
        .route("/api/v1/memories/{id}", put(handlers::update_memory))
        .route("/api/v1/memories/{id}", delete(handlers::delete_memory))
        .route("/api/v1/search", get(handlers::search_memories))
        .route("/api/v1/recall", get(handlers::recall_memories))
        .route("/api/v1/stats", get(handlers::get_stats))
        .route("/api/v1/gc", post(handlers::run_gc))
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("claude-memory listening on {addr}");
    tracing::info!("database: {}", db_path.display());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// --- CLI commands ---

fn cmd_store(db_path: PathBuf, args: StoreArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = Tier::from_str(&args.tier)
        .ok_or_else(|| anyhow::anyhow!("invalid tier: {} (use short, mid, long)", args.tier))?;
    let tags: Vec<String> = args.tags.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let now = Utc::now();
    let expires_at = tier.default_ttl_secs().map(|secs| (now + Duration::seconds(secs)).to_rfc3339());
    let mem = models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace: args.namespace,
        title: args.title,
        content: args.content,
        tags,
        priority: args.priority.clamp(1, 10),
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
    };
    db::insert(&conn, &mem)?;
    println!("{}", serde_json::to_string_pretty(&mem)?);
    Ok(())
}

fn cmd_recall(db_path: PathBuf, args: RecallArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    let results = db::recall(&conn, &args.context, args.namespace.as_deref(), args.limit)?;
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

fn cmd_search(db_path: PathBuf, args: SearchArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::search(&conn, &args.query, args.namespace.as_deref(), tier.as_ref(), args.limit, None)?;
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

fn cmd_get(db_path: PathBuf, args: GetArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    match db::get(&conn, &args.id)? {
        Some(mem) => println!("{}", serde_json::to_string_pretty(&mem)?),
        None => { eprintln!("not found: {}", args.id); std::process::exit(1); }
    }
    Ok(())
}

fn cmd_list(db_path: PathBuf, args: ListArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::list(&conn, args.namespace.as_deref(), tier.as_ref(), args.limit, 0, None)?;
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

fn cmd_delete(db_path: PathBuf, args: DeleteArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    if db::delete(&conn, &args.id)? {
        println!("deleted: {}", args.id);
    } else {
        eprintln!("not found: {}", args.id);
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_gc(db_path: PathBuf) -> Result<()> {
    let conn = db::open(&db_path)?;
    let count = db::gc(&conn)?;
    println!("expired memories deleted: {}", count);
    Ok(())
}

fn cmd_stats(db_path: PathBuf) -> Result<()> {
    let conn = db::open(&db_path)?;
    let stats = db::stats(&conn, &db_path)?;
    println!("total memories: {}", stats.total);
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
