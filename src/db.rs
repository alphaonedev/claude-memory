use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::path::Path;

use crate::models::{Category, CategoryCount, Memory, Stats};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id            TEXT PRIMARY KEY,
    category      TEXT NOT NULL,
    title         TEXT NOT NULL,
    content       TEXT NOT NULL,
    tags          TEXT NOT NULL DEFAULT '[]',
    priority      INTEGER NOT NULL DEFAULT 5,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    expires_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);
CREATE INDEX IF NOT EXISTS idx_memories_priority ON memories(priority DESC);
CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at DESC);

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
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA).context("failed to initialize schema")?;
    Ok(conn)
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let tags_json: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let cat_str: String = row.get("category")?;
    let category = Category::from_str(&cat_str).unwrap_or(Category::Reference);
    Ok(Memory {
        id: row.get("id")?,
        category,
        title: row.get("title")?,
        content: row.get("content")?,
        tags,
        priority: row.get("priority")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        expires_at: row.get("expires_at")?,
    })
}

pub fn insert(conn: &Connection, mem: &Memory) -> Result<()> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    conn.execute(
        "INSERT INTO memories (id, category, title, content, tags, priority, created_at, updated_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            mem.id,
            mem.category.as_str(),
            mem.title,
            mem.content,
            tags_json,
            mem.priority,
            mem.created_at,
            mem.updated_at,
            mem.expires_at,
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, category, title, content, tags, priority, created_at, updated_at, expires_at
         FROM memories WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], row_to_memory)?;
    match rows.next() {
        Some(Ok(m)) => Ok(Some(m)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

pub fn update(
    conn: &Connection,
    id: &str,
    title: Option<&str>,
    content: Option<&str>,
    category: Option<&Category>,
    tags: Option<&Vec<String>>,
    priority: Option<i32>,
    expires_at: Option<&str>,
) -> Result<bool> {
    let existing = get(conn, id)?;
    let Some(existing) = existing else {
        return Ok(false);
    };

    let title = title.unwrap_or(&existing.title);
    let content = content.unwrap_or(&existing.content);
    let cat = category.unwrap_or(&existing.category);
    let tags = tags.unwrap_or(&existing.tags);
    let priority = priority.unwrap_or(existing.priority);
    let expires_at = expires_at.or(existing.expires_at.as_deref());
    let tags_json = serde_json::to_string(tags)?;
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE memories SET category=?1, title=?2, content=?3, tags=?4, priority=?5, updated_at=?6, expires_at=?7
         WHERE id=?8",
        params![cat.as_str(), title, content, tags_json, priority, now, expires_at, id],
    )?;
    Ok(true)
}

pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let changed = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
    Ok(changed > 0)
}

pub fn list(
    conn: &Connection,
    category: Option<&Category>,
    limit: usize,
    offset: usize,
    min_priority: Option<i32>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let cat_str = category.map(|c| c.as_str().to_string());
    let mut stmt = conn.prepare(
        "SELECT id, category, title, content, tags, priority, created_at, updated_at, expires_at
         FROM memories
         WHERE (?1 IS NULL OR category = ?1)
           AND (?2 IS NULL OR priority >= ?2)
           AND (expires_at IS NULL OR expires_at > ?3)
         ORDER BY priority DESC, updated_at DESC
         LIMIT ?4 OFFSET ?5",
    )?;
    let rows = stmt.query_map(
        params![cat_str, min_priority, now, limit as i64, offset as i64],
        row_to_memory,
    )?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

pub fn search(
    conn: &Connection,
    query: &str,
    category: Option<&Category>,
    limit: usize,
    min_priority: Option<i32>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let cat_str = category.map(|c| c.as_str().to_string());

    // Sanitize FTS5 query: wrap each token in double quotes unless it already contains operators
    let fts_query = sanitize_fts_query(query);

    let mut stmt = conn.prepare(
        "SELECT m.id, m.category, m.title, m.content, m.tags, m.priority,
                m.created_at, m.updated_at, m.expires_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1
           AND (?2 IS NULL OR m.category = ?2)
           AND (?3 IS NULL OR m.priority >= ?3)
           AND (m.expires_at IS NULL OR m.expires_at > ?4)
         ORDER BY (fts.rank * -1) + (m.priority * 0.5) DESC
         LIMIT ?5",
    )?;
    let rows = stmt.query_map(
        params![fts_query, cat_str, min_priority, now, limit as i64],
        row_to_memory,
    )?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn sanitize_fts_query(input: &str) -> String {
    let has_operators = input.contains('"') || input.contains("OR") || input.contains("AND") || input.contains("NOT") || input.contains('*');
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
    let mut stmt = conn.prepare("SELECT category, COUNT(*) as cnt FROM memories GROUP BY category ORDER BY cnt DESC")?;
    let rows = stmt.query_map([], |row| {
        Ok(CategoryCount {
            category: row.get(0)?,
            count: row.get(1)?,
        })
    })?;
    let mut by_category = Vec::new();
    for row in rows {
        by_category.push(row?);
    }
    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    Ok(Stats {
        total,
        by_category,
        db_size_bytes,
    })
}

pub fn gc(conn: &Connection) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let deleted = conn.execute(
        "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
        params![now],
    )?;
    Ok(deleted)
}
