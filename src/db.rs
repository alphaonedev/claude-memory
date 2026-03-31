use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::path::Path;

use crate::models::{Memory, NamespaceCount, Stats, Tier, TierCount};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id               TEXT PRIMARY KEY,
    tier             TEXT NOT NULL,
    namespace        TEXT NOT NULL DEFAULT 'global',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '[]',
    priority         INTEGER NOT NULL DEFAULT 5,
    access_count     INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    last_accessed_at TEXT,
    expires_at       TEXT
);

CREATE INDEX IF NOT EXISTS idx_memories_tier ON memories(tier);
CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
CREATE INDEX IF NOT EXISTS idx_memories_priority ON memories(priority DESC);
CREATE INDEX IF NOT EXISTS idx_memories_expires ON memories(expires_at);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    title,
    content,
    tags,
    content=memories,
    content_rowid=rowid
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("failed to open database")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA).context("failed to initialize schema")?;
    Ok(conn)
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let tags_json: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let tier_str: String = row.get("tier")?;
    let tier = Tier::from_str(&tier_str).unwrap_or(Tier::Mid);
    Ok(Memory {
        id: row.get("id")?,
        tier,
        namespace: row.get("namespace")?,
        title: row.get("title")?,
        content: row.get("content")?,
        tags,
        priority: row.get("priority")?,
        access_count: row.get("access_count")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_accessed_at: row.get("last_accessed_at")?,
        expires_at: row.get("expires_at")?,
    })
}

pub fn insert(conn: &Connection, mem: &Memory) -> Result<()> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, access_count, created_at, updated_at, last_accessed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            mem.id, mem.tier.as_str(), mem.namespace, mem.title, mem.content,
            tags_json, mem.priority, mem.access_count, mem.created_at,
            mem.updated_at, mem.last_accessed_at, mem.expires_at,
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM memories WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], row_to_memory)?;
    match rows.next() {
        Some(Ok(m)) => {
            // Bump access count
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE memories SET access_count = access_count + 1, last_accessed_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            Ok(Some(m))
        }
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

pub fn update(
    conn: &Connection,
    id: &str,
    title: Option<&str>,
    content: Option<&str>,
    tier: Option<&Tier>,
    namespace: Option<&str>,
    tags: Option<&Vec<String>>,
    priority: Option<i32>,
    expires_at: Option<&str>,
) -> Result<bool> {
    // Read without bumping access count
    let mut stmt = conn.prepare("SELECT * FROM memories WHERE id = ?1")?;
    let mut rows = stmt.query_map(params![id], row_to_memory)?;
    let existing = match rows.next() {
        Some(Ok(m)) => m,
        _ => return Ok(false),
    };
    drop(rows);
    drop(stmt);

    let title = title.unwrap_or(&existing.title);
    let content = content.unwrap_or(&existing.content);
    let tier = tier.unwrap_or(&existing.tier);
    let namespace = namespace.unwrap_or(&existing.namespace);
    let tags = tags.unwrap_or(&existing.tags);
    let priority = priority.unwrap_or(existing.priority);
    let expires_at = expires_at.or(existing.expires_at.as_deref());
    let tags_json = serde_json::to_string(tags)?;
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE memories SET tier=?1, namespace=?2, title=?3, content=?4, tags=?5, priority=?6, updated_at=?7, expires_at=?8
         WHERE id=?9",
        params![tier.as_str(), namespace, title, content, tags_json, priority, now, expires_at, id],
    )?;
    Ok(true)
}

pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let changed = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
    Ok(changed > 0)
}

pub fn list(
    conn: &Connection,
    namespace: Option<&str>,
    tier: Option<&Tier>,
    limit: usize,
    offset: usize,
    min_priority: Option<i32>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let tier_str = tier.map(|t| t.as_str().to_string());
    let mut stmt = conn.prepare(
        "SELECT * FROM memories
         WHERE (?1 IS NULL OR namespace = ?1)
           AND (?2 IS NULL OR tier = ?2)
           AND (?3 IS NULL OR priority >= ?3)
           AND (expires_at IS NULL OR expires_at > ?4)
         ORDER BY priority DESC, updated_at DESC
         LIMIT ?5 OFFSET ?6",
    )?;
    let rows = stmt.query_map(
        params![namespace, tier_str, min_priority, now, limit as i64, offset as i64],
        row_to_memory,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn search(
    conn: &Connection,
    query: &str,
    namespace: Option<&str>,
    tier: Option<&Tier>,
    limit: usize,
    min_priority: Option<i32>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let tier_str = tier.map(|t| t.as_str().to_string());
    let fts_query = sanitize_fts_query(query);

    let mut stmt = conn.prepare(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.access_count, m.created_at, m.updated_at, m.last_accessed_at, m.expires_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1
           AND (?2 IS NULL OR m.namespace = ?2)
           AND (?3 IS NULL OR m.tier = ?3)
           AND (?4 IS NULL OR m.priority >= ?4)
           AND (m.expires_at IS NULL OR m.expires_at > ?5)
         ORDER BY (fts.rank * -1) + (m.priority * 0.5) + (m.access_count * 0.1) DESC
         LIMIT ?6",
    )?;
    let rows = stmt.query_map(
        params![fts_query, namespace, tier_str, min_priority, now, limit as i64],
        row_to_memory,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Recall — the high-level "what do I know about X?" query.
/// Searches across all tiers, boosts long-term and frequently-accessed memories.
pub fn recall(
    conn: &Connection,
    context: &str,
    namespace: Option<&str>,
    limit: usize,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let fts_query = sanitize_fts_query(context);

    let mut stmt = conn.prepare(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.access_count, m.created_at, m.updated_at, m.last_accessed_at, m.expires_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1
           AND (?2 IS NULL OR m.namespace = ?2)
           AND (m.expires_at IS NULL OR m.expires_at > ?3)
         ORDER BY
           (fts.rank * -1)
           + (m.priority * 0.5)
           + (m.access_count * 0.1)
           + (CASE m.tier WHEN 'long' THEN 3.0 WHEN 'mid' THEN 1.0 ELSE 0.0 END)
           DESC
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![fts_query, namespace, now, limit as i64],
        row_to_memory,
    )?;
    // Bump access counts for recalled memories
    let results: Vec<Memory> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    for mem in &results {
        let _ = conn.execute(
            "UPDATE memories SET access_count = access_count + 1, last_accessed_at = ?1 WHERE id = ?2",
            params![now, mem.id],
        );
    }
    Ok(results)
}

fn sanitize_fts_query(input: &str) -> String {
    let has_operators = input.contains('"')
        || input.contains("OR")
        || input.contains("AND")
        || input.contains("NOT")
        || input.contains('*');
    if has_operators {
        return input.to_string();
    }
    input
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn stats(conn: &Connection, db_path: &Path) -> Result<Stats> {
    let total: usize = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;

    let mut stmt = conn.prepare("SELECT tier, COUNT(*) FROM memories GROUP BY tier ORDER BY COUNT(*) DESC")?;
    let by_tier: Vec<TierCount> = stmt
        .query_map([], |row| Ok(TierCount { tier: row.get(0)?, count: row.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare("SELECT namespace, COUNT(*) FROM memories GROUP BY namespace ORDER BY COUNT(*) DESC")?;
    let by_namespace: Vec<NamespaceCount> = stmt
        .query_map([], |row| Ok(NamespaceCount { namespace: row.get(0)?, count: row.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    Ok(Stats { total, by_tier, by_namespace, db_size_bytes })
}

pub fn gc(conn: &Connection) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let deleted = conn.execute(
        "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
        params![now],
    )?;
    Ok(deleted)
}
