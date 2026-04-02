// Copyright (c) 2026 AlphaOne LLC. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::path::Path;

use crate::models::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id               TEXT PRIMARY KEY,
    tier             TEXT NOT NULL,
    namespace        TEXT NOT NULL DEFAULT 'global',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '[]',
    priority         INTEGER NOT NULL DEFAULT 5,
    confidence       REAL NOT NULL DEFAULT 1.0,
    source           TEXT NOT NULL DEFAULT 'api',
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
CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_title_ns ON memories(title, namespace);

CREATE TABLE IF NOT EXISTS memory_links (
    source_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation    TEXT NOT NULL DEFAULT 'related_to',
    created_at  TEXT NOT NULL,
    PRIMARY KEY (source_id, target_id, relation)
);

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

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);
"#;

const CURRENT_SCHEMA_VERSION: i64 = 2;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("failed to open database")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)
        .context("failed to initialize schema")?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if version < 2 {
        // Check which columns exist using PRAGMA
        let mut has_confidence = false;
        let mut has_source = false;
        let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
        let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for col in cols {
            match col?.as_str() {
                "confidence" => has_confidence = true,
                "source" => has_source = true,
                _ => {}
            }
        }
        drop(stmt);
        if !has_confidence {
            conn.execute(
                "ALTER TABLE memories ADD COLUMN confidence REAL NOT NULL DEFAULT 1.0",
                [],
            )?;
        }
        if !has_source {
            conn.execute(
                "ALTER TABLE memories ADD COLUMN source TEXT NOT NULL DEFAULT 'api'",
                [],
            )?;
        }
    }
    if version < CURRENT_SCHEMA_VERSION {
        conn.execute("DELETE FROM schema_version", [])?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![CURRENT_SCHEMA_VERSION],
        )?;
    }
    Ok(())
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
        confidence: row.get("confidence").unwrap_or(1.0),
        source: row.get("source").unwrap_or_else(|_| "api".to_string()),
        access_count: row.get("access_count")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_accessed_at: row.get("last_accessed_at")?,
        expires_at: row.get("expires_at")?,
    })
}

/// Insert with upsert on title+namespace. Returns the ID (existing or new).
pub fn insert(conn: &Connection, mem: &Memory) -> Result<String> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at, last_accessed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(title, namespace) DO UPDATE SET
            content = excluded.content,
            tags = excluded.tags,
            priority = MAX(memories.priority, excluded.priority),
            confidence = MAX(memories.confidence, excluded.confidence),
            source = excluded.source,
            tier = CASE WHEN excluded.tier = 'long' THEN 'long'
                        WHEN memories.tier = 'long' THEN 'long'
                        WHEN excluded.tier = 'mid' THEN 'mid'
                        ELSE memories.tier END,
            updated_at = excluded.updated_at,
            expires_at = CASE WHEN excluded.tier = 'long' OR memories.tier = 'long' THEN NULL
                              ELSE COALESCE(excluded.expires_at, memories.expires_at) END",
        params![
            mem.id, mem.tier.as_str(), mem.namespace, mem.title, mem.content,
            tags_json, mem.priority, mem.confidence, mem.source, mem.access_count,
            mem.created_at, mem.updated_at, mem.last_accessed_at, mem.expires_at,
        ],
    )?;
    // Return the actual ID (could be the existing one on conflict)
    let actual_id: String = conn.query_row(
        "SELECT id FROM memories WHERE title = ?1 AND namespace = ?2",
        params![mem.title, mem.namespace],
        |r| r.get(0),
    )?;
    Ok(actual_id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    let mut stmt = conn.prepare("SELECT * FROM memories WHERE id = ?1")?;
    let mut rows = stmt.query_map(params![id], row_to_memory)?;
    match rows.next() {
        Some(Ok(m)) => Ok(Some(m)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// Bump access count, extend TTL, auto-promote — atomic via transaction.
pub fn touch(conn: &Connection, id: &str) -> Result<()> {
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let short_expires = (now + chrono::Duration::seconds(SHORT_TTL_EXTEND_SECS)).to_rfc3339();
    let mid_expires = (now + chrono::Duration::seconds(MID_TTL_EXTEND_SECS)).to_rfc3339();

    conn.execute_batch("BEGIN IMMEDIATE")?;

    let result = (|| -> Result<()> {
        conn.execute(
            "UPDATE memories SET
                access_count = MIN(access_count + 1, 1000000),
                last_accessed_at = ?1,
                expires_at = CASE
                    WHEN tier = 'long' THEN expires_at
                    WHEN tier = 'short' AND expires_at IS NOT NULL THEN ?2
                    WHEN tier = 'mid' AND expires_at IS NOT NULL THEN ?3
                    ELSE expires_at
                END
             WHERE id = ?4",
            params![now_str, short_expires, mid_expires, id],
        )?;

        conn.execute(
            "UPDATE memories SET tier = 'long', expires_at = NULL, updated_at = ?1
             WHERE id = ?2 AND tier = 'mid' AND access_count >= ?3",
            params![now_str, id, PROMOTION_THRESHOLD],
        )?;

        conn.execute(
            "UPDATE memories SET priority = MIN(priority + 1, 10)
             WHERE id = ?1 AND access_count > 0 AND access_count % 10 = 0 AND priority < 10",
            params![id],
        )?;

        Ok(())
    })();

    match result {
        Ok(()) => { conn.execute_batch("COMMIT")?; Ok(()) }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("ROLLBACK failed in touch: {}", rb);
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn update(
    conn: &Connection,
    id: &str,
    title: Option<&str>,
    content: Option<&str>,
    tier: Option<&Tier>,
    namespace: Option<&str>,
    tags: Option<&Vec<String>>,
    priority: Option<i32>,
    confidence: Option<f64>,
    expires_at: Option<&str>,
) -> Result<bool> {
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
    let confidence = confidence.unwrap_or(existing.confidence);
    let expires_at = expires_at.or(existing.expires_at.as_deref());
    let tags_json = serde_json::to_string(tags)?;
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE memories SET tier=?1, namespace=?2, title=?3, content=?4, tags=?5, priority=?6, confidence=?7, updated_at=?8, expires_at=?9
         WHERE id=?10",
        params![tier.as_str(), namespace, title, content, tags_json, priority, confidence, now, expires_at, id],
    )?;
    Ok(true)
}

pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let changed = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
    Ok(changed > 0)
}

/// Forget by pattern — delete memories matching namespace + FTS pattern + tier.
pub fn forget(
    conn: &Connection,
    namespace: Option<&str>,
    pattern: Option<&str>,
    tier: Option<&Tier>,
) -> Result<usize> {
    if pattern.is_none() && namespace.is_none() && tier.is_none() {
        anyhow::bail!("at least one of namespace, pattern, or tier is required");
    }

    // If pattern provided, use FTS to find matching IDs
    if let Some(pat) = pattern {
        let fts_query = sanitize_fts_query(pat, true);
        let tier_str = tier.map(|t| t.as_str().to_string());
        let deleted = conn.execute(
            "DELETE FROM memories WHERE rowid IN (
                SELECT m.rowid FROM memories_fts fts
                JOIN memories m ON m.rowid = fts.rowid
                WHERE memories_fts MATCH ?1
                  AND (?2 IS NULL OR m.namespace = ?2)
                  AND (?3 IS NULL OR m.tier = ?3)
            )",
            params![fts_query, namespace, tier_str],
        )?;
        return Ok(deleted);
    }

    let tier_str = tier.map(|t| t.as_str().to_string());
    let deleted = conn.execute(
        "DELETE FROM memories WHERE (?1 IS NULL OR namespace = ?1) AND (?2 IS NULL OR tier = ?2)",
        params![namespace, tier_str],
    )?;
    Ok(deleted)
}

#[allow(clippy::too_many_arguments)]
pub fn list(
    conn: &Connection,
    namespace: Option<&str>,
    tier: Option<&Tier>,
    limit: usize,
    offset: usize,
    min_priority: Option<i32>,
    since: Option<&str>,
    until: Option<&str>,
    tags_filter: Option<&str>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let tier_str = tier.map(|t| t.as_str().to_string());
    let mut stmt = conn.prepare(
        "SELECT * FROM memories
         WHERE (?1 IS NULL OR namespace = ?1)
           AND (?2 IS NULL OR tier = ?2)
           AND (?3 IS NULL OR priority >= ?3)
           AND (expires_at IS NULL OR expires_at > ?4)
           AND (?5 IS NULL OR created_at >= ?5)
           AND (?6 IS NULL OR created_at <= ?6)
           AND (?7 IS NULL OR EXISTS (SELECT 1 FROM json_each(memories.tags) WHERE json_each.value = ?7))
         ORDER BY priority DESC, updated_at DESC
         LIMIT ?8 OFFSET ?9",
    )?;
    let rows = stmt.query_map(
        params![
            namespace,
            tier_str,
            min_priority,
            now,
            since,
            until,
            tags_filter,
            limit as i64,
            offset as i64
        ],
        row_to_memory,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
pub fn search(
    conn: &Connection,
    query: &str,
    namespace: Option<&str>,
    tier: Option<&Tier>,
    limit: usize,
    min_priority: Option<i32>,
    since: Option<&str>,
    until: Option<&str>,
    tags_filter: Option<&str>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let tier_str = tier.map(|t| t.as_str().to_string());
    let fts_query = sanitize_fts_query(query, false);

    let mut stmt = conn.prepare(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.confidence, m.source, m.access_count, m.created_at, m.updated_at,
                m.last_accessed_at, m.expires_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1
           AND (?2 IS NULL OR m.namespace = ?2)
           AND (?3 IS NULL OR m.tier = ?3)
           AND (?4 IS NULL OR m.priority >= ?4)
           AND (m.expires_at IS NULL OR m.expires_at > ?5)
           AND (?6 IS NULL OR m.created_at >= ?6)
           AND (?7 IS NULL OR m.created_at <= ?7)
           AND (?8 IS NULL OR EXISTS (SELECT 1 FROM json_each(m.tags) WHERE json_each.value = ?8))
         ORDER BY (fts.rank * -1)
           + (m.priority * 0.5)
           + (MIN(m.access_count, 50) * 0.1)
           + (m.confidence * 2.0)
           + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
           DESC
         LIMIT ?9",
    )?;
    let rows = stmt.query_map(
        params![
            fts_query,
            namespace,
            tier_str,
            min_priority,
            now,
            since,
            until,
            tags_filter,
            limit as i64
        ],
        row_to_memory,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Recall — fuzzy OR search + touch + auto-promote + TTL extension.
pub fn recall(
    conn: &Connection,
    context: &str,
    namespace: Option<&str>,
    limit: usize,
    tags_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let fts_query = sanitize_fts_query(context, true);

    let mut stmt = conn.prepare(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.confidence, m.source, m.access_count, m.created_at, m.updated_at,
                m.last_accessed_at, m.expires_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1
           AND (?2 IS NULL OR m.namespace = ?2)
           AND (m.expires_at IS NULL OR m.expires_at > ?3)
           AND (?4 IS NULL OR EXISTS (SELECT 1 FROM json_each(m.tags) WHERE json_each.value = ?4))
           AND (?5 IS NULL OR m.created_at >= ?5)
           AND (?6 IS NULL OR m.created_at <= ?6)
         ORDER BY
           (fts.rank * -1)
           + (m.priority * 0.5)
           + (MIN(m.access_count, 50) * 0.1)
           + (m.confidence * 2.0)
           + (CASE m.tier WHEN 'long' THEN 3.0 WHEN 'mid' THEN 1.0 ELSE 0.0 END)
           + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
           DESC
         LIMIT ?7",
    )?;
    let rows = stmt.query_map(
        params![fts_query, namespace, now, tags_filter, since, until, limit as i64],
        row_to_memory,
    )?;
    let results: Vec<Memory> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    // Touch all recalled memories (bumps access, extends TTL, auto-promotes)
    for mem in &results {
        if let Err(e) = touch(conn, &mem.id) {
            tracing::warn!("touch failed for memory {}: {}", &mem.id, e);
        }
    }
    Ok(results)
}

/// Detect potential contradictions: memories in same namespace with similar titles.
pub fn find_contradictions(conn: &Connection, title: &str, namespace: &str) -> Result<Vec<Memory>> {
    let fts_query = sanitize_fts_query(title, true);
    let mut stmt = conn.prepare(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.confidence, m.source, m.access_count, m.created_at, m.updated_at,
                m.last_accessed_at, m.expires_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1 AND m.namespace = ?2
         ORDER BY fts.rank
         LIMIT 5",
    )?;
    let rows = stmt.query_map(params![fts_query, namespace], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// --- Links ---

pub fn create_link(
    conn: &Connection,
    source_id: &str,
    target_id: &str,
    relation: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO memory_links (source_id, target_id, relation, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![source_id, target_id, relation, now],
    )?;
    Ok(())
}

pub fn get_links(conn: &Connection, id: &str) -> Result<Vec<MemoryLink>> {
    let mut stmt = conn.prepare(
        "SELECT source_id, target_id, relation, created_at FROM memory_links
         WHERE source_id = ?1 OR target_id = ?1",
    )?;
    let rows = stmt.query_map(params![id], |row| {
        Ok(MemoryLink {
            source_id: row.get(0)?,
            target_id: row.get(1)?,
            relation: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

#[allow(dead_code)]
pub fn delete_link(conn: &Connection, source_id: &str, target_id: &str) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM memory_links WHERE source_id = ?1 AND target_id = ?2",
        params![source_id, target_id],
    )?;
    Ok(changed > 0)
}

// --- Consolidation ---

/// Consolidate multiple memories into one. Returns the new memory ID.
/// Deletes the source memories and creates links from new → old (derived_from).
pub fn consolidate(
    conn: &Connection,
    ids: &[String],
    title: &str,
    summary: &str,
    namespace: &str,
    tier: &Tier,
    source: &str,
) -> Result<String> {
    let now = Utc::now().to_rfc3339();
    let new_id = uuid::Uuid::new_v4().to_string();

    conn.execute_batch("BEGIN IMMEDIATE")?;

    let result = (|| -> Result<String> {
        // Verify all IDs exist before proceeding
        for id in ids {
            if get(conn, id)?.is_none() {
                anyhow::bail!("memory not found: {}", id);
            }
        }

        // Collect max priority and all tags from source memories
        let mut max_priority = 5i32;
        let mut all_tags: Vec<String> = Vec::new();
        let mut total_access = 0i64;
        for id in ids {
            if let Some(mem) = get(conn, id)? {
                max_priority = max_priority.max(mem.priority);
                all_tags.extend(mem.tags);
                total_access += mem.access_count;
            }
        }
        all_tags.sort();
        all_tags.dedup();
        let tags_json = serde_json::to_string(&all_tags)?;

        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1.0, ?8, ?9, ?10, ?10)",
            params![new_id, tier.as_str(), namespace, title, summary, tags_json, max_priority, source, total_access, now],
        )?;

        for id in ids {
            create_link(conn, &new_id, id, "derived_from")?;
        }

        for id in ids {
            delete(conn, id)?;
        }

        Ok(new_id.clone())
    })();

    match result {
        Ok(id) => { conn.execute_batch("COMMIT")?; Ok(id) }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("ROLLBACK failed in consolidate: {}", rb);
            }
            Err(e)
        }
    }
}

fn sanitize_fts_query(input: &str, use_or: bool) -> String {
    let joiner = if use_or { " OR " } else { " " };
    let tokens: Vec<String> = input
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .filter(|t| {
            // Filter out FTS5 boolean operators as standalone tokens
            let upper = t.to_uppercase();
            upper != "AND" && upper != "OR" && upper != "NOT" && upper != "NEAR"
        })
        .map(|token| {
            // Strip ALL FTS5 special characters to prevent injection
            let clean: String = token
                .chars()
                .filter(|c| {
                    *c != '"' && *c != '*' && *c != '^'
                        && *c != '{' && *c != '}'
                        && *c != '(' && *c != ')'
                        && *c != ':' && *c != '-'
                })
                .collect();
            if clean.is_empty() {
                return String::new();
            }
            format!("\"{}\"", clean)
        })
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return "\"_empty_\"".to_string();
    }
    tokens.join(joiner)
}

pub fn list_namespaces(conn: &Connection) -> Result<Vec<NamespaceCount>> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT namespace, COUNT(*) FROM memories WHERE expires_at IS NULL OR expires_at > ?1 GROUP BY namespace ORDER BY COUNT(*) DESC",
    )?;
    let rows = stmt.query_map(params![now], |row| {
        Ok(NamespaceCount {
            namespace: row.get(0)?,
            count: row.get(1)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn stats(conn: &Connection, db_path: &Path) -> Result<Stats> {
    let total: usize = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;

    let mut stmt =
        conn.prepare("SELECT tier, COUNT(*) FROM memories GROUP BY tier ORDER BY COUNT(*) DESC")?;
    let by_tier = stmt
        .query_map([], |row| {
            Ok(TierCount {
                tier: row.get(0)?,
                count: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(
        "SELECT namespace, COUNT(*) FROM memories GROUP BY namespace ORDER BY COUNT(*) DESC",
    )?;
    let by_namespace = stmt
        .query_map([], |row| {
            Ok(NamespaceCount {
                namespace: row.get(0)?,
                count: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let now = Utc::now().to_rfc3339();
    let one_hour = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let expiring_soon: usize = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE expires_at IS NOT NULL AND expires_at > ?1 AND expires_at <= ?2",
        params![now, one_hour], |r| r.get(0),
    )?;

    let links_count: usize = conn
        .query_row("SELECT COUNT(*) FROM memory_links", [], |r| r.get(0))
        .unwrap_or(0);
    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    Ok(Stats {
        total,
        by_tier,
        by_namespace,
        expiring_soon,
        links_count,
        db_size_bytes,
    })
}

/// Run GC if there are any expired memories. Lightweight check first.
pub fn gc_if_needed(conn: &Connection) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let has_expired: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1)",
            params![now],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if has_expired { gc(conn) } else { Ok(0) }
}

pub fn gc(conn: &Connection) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let deleted = conn.execute(
        "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
        params![now],
    )?;
    Ok(deleted)
}

pub fn export_all(conn: &Connection) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare("SELECT * FROM memories ORDER BY created_at ASC")?;
    let rows = stmt.query_map([], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn export_links(conn: &Connection) -> Result<Vec<MemoryLink>> {
    let mut stmt =
        conn.prepare("SELECT source_id, target_id, relation, created_at FROM memory_links")?;
    let rows = stmt.query_map([], |row| {
        Ok(MemoryLink {
            source_id: row.get(0)?,
            target_id: row.get(1)?,
            relation: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Insert with timestamp-aware conflict resolution for sync.
/// Only overwrites if the incoming memory is newer (by updated_at).
pub fn insert_if_newer(conn: &Connection, mem: &Memory) -> Result<String> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at, last_accessed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(title, namespace) DO UPDATE SET
            content = CASE WHEN excluded.updated_at > memories.updated_at THEN excluded.content ELSE memories.content END,
            tags = CASE WHEN excluded.updated_at > memories.updated_at THEN excluded.tags ELSE memories.tags END,
            priority = MAX(memories.priority, excluded.priority),
            confidence = MAX(memories.confidence, excluded.confidence),
            source = CASE WHEN excluded.updated_at > memories.updated_at THEN excluded.source ELSE memories.source END,
            tier = CASE WHEN excluded.tier = 'long' THEN 'long'
                        WHEN memories.tier = 'long' THEN 'long'
                        WHEN excluded.tier = 'mid' THEN 'mid'
                        ELSE memories.tier END,
            updated_at = MAX(memories.updated_at, excluded.updated_at),
            access_count = MAX(memories.access_count, excluded.access_count),
            expires_at = CASE WHEN excluded.tier = 'long' OR memories.tier = 'long' THEN NULL
                              ELSE COALESCE(excluded.expires_at, memories.expires_at) END",
        params![
            mem.id, mem.tier.as_str(), mem.namespace, mem.title, mem.content,
            tags_json, mem.priority, mem.confidence, mem.source, mem.access_count,
            mem.created_at, mem.updated_at, mem.last_accessed_at, mem.expires_at,
        ],
    )?;
    let actual_id: String = conn.query_row(
        "SELECT id FROM memories WHERE title = ?1 AND namespace = ?2",
        params![mem.title, mem.namespace],
        |r| r.get(0),
    )?;
    Ok(actual_id)
}

/// Checkpoint WAL for clean shutdown.
pub fn checkpoint(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    Ok(())
}

/// Deep health check — verifies DB is accessible and FTS is functional.
pub fn health_check(conn: &Connection) -> Result<bool> {
    let _: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO memories_fts(memories_fts) VALUES('integrity-check')",
        [],
    )?;
    Ok(true)
}
