mod db;
mod handlers;
mod models;

use anyhow::Result;
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use crate::models::Category;

const DEFAULT_DB: &str = "/opt/cybercommand/claude-memory.db";
const DEFAULT_PORT: u16 = 9077;

#[derive(Parser)]
#[command(name = "claude-memory", about = "Persistent memory daemon for Claude Code")]
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
    /// Retrieve a memory by ID
    Get(GetArgs),
    /// Search memories by text
    Search(SearchArgs),
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
    /// Host to bind to
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to bind to
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
}

#[derive(Args)]
struct StoreArgs {
    /// Category
    #[arg(long, short)]
    category: String,
    /// Title
    #[arg(long, short)]
    title: String,
    /// Content
    #[arg(long)]
    content: String,
    /// Comma-separated tags
    #[arg(long, default_value = "")]
    tags: String,
    /// Priority (1-10)
    #[arg(long, default_value_t = 5)]
    priority: i32,
}

#[derive(Args)]
struct GetArgs {
    /// Memory ID
    id: String,
}

#[derive(Args)]
struct SearchArgs {
    /// Search query
    query: String,
    /// Filter by category
    #[arg(long, short)]
    category: Option<String>,
    /// Max results
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Args)]
struct ListArgs {
    /// Filter by category
    #[arg(long, short)]
    category: Option<String>,
    /// Max results
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Args)]
struct DeleteArgs {
    /// Memory ID
    id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve(args) => serve(cli.db, args).await,
        Command::Store(args) => cmd_store(cli.db, args),
        Command::Get(args) => cmd_get(cli.db, args),
        Command::Search(args) => cmd_search(cli.db, args),
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
        .route("/api/v1/stats", get(handlers::get_stats))
        .route("/api/v1/gc", post(handlers::run_gc))
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("claude-memory listening on {}", addr);
    tracing::info!("database: {}", db_path.display());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// --- CLI commands (direct SQLite, no HTTP) ---

fn cmd_store(db_path: PathBuf, args: StoreArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    let category = Category::from_str(&args.category)
        .ok_or_else(|| anyhow::anyhow!("invalid category: {}", args.category))?;
    let tags: Vec<String> = args
        .tags
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let now = chrono::Utc::now().to_rfc3339();
    let mem = models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        category,
        title: args.title,
        content: args.content,
        tags,
        priority: args.priority.clamp(1, 10),
        created_at: now.clone(),
        updated_at: now,
        expires_at: None,
    };
    db::insert(&conn, &mem)?;
    println!("{}", serde_json::to_string_pretty(&mem)?);
    Ok(())
}

fn cmd_get(db_path: PathBuf, args: GetArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    match db::get(&conn, &args.id)? {
        Some(mem) => println!("{}", serde_json::to_string_pretty(&mem)?),
        None => {
            eprintln!("not found: {}", args.id);
            std::process::exit(1);
        }
    }
    Ok(())
}

fn cmd_search(db_path: PathBuf, args: SearchArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    let category = args.category.as_deref().and_then(Category::from_str);
    let results = db::search(&conn, &args.query, category.as_ref(), args.limit, None)?;
    if results.is_empty() {
        eprintln!("no results for: {}", args.query);
    } else {
        for mem in &results {
            println!("[{}] {} (priority={}, cat={})", mem.id, mem.title, mem.priority, mem.category);
            if !mem.content.is_empty() {
                let preview: String = mem.content.chars().take(120).collect();
                println!("  {}", preview);
            }
        }
        println!("\n{} result(s)", results.len());
    }
    Ok(())
}

fn cmd_list(db_path: PathBuf, args: ListArgs) -> Result<()> {
    let conn = db::open(&db_path)?;
    let category = args.category.as_deref().and_then(Category::from_str);
    let results = db::list(&conn, category.as_ref(), args.limit, 0, None)?;
    if results.is_empty() {
        eprintln!("no memories stored");
    } else {
        for mem in &results {
            println!("[{}] {} (priority={}, cat={})", mem.id, mem.title, mem.priority, mem.category);
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
    for cat in &stats.by_category {
        println!("  {}: {}", cat.category, cat.count);
    }
    Ok(())
}
