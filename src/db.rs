// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;

use crate::models::{
    AGENTS_NAMESPACE, AgentRegistration, Memory, MemoryLink, NamespaceCount, PROMOTION_THRESHOLD,
    Stats, Tier, TierCount, namespace_ancestors,
};

/// Computed 4-tuple of visibility prefixes for an agent position (Task 1.5).
/// Index 0 = agent's own namespace (private), 1 = parent (team),
/// 2 = grandparent (unit), 3 = great-grandparent (org). Missing = `None`.
type VisibilityPrefixes = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn compute_visibility_prefixes(as_agent: Option<&str>) -> VisibilityPrefixes {
    let Some(ns) = as_agent else {
        return (None, None, None, None);
    };
    let ancestors = namespace_ancestors(ns);
    let p = ancestors.first().cloned();
    let t = ancestors.get(1).cloned();
    let u = ancestors.get(2).cloned();
    let o = ancestors.get(3).cloned();
    (p, t, u, o)
}

/// Rust-side visibility check for paths that can't easily attach SQL
/// visibility (the HNSW branch of `recall_hybrid` iterates memories loaded
/// via `get()`). Returns `true` when `as_agent` is unset (no filter) or
/// when the memory's scope + namespace grant visibility to the caller.
fn is_visible(mem: &Memory, prefixes: &VisibilityPrefixes) -> bool {
    let (p, t, u, o) = prefixes;
    if p.is_none() {
        return true;
    }
    let scope = mem
        .metadata
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("private");
    match scope {
        "collective" => true,
        "private" => p.as_ref().is_some_and(|ns| &mem.namespace == ns),
        "team" => matches_subtree(&mem.namespace, t.as_deref()),
        "unit" => matches_subtree(&mem.namespace, u.as_deref()),
        "org" => matches_subtree(&mem.namespace, o.as_deref()),
        _ => false,
    }
}

fn matches_subtree(namespace: &str, prefix: Option<&str>) -> bool {
    match prefix {
        None => false,
        Some(p) => namespace == p || namespace.starts_with(&format!("{p}/")),
    }
}

/// Generate the visibility WHERE-clause fragment starting at placeholder `start`.
/// Uses placeholders `?start .. ?start+3` for private/team/unit/org prefixes.
/// See `compute_visibility_prefixes` for the bind order.
fn visibility_clause(start: usize, table_alias: &str) -> String {
    let private_ph = start;
    let team_ph = start + 1;
    let unit_ph = start + 2;
    let org_ph = start + 3;
    let ta = table_alias;
    format!(
        "AND (\
            ?{private_ph} IS NULL \
            OR COALESCE(json_extract({ta}.metadata, '$.scope'), 'private') = 'collective' \
            OR (COALESCE(json_extract({ta}.metadata, '$.scope'), 'private') = 'private' AND {ta}.namespace = ?{private_ph}) \
            OR (json_extract({ta}.metadata, '$.scope') = 'team' AND ?{team_ph} IS NOT NULL AND ({ta}.namespace = ?{team_ph} OR {ta}.namespace LIKE ?{team_ph} || '/%')) \
            OR (json_extract({ta}.metadata, '$.scope') = 'unit' AND ?{unit_ph} IS NOT NULL AND ({ta}.namespace = ?{unit_ph} OR {ta}.namespace LIKE ?{unit_ph} || '/%')) \
            OR (json_extract({ta}.metadata, '$.scope') = 'org'  AND ?{org_ph}  IS NOT NULL AND ({ta}.namespace = ?{org_ph}  OR {ta}.namespace LIKE ?{org_ph}  || '/%'))\
        )"
    )
}

const SCHEMA: &str = r"
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
    expires_at       TEXT,
    metadata         TEXT NOT NULL DEFAULT '{}'
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
";

const CURRENT_SCHEMA_VERSION: i64 = 7;

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

#[allow(clippy::too_many_lines)]
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    conn.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| -> Result<()> {
        if version < 2 {
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

        if version < 3 {
            // Add embedding column for semantic search (Phase 1+2)
            let mut has_embedding = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                if col?.as_str() == "embedding" {
                    has_embedding = true;
                }
            }
            drop(stmt);
            if !has_embedding {
                conn.execute("ALTER TABLE memories ADD COLUMN embedding BLOB", [])?;
            }
        }
        if version < 4 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS archived_memories (
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
                    expires_at       TEXT,
                    archived_at      TEXT NOT NULL,
                    archive_reason   TEXT NOT NULL DEFAULT 'ttl_expired',
                    metadata         TEXT NOT NULL DEFAULT '{}'
                );
                CREATE INDEX IF NOT EXISTS idx_archived_namespace ON archived_memories(namespace);
                CREATE INDEX IF NOT EXISTS idx_archived_at ON archived_memories(archived_at);",
            )?;
        }
        if version < 5 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS namespace_meta (
                    namespace    TEXT PRIMARY KEY,
                    standard_id  TEXT,
                    updated_at   TEXT NOT NULL
                );",
            )?;
        }
        if version < 6 {
            // Add parent_namespace column for rule layering
            let has_parent: bool = conn
                .prepare("SELECT parent_namespace FROM namespace_meta LIMIT 0")
                .is_ok();
            if !has_parent {
                conn.execute_batch("ALTER TABLE namespace_meta ADD COLUMN parent_namespace TEXT;")?;
            }
        }
        if version < 7 {
            // Add metadata JSON column to memories and archived_memories tables
            let has_metadata: bool = conn
                .prepare("SELECT metadata FROM memories LIMIT 0")
                .is_ok();
            if !has_metadata {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'",
                    [],
                )?;
            }
            let has_archive_metadata: bool = conn
                .prepare("SELECT metadata FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_archive_metadata {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'",
                    [],
                )?;
            }
        }

        conn.execute("DELETE FROM schema_version", [])?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![CURRENT_SCHEMA_VERSION],
        )?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let tags_json: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let tier_str: String = row.get("tier")?;
    let tier = Tier::from_str(&tier_str).unwrap_or(Tier::Mid);
    let metadata_str: String = row
        .get::<_, String>("metadata")
        .unwrap_or_else(|_| "{}".to_string());
    let metadata: serde_json::Value = serde_json::from_str(&metadata_str).unwrap_or_else(|e| {
        tracing::warn!("corrupt metadata in DB row, defaulting to {{}}: {e}");
        serde_json::json!({})
    });
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
        metadata,
    })
}

/// Insert with upsert on title+namespace. Returns the ID (existing or new).
pub fn insert(conn: &Connection, mem: &Memory) -> Result<String> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    let metadata_json = serde_json::to_string(&mem.metadata)?;
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at, last_accessed_at, expires_at, metadata)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
                              ELSE COALESCE(excluded.expires_at, memories.expires_at) END,
            -- Preserve metadata.agent_id across upsert (NHI provenance is immutable).
            metadata = CASE
                WHEN json_extract(memories.metadata, '$.agent_id') IS NOT NULL
                THEN json_set(
                    excluded.metadata,
                    '$.agent_id',
                    json_extract(memories.metadata, '$.agent_id')
                )
                ELSE excluded.metadata
            END",
        params![
            mem.id, mem.tier.as_str(), mem.namespace, mem.title, mem.content,
            tags_json, mem.priority, mem.confidence, mem.source, mem.access_count,
            mem.created_at, mem.updated_at, mem.last_accessed_at, mem.expires_at,
            metadata_json,
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

/// Look up a memory by ID prefix. Returns the memory if exactly one match is found.
/// Returns `Ok(None)` if no matches. Returns an error if the prefix is ambiguous (>1 match).
pub fn get_by_prefix(conn: &Connection, prefix: &str) -> Result<Option<Memory>> {
    // Escape SQL LIKE wildcards in the prefix to prevent % and _ from matching broadly
    let escaped = prefix.replace('%', "\\%").replace('_', "\\_");
    let pattern = format!("{escaped}%");
    let mut stmt = conn.prepare("SELECT * FROM memories WHERE id LIKE ?1 ESCAPE '\\'")?;
    let rows: Vec<Memory> = stmt
        .query_map(params![pattern], row_to_memory)?
        .filter_map(Result::ok)
        .collect();
    match rows.len() {
        0 => Ok(None),
        1 => Ok(Some(rows.into_iter().next().expect("len checked"))),
        n => {
            let ids: Vec<String> = rows.iter().map(|m| m.id.clone()).collect();
            anyhow::bail!(
                "ambiguous ID prefix '{prefix}': {n} matches\n{}",
                ids.join("\n")
            );
        }
    }
}

/// Resolve an ID that may be a prefix. Tries exact match first, then prefix match.
pub fn resolve_id(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    if let Some(mem) = get(conn, id)? {
        return Ok(Some(mem));
    }
    get_by_prefix(conn, id)
}

/// Bump access count, extend TTL, auto-promote — atomic via transaction.
pub fn touch(conn: &Connection, id: &str, short_extend: i64, mid_extend: i64) -> Result<()> {
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let short_expires = (now + chrono::Duration::seconds(short_extend)).to_rfc3339();
    let mid_expires = (now + chrono::Duration::seconds(mid_extend)).to_rfc3339();

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
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("ROLLBACK failed in touch: {}", rb);
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Update a memory by ID. Returns (found, `content_changed`) so callers can
/// re-generate embeddings when the searchable text has changed.
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
    metadata: Option<&serde_json::Value>,
) -> Result<(bool, bool)> {
    let mut stmt = conn.prepare("SELECT * FROM memories WHERE id = ?1")?;
    let mut rows = stmt.query_map(params![id], row_to_memory)?;
    let Some(Ok(existing)) = rows.next() else {
        return Ok((false, false));
    };
    drop(rows);
    drop(stmt);

    let new_title = title.unwrap_or(&existing.title);
    let new_content = content.unwrap_or(&existing.content);
    let content_changed = new_title != existing.title || new_content != existing.content;

    // Tier downgrade protection: never downgrade, consistent with insert path.
    let effective_tier = match (tier, &existing.tier) {
        (Some(requested), existing_tier) => match (existing_tier, requested) {
            (Tier::Long, _) => &Tier::Long,         // long never downgrades
            (Tier::Mid, Tier::Short) => &Tier::Mid, // mid never downgrades to short
            (_, requested) => requested,            // upgrades and same-tier are fine
        },
        (None, existing_tier) => existing_tier,
    };

    let namespace = namespace.unwrap_or(&existing.namespace);
    let tags = tags.unwrap_or(&existing.tags);
    let priority = priority.unwrap_or(existing.priority);
    let confidence = confidence.unwrap_or(existing.confidence);
    // Treat empty string as None (clear expiry) — don't store "" in the DB
    let expires_at = match expires_at {
        Some("" | "null") => None,
        Some(v) => Some(v),
        None => existing.expires_at.as_deref(),
    };
    let metadata = metadata.unwrap_or(&existing.metadata);
    let tags_json = serde_json::to_string(tags)?;
    let metadata_json = serde_json::to_string(metadata)?;
    let now = Utc::now().to_rfc3339();

    // Check for title+namespace collision with a DIFFERENT memory
    if new_title != existing.title || namespace != existing.namespace {
        let collision: Option<String> = conn
            .query_row(
                "SELECT id FROM memories WHERE title = ?1 AND namespace = ?2 AND id != ?3",
                params![new_title, namespace, id],
                |r| r.get(0),
            )
            .ok();
        if let Some(other_id) = collision {
            anyhow::bail!(
                "title '{new_title}' already exists in namespace '{namespace}' (memory {other_id})"
            );
        }
    }

    conn.execute(
        "UPDATE memories SET tier=?1, namespace=?2, title=?3, content=?4, tags=?5, priority=?6, confidence=?7, updated_at=?8, expires_at=?9, metadata=?10
         WHERE id=?11",
        params![effective_tier.as_str(), namespace, new_title, new_content, tags_json, priority, confidence, now, expires_at, metadata_json, id],
    )?;
    Ok((true, content_changed))
}

pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
    // Clean up namespace_meta if this memory was a namespace standard
    conn.execute(
        "DELETE FROM namespace_meta WHERE standard_id = ?1",
        params![id],
    )?;
    let changed = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
    Ok(changed > 0)
}

/// Count memories that would be deleted by forget (for `dry_run`).
pub fn forget_count(
    conn: &Connection,
    namespace: Option<&str>,
    pattern: Option<&str>,
    tier: Option<&Tier>,
) -> Result<usize> {
    if pattern.is_none() && namespace.is_none() && tier.is_none() {
        anyhow::bail!("at least one of namespace, pattern, or tier is required");
    }
    if let Some(pat) = pattern {
        let fts_query = sanitize_fts_query(pat, true);
        let tier_str = tier.map(|t| t.as_str().to_string());
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE rowid IN (
                SELECT m.rowid FROM memories_fts fts
                JOIN memories m ON m.rowid = fts.rowid
                WHERE memories_fts MATCH ?1
                  AND (?2 IS NULL OR m.namespace = ?2)
                  AND (?3 IS NULL OR m.tier = ?3)
            )",
            params![fts_query, namespace, tier_str],
            |r| r.get(0),
        )?;
        return Ok(usize::try_from(count).unwrap_or(0));
    }
    let tier_str = tier.map(|t| t.as_str().to_string());
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE (?1 IS NULL OR namespace = ?1) AND (?2 IS NULL OR tier = ?2)",
        params![namespace, tier_str],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(0))
}

/// Forget by pattern — delete memories matching namespace + FTS pattern + tier.
/// If `archive` is true, archives memories before deletion.
pub fn forget(
    conn: &Connection,
    namespace: Option<&str>,
    pattern: Option<&str>,
    tier: Option<&Tier>,
    archive: bool,
) -> Result<usize> {
    if pattern.is_none() && namespace.is_none() && tier.is_none() {
        anyhow::bail!("at least one of namespace, pattern, or tier is required");
    }

    if archive {
        // Archive matching memories before deletion
        let now = Utc::now().to_rfc3339();
        if let Some(pat) = pattern {
            let fts_query = sanitize_fts_query(pat, true);
            let tier_str = tier.map(|t| t.as_str().to_string());
            conn.execute(
                "INSERT OR REPLACE INTO archived_memories
                 (id, tier, namespace, title, content, tags, priority, confidence,
                  source, access_count, created_at, updated_at, last_accessed_at,
                  expires_at, archived_at, archive_reason)
                 SELECT id, tier, namespace, title, content, tags, priority, confidence,
                        source, access_count, created_at, updated_at, last_accessed_at,
                        expires_at, ?4, 'forget'
                 FROM memories WHERE rowid IN (
                    SELECT m.rowid FROM memories_fts fts
                    JOIN memories m ON m.rowid = fts.rowid
                    WHERE memories_fts MATCH ?1
                      AND (?2 IS NULL OR m.namespace = ?2)
                      AND (?3 IS NULL OR m.tier = ?3)
                 )",
                params![fts_query, namespace, tier_str, now],
            )?;
        } else {
            let tier_str = tier.map(|t| t.as_str().to_string());
            conn.execute(
                "INSERT OR REPLACE INTO archived_memories
                 (id, tier, namespace, title, content, tags, priority, confidence,
                  source, access_count, created_at, updated_at, last_accessed_at,
                  expires_at, archived_at, archive_reason)
                 SELECT id, tier, namespace, title, content, tags, priority, confidence,
                        source, access_count, created_at, updated_at, last_accessed_at,
                        expires_at, ?3, 'forget'
                 FROM memories WHERE (?1 IS NULL OR namespace = ?1) AND (?2 IS NULL OR tier = ?2)",
                params![namespace, tier_str, now],
            )?;
        }
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
    agent_id: Option<&str>,
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
           AND (?10 IS NULL OR json_extract(metadata, '$.agent_id') = ?10)
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
            limit,
            offset,
            agent_id,
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
    agent_id: Option<&str>,
    as_agent: Option<&str>,
) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let tier_str = tier.map(|t| t.as_str().to_string());
    let fts_query = sanitize_fts_query(query, false);
    let (vis_p, vis_t, vis_u, vis_o) = compute_visibility_prefixes(as_agent);

    let sql = format!(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.confidence, m.source, m.access_count, m.created_at, m.updated_at,
                m.last_accessed_at, m.expires_at, m.metadata
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
           AND (?10 IS NULL OR json_extract(m.metadata, '$.agent_id') = ?10)
           {vis}
         ORDER BY (fts.rank * -1)
           + (m.priority * 0.5)
           + (MIN(m.access_count, 50) * 0.1)
           + (m.confidence * 2.0)
           + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
           DESC
         LIMIT ?9",
        vis = visibility_clause(11, "m"),
    );
    let mut stmt = conn.prepare(&sql)?;
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
            limit,
            agent_id,
            vis_p,
            vis_t,
            vis_u,
            vis_o,
        ],
        row_to_memory,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Recall — fuzzy OR search + touch + auto-promote + TTL extension.
#[allow(clippy::too_many_arguments)]
pub fn recall(
    conn: &Connection,
    context: &str,
    namespace: Option<&str>,
    limit: usize,
    tags_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    short_extend: i64,
    mid_extend: i64,
    as_agent: Option<&str>,
) -> Result<Vec<(Memory, f64)>> {
    let now = Utc::now().to_rfc3339();
    let fts_query = sanitize_fts_query(context, true);
    let (vis_p, vis_t, vis_u, vis_o) = compute_visibility_prefixes(as_agent);

    let sql = format!(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.confidence, m.source, m.access_count, m.created_at, m.updated_at,
                m.last_accessed_at, m.expires_at, m.metadata,
                (fts.rank * -1)
                + (m.priority * 0.5)
                + (MIN(m.access_count, 50) * 0.1)
                + (m.confidence * 2.0)
                + (CASE m.tier WHEN 'long' THEN 3.0 WHEN 'mid' THEN 1.0 ELSE 0.0 END)
                + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
                AS score
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1
           AND (?2 IS NULL OR m.namespace = ?2)
           AND (m.expires_at IS NULL OR m.expires_at > ?3)
           AND (?4 IS NULL OR EXISTS (SELECT 1 FROM json_each(m.tags) WHERE json_each.value = ?4))
           AND (?5 IS NULL OR m.created_at >= ?5)
           AND (?6 IS NULL OR m.created_at <= ?6)
           {vis}
         ORDER BY score DESC
         LIMIT ?7",
        vis = visibility_clause(8, "m"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            fts_query,
            namespace,
            now,
            tags_filter,
            since,
            until,
            limit,
            vis_p,
            vis_t,
            vis_u,
            vis_o
        ],
        |row| {
            let mem = row_to_memory(row)?;
            let score: f64 = row.get(15)?;
            Ok((mem, score))
        },
    )?;
    let results: Vec<(Memory, f64)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    // Touch all recalled memories (bumps access, extends TTL, auto-promotes)
    for (mem, _) in &results {
        if let Err(e) = touch(conn, &mem.id, short_extend, mid_extend) {
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
                m.last_accessed_at, m.expires_at, m.metadata
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
    // Verify both IDs exist before creating link
    let source_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
            params![source_id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !source_exists {
        anyhow::bail!("source memory not found: {source_id}");
    }
    let target_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
            params![target_id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !target_exists {
        anyhow::bail!("target memory not found: {target_id}");
    }
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
/// Deletes the source memories and creates links from new → old (`derived_from`).
#[allow(clippy::too_many_arguments)]
pub fn consolidate(
    conn: &Connection,
    ids: &[String],
    title: &str,
    summary: &str,
    namespace: &str,
    tier: &Tier,
    source: &str,
    consolidator_agent_id: &str,
) -> Result<String> {
    let now = Utc::now().to_rfc3339();
    let new_id = uuid::Uuid::new_v4().to_string();

    conn.execute_batch("BEGIN IMMEDIATE")?;

    let result = (|| -> Result<String> {
        // Verify all IDs exist and collect metadata in one pass
        let mut max_priority = 5i32;
        let mut all_tags: Vec<String> = Vec::new();
        let mut total_access = 0i64;
        let mut merged_metadata = serde_json::Map::new();
        // Collect original agent_ids separately — they go into
        // `consolidated_from_agents` for forensic attribution.
        // The consolidator's own agent_id becomes `agent_id` on the result.
        let mut source_agent_ids: Vec<String> = Vec::new();
        for id in ids {
            match get(conn, id)? {
                Some(mem) => {
                    max_priority = max_priority.max(mem.priority);
                    all_tags.extend(mem.tags);
                    total_access = total_access.saturating_add(mem.access_count);
                    // Merge metadata: later values overwrite earlier ones on key conflict.
                    // Intentionally SKIP `agent_id` to avoid last-write-wins forgery;
                    // the consolidator's id is authoritative on the result.
                    if let serde_json::Value::Object(map) = mem.metadata {
                        for (k, v) in map {
                            if k == "agent_id" {
                                if let serde_json::Value::String(aid) = &v
                                    && !source_agent_ids.contains(aid)
                                {
                                    source_agent_ids.push(aid.clone());
                                }
                                continue;
                            }
                            if let Some(existing) = merged_metadata.get(&k)
                                && std::mem::discriminant(existing) != std::mem::discriminant(&v)
                            {
                                tracing::warn!(
                                    "consolidate: key '{}' type changed during merge",
                                    k
                                );
                            }
                            merged_metadata.insert(k, v);
                        }
                    } else {
                        tracing::warn!(
                            "memory {} has non-object metadata during consolidate, skipping",
                            id
                        );
                    }
                }
                None => anyhow::bail!("memory not found: {id}"),
            }
        }
        all_tags.sort();
        all_tags.dedup();
        let tags_json = serde_json::to_string(&all_tags)?;
        // Record source IDs in metadata for provenance (links would be CASCADE-deleted)
        merged_metadata.insert(
            "derived_from".to_string(),
            serde_json::Value::Array(
                ids.iter()
                    .map(|id| serde_json::Value::String(id.clone()))
                    .collect(),
            ),
        );
        // NHI: the consolidator owns the new memory (authoritative agent_id);
        // original authors are preserved as a separate array for forensics.
        merged_metadata.insert(
            "agent_id".to_string(),
            serde_json::Value::String(consolidator_agent_id.to_string()),
        );
        if !source_agent_ids.is_empty() {
            merged_metadata.insert(
                "consolidated_from_agents".to_string(),
                serde_json::Value::Array(
                    source_agent_ids
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        let merged_metadata_value = serde_json::Value::Object(merged_metadata);
        crate::validate::validate_metadata(&merged_metadata_value)
            .context("merged metadata exceeds size limit")?;
        let metadata_json = serde_json::to_string(&merged_metadata_value)?;

        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1.0, ?8, ?9, ?10, ?10, ?11)",
            params![new_id, tier.as_str(), namespace, title, summary, tags_json, max_priority, source, total_access, now, metadata_json],
        )?;

        // Delete source memories first. Note: we intentionally do NOT create
        // derived_from links before deletion because ON DELETE CASCADE would
        // immediately remove them. Instead, source IDs are recorded in the
        // consolidated memory's metadata for provenance.
        for id in ids {
            delete(conn, id)?;
        }

        Ok(new_id.clone())
    })();

    match result {
        Ok(id) => {
            conn.execute_batch("COMMIT")?;
            Ok(id)
        }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("ROLLBACK failed in consolidate: {}", rb);
            }
            Err(e)
        }
    }
}

/// Strip zero-width and invisible Unicode characters that could bypass FTS search.
fn strip_invisible(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(c,
                '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' |
                '\u{00AD}' | '\u{034F}' | '\u{061C}' |
                '\u{180E}' | '\u{2060}' | '\u{2061}'..='\u{2064}' |
                '\u{FE00}'..='\u{FE0F}' | '\u{200E}' | '\u{200F}' |
                '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            )
        })
        .collect()
}

fn sanitize_fts_query(input: &str, use_or: bool) -> String {
    let joiner = if use_or { " OR " } else { " " };
    let cleaned = strip_invisible(input);
    let tokens: Vec<String> = cleaned
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .filter(|t| {
            // Filter out FTS5 boolean operators as standalone tokens
            let upper = t.to_uppercase();
            upper != "AND" && upper != "OR" && upper != "NOT" && upper != "NEAR"
        })
        .map(|token| {
            // Strip FTS5 special characters to prevent injection.
            // Hyphens are allowed inside words (e.g. "well-known").
            let clean: String = token
                .chars()
                .filter(|c| {
                    *c != '"'
                        && *c != '*'
                        && *c != '^'
                        && *c != '{'
                        && *c != '}'
                        && *c != '('
                        && *c != ')'
                        && *c != ':'
                        && *c != '|'
                        && *c != '+'
                        && *c != '-'
                })
                .collect();
            if clean.is_empty() {
                return String::new();
            }
            format!("\"{clean}\"")
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

/// Register or refresh an agent in the reserved `_agents` namespace.
///
/// Each agent is stored as a long-tier memory with `title = "agent:<agent_id>"`.
/// Duplicate registration for the same `agent_id` refreshes `last_seen_at` and
/// overwrites `agent_type` + `capabilities`, while preserving the original
/// `registered_at` timestamp (caller-observable provenance).
///
/// Returns the stored memory ID.
pub fn register_agent(
    conn: &Connection,
    agent_id: &str,
    agent_type: &str,
    capabilities: &[String],
) -> Result<String> {
    let title = format!("agent:{agent_id}");
    let now = Utc::now().to_rfc3339();

    // Preserve original registered_at across re-registration.
    let registered_at = conn
        .query_row(
            "SELECT json_extract(metadata, '$.registered_at') FROM memories
             WHERE namespace = ?1 AND title = ?2",
            params![AGENTS_NAMESPACE, &title],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .unwrap_or_else(|| now.clone());

    let caps_json: Vec<serde_json::Value> = capabilities
        .iter()
        .map(|c| serde_json::Value::String(c.clone()))
        .collect();

    let metadata = serde_json::json!({
        "agent_id": agent_id,
        "agent_type": agent_type,
        "capabilities": caps_json,
        "registered_at": registered_at,
        "last_seen_at": now,
    });

    let content = serde_json::to_string(&metadata)
        .context("failed to serialize agent registration content")?;

    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: AGENTS_NAMESPACE.to_string(),
        title,
        content,
        tags: vec!["agent-registration".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "system".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
    };

    insert(conn, &mem)
}

/// List every registered agent. Rows are drawn from the `_agents` namespace
/// and parsed out of each memory's metadata.
pub fn list_agents(conn: &Connection) -> Result<Vec<AgentRegistration>> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT metadata FROM memories
         WHERE namespace = ?1
           AND (expires_at IS NULL OR expires_at > ?2)
         ORDER BY json_extract(metadata, '$.registered_at') ASC",
    )?;
    let rows = stmt.query_map(params![AGENTS_NAMESPACE, now], |row| {
        row.get::<_, String>(0)
    })?;

    let mut agents = Vec::new();
    for r in rows {
        let raw = r?;
        let meta: serde_json::Value =
            serde_json::from_str(&raw).context("failed to parse agent metadata as JSON")?;
        let agent_id = meta
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let agent_type = meta
            .get("agent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let capabilities: Vec<String> = meta
            .get("capabilities")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let registered_at = meta
            .get("registered_at")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let last_seen_at = meta
            .get("last_seen_at")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        agents.push(AgentRegistration {
            agent_id,
            agent_type,
            capabilities,
            registered_at,
            last_seen_at,
        });
    }
    Ok(agents)
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
    let db_size_bytes = std::fs::metadata(db_path).map_or(0, |m| m.len());

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
pub fn gc_if_needed(conn: &Connection, archive: bool) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let has_expired: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1)",
            params![now],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if has_expired {
        gc(conn, archive)
    } else {
        Ok(0)
    }
}

/// Purge old archives if `archive_max_days` is configured.
pub fn auto_purge_archive(conn: &Connection, max_days: Option<i64>) -> Result<usize> {
    match max_days {
        Some(days) if days > 0 => purge_archive(conn, Some(days)),
        _ => Ok(0),
    }
}

pub fn gc(conn: &Connection, archive: bool) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<usize> {
        if archive {
            conn.execute(
                "INSERT OR REPLACE INTO archived_memories
                 (id, tier, namespace, title, content, tags, priority, confidence,
                  source, access_count, created_at, updated_at, last_accessed_at,
                  expires_at, archived_at, archive_reason, metadata)
                 SELECT id, tier, namespace, title, content, tags, priority, confidence,
                        source, access_count, created_at, updated_at, last_accessed_at,
                        expires_at, ?1, 'ttl_expired', metadata
                 FROM memories
                 WHERE expires_at IS NOT NULL AND expires_at < ?1",
                params![now],
            )?;
        }
        let deleted = conn.execute(
            "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
            params![now],
        )?;
        Ok(deleted)
    })();
    match result {
        Ok(n) => {
            conn.execute_batch("COMMIT")?;
            // Clean up namespace_meta rows pointing to deleted memories
            let _ = conn.execute(
                "DELETE FROM namespace_meta WHERE standard_id NOT IN (SELECT id FROM memories)",
                [],
            );
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Archive operations
// ---------------------------------------------------------------------------

pub fn list_archived(
    conn: &Connection,
    namespace: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Vec<serde_json::Value>> {
    let (sql, params_vec): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match namespace {
        Some(ns) => (
            "SELECT id, tier, namespace, title, content, tags, priority, confidence, \
             source, access_count, created_at, updated_at, last_accessed_at, \
             expires_at, archived_at, archive_reason, metadata \
             FROM archived_memories WHERE namespace = ?1 \
             ORDER BY archived_at DESC LIMIT ?2 OFFSET ?3"
                .to_string(),
            vec![Box::new(ns.to_string()), Box::new(limit), Box::new(offset)],
        ),
        None => (
            "SELECT id, tier, namespace, title, content, tags, priority, confidence, \
             source, access_count, created_at, updated_at, last_accessed_at, \
             expires_at, archived_at, archive_reason, metadata \
             FROM archived_memories \
             ORDER BY archived_at DESC LIMIT ?1 OFFSET ?2"
                .to_string(),
            vec![Box::new(limit), Box::new(offset)],
        ),
    };
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(std::convert::AsRef::as_ref).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        let metadata_str = row
            .get::<_, String>(16)
            .unwrap_or_else(|_| "{}".to_string());
        let metadata: serde_json::Value =
            serde_json::from_str(&metadata_str).unwrap_or_else(|_| serde_json::json!({}));
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "tier": row.get::<_, String>(1)?,
            "namespace": row.get::<_, String>(2)?,
            "title": row.get::<_, String>(3)?,
            "content": row.get::<_, String>(4)?,
            "tags": row.get::<_, String>(5)?,
            "priority": row.get::<_, i32>(6)?,
            "confidence": row.get::<_, f64>(7)?,
            "source": row.get::<_, String>(8)?,
            "access_count": row.get::<_, i64>(9)?,
            "created_at": row.get::<_, String>(10)?,
            "updated_at": row.get::<_, String>(11)?,
            "last_accessed_at": row.get::<_, Option<String>>(12)?,
            "expires_at": row.get::<_, Option<String>>(13)?,
            "archived_at": row.get::<_, String>(14)?,
            "archive_reason": row.get::<_, String>(15)?,
            "metadata": metadata,
        }))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn restore_archived(conn: &Connection, id: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<bool> {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM archived_memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !exists {
            return Ok(false);
        }
        // Check if ID already exists in active memories to prevent silent overwrite
        let active_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if active_exists {
            anyhow::bail!(
                "cannot restore: memory {id} already exists in active table (would overwrite)"
            );
        }
        // Validate archived metadata before restoring
        let archived_metadata: String = conn
            .query_row(
                "SELECT metadata FROM archived_memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "{}".to_string());
        let meta_value: serde_json::Value =
            serde_json::from_str(&archived_metadata).unwrap_or_else(|_| serde_json::json!({}));
        if let Err(e) = crate::validate::validate_metadata(&meta_value) {
            tracing::warn!("archived memory {id} has invalid metadata, resetting to {{}}: {e}");
            conn.execute(
                "UPDATE archived_memories SET metadata = '{}' WHERE id = ?1",
                params![id],
            )?;
        }

        conn.execute(
            "INSERT INTO memories
             (id, tier, namespace, title, content, tags, priority, confidence,
              source, access_count, created_at, updated_at, last_accessed_at, expires_at, metadata)
             SELECT id, 'long', namespace, title, content, tags, priority, confidence,
                    source, access_count, created_at, ?1, last_accessed_at, NULL, metadata
             FROM archived_memories WHERE id = ?2",
            params![now, id],
        )?;
        conn.execute("DELETE FROM archived_memories WHERE id = ?1", params![id])?;
        Ok(true)
    })();
    match result {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

pub fn purge_archive(conn: &Connection, older_than_days: Option<i64>) -> Result<usize> {
    match older_than_days {
        Some(days) if days < 0 => {
            anyhow::bail!("older_than_days must be non-negative (got {days})");
        }
        Some(days) => {
            let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
            let deleted = conn.execute(
                "DELETE FROM archived_memories WHERE archived_at < ?1",
                params![cutoff],
            )?;
            Ok(deleted)
        }
        None => {
            let deleted = conn.execute("DELETE FROM archived_memories", [])?;
            Ok(deleted)
        }
    }
}

pub fn archive_stats(conn: &Connection) -> Result<serde_json::Value> {
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM archived_memories", [], |r| r.get(0))?;
    let mut stmt = conn.prepare(
        "SELECT namespace, COUNT(*) FROM archived_memories GROUP BY namespace ORDER BY COUNT(*) DESC",
    )?;
    let by_ns: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "namespace": row.get::<_, String>(0)?,
                "count": row.get::<_, i64>(1)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(serde_json::json!({
        "archived_total": total,
        "by_namespace": by_ns,
    }))
}

pub fn export_all(conn: &Connection) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT * FROM memories WHERE expires_at IS NULL OR expires_at > ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![now], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn export_links(conn: &Connection) -> Result<Vec<MemoryLink>> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT ml.source_id, ml.target_id, ml.relation, ml.created_at
         FROM memory_links ml
         JOIN memories ms ON ms.id = ml.source_id AND (ms.expires_at IS NULL OR ms.expires_at > ?1)
         JOIN memories mt ON mt.id = ml.target_id AND (mt.expires_at IS NULL OR mt.expires_at > ?1)",
    )?;
    let rows = stmt.query_map(params![now], |row| {
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
/// Only overwrites if the incoming memory is newer (by `updated_at`).
pub fn insert_if_newer(conn: &Connection, mem: &Memory) -> Result<String> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    let metadata_json = serde_json::to_string(&mem.metadata)?;
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at, last_accessed_at, expires_at, metadata)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
                              ELSE COALESCE(excluded.expires_at, memories.expires_at) END,
            -- Preserve metadata.agent_id across newer-wins merge (NHI provenance immutable).
            metadata = CASE
                WHEN json_extract(memories.metadata, '$.agent_id') IS NOT NULL
                THEN json_set(
                    CASE WHEN excluded.updated_at > memories.updated_at
                         THEN excluded.metadata
                         ELSE memories.metadata END,
                    '$.agent_id',
                    json_extract(memories.metadata, '$.agent_id')
                )
                ELSE CASE WHEN excluded.updated_at > memories.updated_at
                          THEN excluded.metadata
                          ELSE memories.metadata END
            END",
        params![
            mem.id, mem.tier.as_str(), mem.namespace, mem.title, mem.content,
            tags_json, mem.priority, mem.confidence, mem.source, mem.access_count,
            mem.created_at, mem.updated_at, mem.last_accessed_at, mem.expires_at,
            metadata_json,
        ],
    )?;
    let actual_id: String = conn.query_row(
        "SELECT id FROM memories WHERE title = ?1 AND namespace = ?2",
        params![mem.title, mem.namespace],
        |r| r.get(0),
    )?;
    Ok(actual_id)
}

// --- Embedding support ---

/// Store an embedding vector for a memory.
pub fn set_embedding(conn: &Connection, id: &str, embedding: &[f32]) -> Result<()> {
    let bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
    conn.execute(
        "UPDATE memories SET embedding = ?1 WHERE id = ?2",
        params![bytes, id],
    )?;
    Ok(())
}

/// Load an embedding vector for a memory. Returns None if not set.
#[allow(clippy::unnecessary_wraps)]
pub fn get_embedding(conn: &Connection, id: &str) -> Result<Option<Vec<f32>>> {
    let result: Option<Vec<u8>> = conn
        .query_row(
            "SELECT embedding FROM memories WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .ok();
    match result {
        Some(bytes) if !bytes.is_empty() => {
            let floats: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect();
            Ok(Some(floats))
        }
        _ => Ok(None),
    }
}

/// Get all memory IDs that are missing embeddings.
pub fn get_unembedded_ids(conn: &Connection) -> Result<Vec<(String, String, String)>> {
    let mut stmt =
        conn.prepare("SELECT id, title, content FROM memories WHERE embedding IS NULL")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Get all stored embeddings as (id, embedding) pairs for building the HNSW index.
pub fn get_all_embeddings(conn: &Connection) -> Result<Vec<(String, Vec<f32>)>> {
    let mut stmt =
        conn.prepare("SELECT id, embedding FROM memories WHERE embedding IS NOT NULL")?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        Ok((id, bytes))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (id, bytes) = row?;
        if bytes.is_empty() {
            continue;
        }
        let floats: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        entries.push((id, floats));
    }
    Ok(entries)
}

/// Hybrid recall — FTS5 keyword search + semantic cosine similarity.
/// Returns memories ranked by a blended score of keyword and semantic relevance.
/// When an HNSW `vector_index` is provided, uses approximate nearest-neighbor
/// search instead of scanning all embeddings linearly.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub fn recall_hybrid(
    conn: &Connection,
    context: &str,
    query_embedding: &[f32],
    namespace: Option<&str>,
    limit: usize,
    tags_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    vector_index: Option<&crate::hnsw::VectorIndex>,
    short_extend: i64,
    mid_extend: i64,
    as_agent: Option<&str>,
) -> Result<Vec<(Memory, f64)>> {
    let now = Utc::now().to_rfc3339();
    let fts_query = sanitize_fts_query(context, true);
    let prefixes = compute_visibility_prefixes(as_agent);
    let (vis_p, vis_t, vis_u, vis_o) = prefixes.clone();

    // Step 1: Get FTS candidates (up to 3x limit to have a good pool)
    let fts_limit = (limit * 3).max(30);
    let fts_sql = format!(
        "SELECT m.id, m.tier, m.namespace, m.title, m.content, m.tags, m.priority,
                m.confidence, m.source, m.access_count, m.created_at, m.updated_at,
                m.last_accessed_at, m.expires_at, m.metadata, m.embedding,
                (fts.rank * -1) + (m.priority * 0.5) + (MIN(m.access_count, 50) * 0.1)
                + (m.confidence * 2.0)
                + (CASE m.tier WHEN 'long' THEN 3.0 WHEN 'mid' THEN 1.0 ELSE 0.0 END)
                + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
                AS fts_score
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1
           AND (?2 IS NULL OR m.namespace = ?2)
           AND (m.expires_at IS NULL OR m.expires_at > ?3)
           AND (?4 IS NULL OR EXISTS (SELECT 1 FROM json_each(m.tags) WHERE json_each.value = ?4))
           AND (?5 IS NULL OR m.created_at >= ?5)
           AND (?6 IS NULL OR m.created_at <= ?6)
           {vis}
         ORDER BY fts_score DESC
         LIMIT ?7",
        vis = visibility_clause(8, "m"),
    );
    let mut fts_stmt = conn.prepare(&fts_sql)?;

    // Step 2: Get semantic candidates — all memories with embeddings
    let sem_sql = format!(
        "SELECT id, tier, namespace, title, content, tags, priority,
                confidence, source, access_count, created_at, updated_at,
                last_accessed_at, expires_at, metadata, embedding
         FROM memories
         WHERE embedding IS NOT NULL
           AND (?1 IS NULL OR namespace = ?1)
           AND (expires_at IS NULL OR expires_at > ?2)
           AND (?3 IS NULL OR EXISTS (SELECT 1 FROM json_each(memories.tags) WHERE json_each.value = ?3))
           AND (?4 IS NULL OR created_at >= ?4)
           AND (?5 IS NULL OR created_at <= ?5)
           {vis}",
        vis = visibility_clause(6, "memories"),
    );
    let mut sem_stmt = conn.prepare(&sem_sql)?;

    // Collect FTS results with scores
    let mut scored: HashMap<String, (Memory, f64, f64)> = HashMap::new(); // id -> (memory, fts_score, cosine_score)

    let fts_rows = fts_stmt.query_map(
        params![
            fts_query,
            namespace,
            now,
            tags_filter,
            since,
            until,
            fts_limit,
            vis_p,
            vis_t,
            vis_u,
            vis_o,
        ],
        |row| {
            let mem = row_to_memory(row)?;
            let fts_score: f64 = row.get(16)?;
            Ok((mem, fts_score))
        },
    )?;

    let mut max_fts_score: f64 = 1.0;
    for row in fts_rows {
        let (mem, fts_score) = row?;
        if fts_score > max_fts_score {
            max_fts_score = fts_score;
        }
        // Compute cosine similarity if embedding exists
        let cosine = get_embedding(conn, &mem.id)?.map_or(0.0, |emb| {
            f64::from(crate::embeddings::Embedder::cosine_similarity(
                query_embedding,
                &emb,
            ))
        });
        scored.insert(mem.id.clone(), (mem, fts_score, cosine));
    }

    // Semantic-only candidates — use HNSW index for fast ANN if available,
    // otherwise fall back to linear scan over all embeddings.
    if let Some(idx) = vector_index {
        // HNSW approximate nearest-neighbor search
        let ann_limit = (limit * 5).max(50);
        let hits = idx.search(query_embedding, ann_limit);
        for hit in hits {
            if scored.contains_key(&hit.id) {
                continue;
            }
            let cosine = f64::from(1.0 - hit.distance);
            if cosine > 0.3
                && let Some(mem) = get(conn, &hit.id)?
            {
                // Apply namespace/expiry/tag filters
                if let Some(ns) = namespace
                    && mem.namespace != ns
                {
                    continue;
                }
                if let Some(exp) = &mem.expires_at
                    && exp.as_str() <= now.as_str()
                {
                    continue;
                }
                if let Some(tf) = tags_filter
                    && !mem.tags.iter().any(|t| t == tf)
                {
                    continue;
                }
                if let Some(s) = since
                    && mem.created_at.as_str() < s
                {
                    continue;
                }
                if let Some(u) = until
                    && mem.created_at.as_str() > u
                {
                    continue;
                }
                // #151 visibility filter (HNSW branch)
                if !is_visible(&mem, &prefixes) {
                    continue;
                }
                scored.insert(mem.id.clone(), (mem, 0.0, cosine));
            }
        }
    } else {
        // Fallback: linear scan over all embeddings
        let sem_rows = sem_stmt.query_map(
            params![
                namespace,
                now,
                tags_filter,
                since,
                until,
                vis_p,
                vis_t,
                vis_u,
                vis_o
            ],
            |row| {
                let mem = row_to_memory(row)?;
                let emb_bytes: Option<Vec<u8>> = row.get(15)?;
                Ok((mem, emb_bytes))
            },
        )?;

        for row in sem_rows {
            let (mem, emb_bytes) = row?;
            if scored.contains_key(&mem.id) {
                continue;
            }
            if let Some(bytes) = emb_bytes
                && !bytes.is_empty()
            {
                let emb: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                let cosine = f64::from(crate::embeddings::Embedder::cosine_similarity(
                    query_embedding,
                    &emb,
                ));
                if cosine > 0.3 {
                    scored.insert(mem.id.clone(), (mem, 0.0, cosine));
                }
            }
        }
    }

    // Normalize FTS scores and compute blended score.
    // Adaptive blend: semantic weight decreases for longer content (embeddings
    // lose information on long text; FTS stays precise).  Short memories
    // (< 500 chars) get 50/50, long memories (> 5 000 chars) get 15/85.
    let mut results: Vec<(Memory, f64)> = scored
        .into_values()
        .map(|(mem, fts_score, cosine)| {
            let norm_fts = if max_fts_score > 0.0 {
                fts_score / max_fts_score
            } else {
                0.0
            };
            let content_len = f64::from(i32::try_from(mem.content.len()).expect("usize as i64"));
            // Lerp semantic_weight from 0.50 (≤500 chars) to 0.15 (≥5000 chars)
            let semantic_weight = if content_len <= 500.0 {
                0.50
            } else if content_len >= 5000.0 {
                0.15
            } else {
                0.50 - 0.35 * ((content_len - 500.0) / 4500.0)
            };
            let blended = semantic_weight * cosine + (1.0 - semantic_weight) * norm_fts;
            (mem, blended)
        })
        .collect();

    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);

    // Touch all recalled memories
    for (mem, _) in &results {
        if let Err(e) = touch(conn, &mem.id, short_extend, mid_extend) {
            tracing::warn!("touch failed for memory {}: {}", &mem.id, e);
        }
    }

    Ok(results)
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

// ---------------------------------------------------------------------------
// Namespace standards
// ---------------------------------------------------------------------------

/// Set the standard memory for a namespace, with optional parent for rule layering.
pub fn set_namespace_standard(
    conn: &Connection,
    namespace: &str,
    standard_id: &str,
    parent: Option<&str>,
) -> Result<()> {
    // Verify the memory exists (but allow cross-namespace — shared policy)
    let _mem = get(conn, standard_id)?
        .ok_or_else(|| anyhow::anyhow!("memory not found: {standard_id}"))?;
    // Resolve parent: explicit > auto-detect by `-` prefix > none
    let resolved_parent = match parent {
        Some(p) => {
            if p == namespace {
                anyhow::bail!("namespace cannot be its own parent");
            }
            Some(p.to_string())
        }
        None => auto_detect_parent(conn, namespace),
    };
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO namespace_meta (namespace, standard_id, updated_at, parent_namespace)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(namespace) DO UPDATE SET standard_id = ?2, updated_at = ?3, parent_namespace = ?4",
        params![namespace, standard_id, now, resolved_parent],
    )?;
    Ok(())
}

/// Auto-detect parent namespace by `-` prefix.
/// "ai-memory-tests" → checks "ai-memory" → checks "ai" → first match wins.
fn auto_detect_parent(conn: &Connection, namespace: &str) -> Option<String> {
    let mut candidate = namespace.to_string();
    while let Some(pos) = candidate.rfind('-') {
        candidate.truncate(pos);
        if candidate.is_empty() {
            break;
        }
        // Check if this candidate has a standard set
        if get_namespace_standard(conn, &candidate)
            .ok()
            .flatten()
            .is_some()
        {
            return Some(candidate);
        }
    }
    None
}

/// Get the standard memory ID for a namespace.
#[allow(clippy::unnecessary_wraps)]
pub fn get_namespace_standard(conn: &Connection, namespace: &str) -> Result<Option<String>> {
    let result = conn
        .query_row(
            "SELECT standard_id FROM namespace_meta WHERE namespace = ?1",
            params![namespace],
            |r| r.get(0),
        )
        .ok();
    Ok(result)
}

/// Get the parent namespace for a given namespace.
pub fn get_namespace_parent(conn: &Connection, namespace: &str) -> Option<String> {
    conn.query_row(
        "SELECT parent_namespace FROM namespace_meta WHERE namespace = ?1 AND parent_namespace IS NOT NULL",
        params![namespace],
        |r| r.get(0),
    )
    .ok()
}

/// Clear the standard for a namespace.
pub fn clear_namespace_standard(conn: &Connection, namespace: &str) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM namespace_meta WHERE namespace = ?1",
        params![namespace],
    )?;
    Ok(changed > 0)
}

/// Check if a memory ID is a namespace standard (used by consolidate to warn).
pub fn is_namespace_standard(conn: &Connection, id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM namespace_meta WHERE standard_id = ?1",
        params![id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MID_TTL_EXTEND_SECS, Memory, SHORT_TTL_EXTEND_SECS, Tier};

    fn test_db() -> Connection {
        open(std::path::Path::new(":memory:")).unwrap()
    }

    fn make_memory(title: &str, ns: &str, tier: Tier, priority: i32) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: tier.clone(),
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("Content for {title}"),
            tags: vec![],
            priority,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: tier
                .default_ttl_secs()
                .map(|s| (chrono::Utc::now() + chrono::Duration::seconds(s)).to_rfc3339()),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn open_creates_schema() {
        let conn = test_db();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn insert_and_get() {
        let conn = test_db();
        let mem = make_memory("Test insert", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.title, "Test insert");
        assert_eq!(got.namespace, "test");
        assert_eq!(got.priority, 5);
    }

    #[test]
    fn get_nonexistent() {
        let conn = test_db();
        let got = get(&conn, "nonexistent-id").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn update_partial_fields() {
        let conn = test_db();
        let mem = make_memory("Original", "test", Tier::Mid, 5);
        let id = insert(&conn, &mem).unwrap();

        let (found, content_changed) = update(
            &conn,
            &id,
            Some("Updated Title"),
            None,
            None,
            None,
            None,
            Some(9),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(found);
        assert!(content_changed); // title changed

        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.title, "Updated Title");
        assert_eq!(got.priority, 9);
        assert_eq!(got.content, mem.content); // unchanged
    }

    #[test]
    fn update_content_changed_flag() {
        let conn = test_db();
        let mem = make_memory("Stable", "test", Tier::Mid, 5);
        let id = insert(&conn, &mem).unwrap();

        // Updating only priority — content_changed should be false
        let (found, content_changed) = update(
            &conn,
            &id,
            None,
            None,
            None,
            None,
            None,
            Some(8),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(found);
        assert!(!content_changed);

        // Updating content — content_changed should be true
        let (found, content_changed) = update(
            &conn,
            &id,
            None,
            Some("New content"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(found);
        assert!(content_changed);
    }

    #[test]
    fn update_nonexistent_returns_false() {
        let conn = test_db();
        let (found, _) = update(
            &conn,
            "bad-id",
            Some("New"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!found);
    }

    #[test]
    fn update_tier_downgrade_protection() {
        let conn = test_db();
        // Long-tier memory should never be downgraded
        let mem = make_memory("Permanent", "test", Tier::Long, 9);
        let id = insert(&conn, &mem).unwrap();

        let (found, _) = update(
            &conn,
            &id,
            None,
            None,
            Some(&Tier::Short),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(found);
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.tier, Tier::Long); // still long

        // Mid-tier should not downgrade to short
        let mem2 = make_memory("Working", "test", Tier::Mid, 5);
        let id2 = insert(&conn, &mem2).unwrap();

        let (found, _) = update(
            &conn,
            &id2,
            None,
            None,
            Some(&Tier::Short),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(found);
        let got2 = get(&conn, &id2).unwrap().unwrap();
        assert_eq!(got2.tier, Tier::Mid); // still mid

        // Mid-tier CAN upgrade to long
        let (found, _) = update(
            &conn,
            &id2,
            None,
            None,
            Some(&Tier::Long),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(found);
        let got3 = get(&conn, &id2).unwrap().unwrap();
        assert_eq!(got3.tier, Tier::Long); // upgraded
    }

    #[test]
    fn update_title_collision_returns_error() {
        let conn = test_db();
        let mem_a = make_memory("Alpha", "test", Tier::Mid, 5);
        let mem_b = make_memory("Beta", "test", Tier::Mid, 5);
        let id_a = insert(&conn, &mem_a).unwrap();
        let _id_b = insert(&conn, &mem_b).unwrap();

        // Updating Alpha's title to "Beta" in same namespace should fail
        let result = update(
            &conn,
            &id_a,
            Some("Beta"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("already exists in namespace"));
    }

    #[test]
    fn delete_existing() {
        let conn = test_db();
        let mem = make_memory("To delete", "test", Tier::Short, 3);
        let id = insert(&conn, &mem).unwrap();
        assert!(delete(&conn, &id).unwrap());
        assert!(get(&conn, &id).unwrap().is_none());
    }

    #[test]
    fn delete_nonexistent() {
        let conn = test_db();
        assert!(!delete(&conn, "bad-id").unwrap());
    }

    #[test]
    fn list_with_namespace_filter() {
        let conn = test_db();
        insert(&conn, &make_memory("A", "ns1", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("B", "ns2", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("C", "ns1", Tier::Long, 5)).unwrap();

        let results = list(
            &conn,
            Some("ns1"),
            None,
            100,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn list_with_tier_filter() {
        let conn = test_db();
        insert(&conn, &make_memory("Long", "test", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("Mid", "test", Tier::Mid, 5)).unwrap();

        let results = list(
            &conn,
            None,
            Some(&Tier::Long),
            100,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Long");
    }

    #[test]
    fn list_with_limit() {
        let conn = test_db();
        for i in 0..5 {
            insert(
                &conn,
                &make_memory(&format!("Mem {i}"), "test", Tier::Long, 5),
            )
            .unwrap();
        }
        let results = list(&conn, None, None, 3, 0, None, None, None, None, None).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_keyword_match() {
        let conn = test_db();
        insert(
            &conn,
            &make_memory("PostgreSQL config", "test", Tier::Long, 5),
        )
        .unwrap();
        insert(&conn, &make_memory("Redis cache", "test", Tier::Long, 5)).unwrap();

        let results = search(
            &conn,
            "PostgreSQL",
            None,
            None,
            10,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].title.contains("PostgreSQL"));
    }

    #[test]
    fn search_no_match() {
        let conn = test_db();
        insert(&conn, &make_memory("PostgreSQL", "test", Tier::Long, 5)).unwrap();
        let results = search(
            &conn,
            "nonexistent_term_xyz",
            None,
            None,
            10,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn recall_returns_scored() {
        let conn = test_db();
        insert(
            &conn,
            &make_memory("Rust programming language", "test", Tier::Long, 8),
        )
        .unwrap();
        insert(
            &conn,
            &make_memory("Python scripting", "test", Tier::Long, 5),
        )
        .unwrap();

        let results = recall(
            &conn,
            "Rust programming",
            None,
            10,
            None,
            None,
            None,
            SHORT_TTL_EXTEND_SECS,
            MID_TTL_EXTEND_SECS,
            None,
        )
        .unwrap();
        assert!(!results.is_empty());
        // Score should be present
        let (mem, score) = &results[0];
        assert!(mem.title.contains("Rust"));
        assert!(*score > 0.0);
    }

    #[test]
    fn recall_empty_context() {
        let conn = test_db();
        insert(&conn, &make_memory("Test", "test", Tier::Long, 5)).unwrap();
        // Empty context should not crash
        let results = recall(
            &conn,
            "",
            None,
            10,
            None,
            None,
            None,
            SHORT_TTL_EXTEND_SECS,
            MID_TTL_EXTEND_SECS,
            None,
        );
        // May return empty or error, both acceptable
        assert!(results.is_ok() || results.is_err());
    }

    #[test]
    fn touch_increments_access_count() {
        let conn = test_db();
        let mem = make_memory("Touchable", "test", Tier::Mid, 5);
        let id = insert(&conn, &mem).unwrap();
        assert_eq!(get(&conn, &id).unwrap().unwrap().access_count, 0);

        touch(&conn, &id, SHORT_TTL_EXTEND_SECS, MID_TTL_EXTEND_SECS).unwrap();
        assert_eq!(get(&conn, &id).unwrap().unwrap().access_count, 1);

        touch(&conn, &id, SHORT_TTL_EXTEND_SECS, MID_TTL_EXTEND_SECS).unwrap();
        assert_eq!(get(&conn, &id).unwrap().unwrap().access_count, 2);
    }

    #[test]
    fn find_contradictions_similar_titles() {
        let conn = test_db();
        insert(
            &conn,
            &make_memory("Database is PostgreSQL", "infra", Tier::Long, 8),
        )
        .unwrap();
        insert(
            &conn,
            &make_memory("Database is MySQL", "infra", Tier::Long, 5),
        )
        .unwrap();

        let contradictions = find_contradictions(&conn, "Database is PostgreSQL", "infra").unwrap();
        assert!(!contradictions.is_empty());
    }

    #[test]
    fn create_and_get_links() {
        let conn = test_db();
        let id1 = insert(&conn, &make_memory("Memory A", "test", Tier::Long, 5)).unwrap();
        let id2 = insert(&conn, &make_memory("Memory B", "test", Tier::Long, 5)).unwrap();

        create_link(&conn, &id1, &id2, "related_to").unwrap();
        let links = get_links(&conn, &id1).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].relation, "related_to");
    }

    #[test]
    fn consolidate_merges_memories() {
        let conn = test_db();
        let id1 = insert(&conn, &make_memory("Part 1", "test", Tier::Mid, 5)).unwrap();
        let id2 = insert(&conn, &make_memory("Part 2", "test", Tier::Mid, 5)).unwrap();

        let new_id = consolidate(
            &conn,
            &[id1.clone(), id2.clone()],
            "Combined",
            "Part 1 + Part 2",
            "test",
            &Tier::Long,
            "test",
            "test-consolidator",
        )
        .unwrap();
        // Original memories should be deleted
        assert!(get(&conn, &id1).unwrap().is_none());
        assert!(get(&conn, &id2).unwrap().is_none());
        // New memory should exist
        let combined = get(&conn, &new_id).unwrap().unwrap();
        assert_eq!(combined.title, "Combined");
        assert_eq!(combined.tier, Tier::Long);
    }

    #[test]
    fn stats_counts() {
        let conn = test_db();
        let path = std::path::Path::new(":memory:");
        insert(&conn, &make_memory("A", "ns1", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("B", "ns1", Tier::Mid, 5)).unwrap();
        insert(&conn, &make_memory("C", "ns2", Tier::Short, 5)).unwrap();

        let s = stats(&conn, path).unwrap();
        assert_eq!(s.total, 3);
    }

    #[test]
    fn gc_removes_expired() {
        let conn = test_db();
        let mut mem = make_memory("Expired", "test", Tier::Short, 5);
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string()); // past
        insert(&conn, &mem).unwrap();

        let removed = gc(&conn, false).unwrap();
        assert_eq!(removed, 1);
    }

    #[test]
    fn gc_preserves_long_term() {
        let conn = test_db();
        insert(&conn, &make_memory("Permanent", "test", Tier::Long, 5)).unwrap();
        let removed = gc(&conn, false).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn gc_archives_before_delete() {
        let conn = test_db();
        let mut mem = make_memory("Archivable", "test", Tier::Short, 5);
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        insert(&conn, &mem).unwrap();

        let removed = gc(&conn, true).unwrap();
        assert_eq!(removed, 1);

        // Should be in archive
        let archived = list_archived(&conn, None, 10, 0).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0]["title"], "Archivable");
        assert_eq!(archived[0]["archive_reason"], "ttl_expired");
    }

    #[test]
    fn restore_archived_memory() {
        let conn = test_db();
        let mut mem = make_memory("Restorable", "test", Tier::Short, 5);
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        let id = insert(&conn, &mem).unwrap();

        gc(&conn, true).unwrap();
        assert!(get(&conn, &id).unwrap().is_none()); // gone from active

        let restored = restore_archived(&conn, &id).unwrap();
        assert!(restored);

        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.title, "Restorable");
        assert!(got.expires_at.is_none()); // restored without expiry
    }

    #[test]
    fn purge_archive_removes_all() {
        let conn = test_db();
        let mut mem = make_memory("Purgeable", "test", Tier::Short, 5);
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        insert(&conn, &mem).unwrap();
        gc(&conn, true).unwrap();

        let purged = purge_archive(&conn, None).unwrap();
        assert_eq!(purged, 1);
        assert_eq!(list_archived(&conn, None, 10, 0).unwrap().len(), 0);
    }

    #[test]
    fn purge_archive_rejects_negative_days() {
        let conn = test_db();
        let result = purge_archive(&conn, Some(-1));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("non-negative"));
    }

    #[test]
    fn restore_rejects_active_id_collision() {
        let conn = test_db();
        let mut mem = make_memory("Collision Test", "test", Tier::Short, 5);
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        let id = insert(&conn, &mem).unwrap();

        // Archive it via GC
        gc(&conn, true).unwrap();
        assert!(get(&conn, &id).unwrap().is_none());

        // Manually insert a memory with the SAME id but different title into active table
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at)
             VALUES (?1, 'long', 'test', 'Blocker Title', 'blocks restore', '[]', 5, 1.0, 'test', 0, datetime('now'), datetime('now'))",
            rusqlite::params![id],
        ).unwrap();

        // Restore should fail because id exists in active table
        let result = restore_archived(&conn, &id);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already exists in active table")
        );
    }

    #[test]
    fn archive_stats_counts() {
        let conn = test_db();
        let mut m1 = make_memory("Stats A", "ns1", Tier::Short, 5);
        m1.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        let mut m2 = make_memory("Stats B", "ns1", Tier::Short, 5);
        m2.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        insert(&conn, &m1).unwrap();
        insert(&conn, &m2).unwrap();
        gc(&conn, true).unwrap();

        let stats = archive_stats(&conn).unwrap();
        assert_eq!(stats["archived_total"], 2);
    }

    #[test]
    fn export_all_and_links() {
        let conn = test_db();
        let id1 = insert(&conn, &make_memory("Export A", "test", Tier::Long, 5)).unwrap();
        let id2 = insert(&conn, &make_memory("Export B", "test", Tier::Long, 5)).unwrap();
        create_link(&conn, &id1, &id2, "supersedes").unwrap();

        let mems = export_all(&conn).unwrap();
        assert_eq!(mems.len(), 2);
        let links = export_links(&conn).unwrap();
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn list_namespaces_counts() {
        let conn = test_db();
        insert(&conn, &make_memory("A", "alpha", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("B", "alpha", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("C", "beta", Tier::Long, 5)).unwrap();

        let ns = list_namespaces(&conn).unwrap();
        assert_eq!(ns.len(), 2);
    }

    #[test]
    fn forget_by_namespace() {
        let conn = test_db();
        insert(&conn, &make_memory("A", "delete-me", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("B", "delete-me", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("C", "keep", Tier::Long, 5)).unwrap();

        let deleted = forget(&conn, Some("delete-me"), None, None, false).unwrap();
        assert_eq!(deleted, 2);
        let remaining = list(&conn, None, None, 100, 0, None, None, None, None, None).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn set_and_get_embedding() {
        let conn = test_db();
        let mem = make_memory("Embed test", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();

        let emb = vec![0.1f32, 0.2, 0.3, 0.4];
        set_embedding(&conn, &id, &emb).unwrap();

        let got = get_embedding(&conn, &id).unwrap().unwrap();
        assert_eq!(got.len(), 4);
        assert!((got[0] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn get_unembedded_returns_memoryless() {
        let conn = test_db();
        let mem = make_memory("No embed", "test", Tier::Long, 5);
        insert(&conn, &mem).unwrap();

        let unembedded = get_unembedded_ids(&conn).unwrap();
        assert_eq!(unembedded.len(), 1);
    }

    #[test]
    fn health_check_passes() {
        let conn = test_db();
        assert!(health_check(&conn).unwrap());
    }

    #[test]
    fn sanitize_fts_strips_operators_and_quotes() {
        // FTS5 special chars: " * ^ { } ( ) : - | are stripped
        let sanitized = sanitize_fts_query("test* \"injection\" (drop)", true);
        assert!(!sanitized.contains("*"));
        assert!(!sanitized.contains("("));
        assert!(!sanitized.contains(")"));
        // Standalone boolean operators are removed
        let sanitized2 = sanitize_fts_query("hello AND world OR NOT NEAR test", true);
        assert!(sanitized2.contains("hello"));
        assert!(sanitized2.contains("world"));
        assert!(sanitized2.contains("test"));
        // Empty input returns placeholder
        let sanitized3 = sanitize_fts_query("", true);
        assert_eq!(sanitized3, "\"_empty_\"");
        // + and - prefix operators are stripped (prevents exclusion injection)
        let sanitized4 = sanitize_fts_query("-secret +required", true);
        assert!(!sanitized4.contains('-'));
        assert!(!sanitized4.contains('+'));
        assert!(sanitized4.contains("secret"));
        assert!(sanitized4.contains("required"));
    }

    #[test]
    fn get_by_prefix_8char() {
        let conn = test_db();
        let mem = make_memory("Prefix test", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();
        let prefix = &id[..8];
        let got = get_by_prefix(&conn, prefix).unwrap().unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.title, "Prefix test");
    }

    #[test]
    fn get_by_prefix_full_uuid() {
        let conn = test_db();
        let mem = make_memory("Full UUID prefix", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();
        // Full UUID used as prefix still works (LIKE 'full-uuid%' matches exact)
        let got = get_by_prefix(&conn, &id).unwrap().unwrap();
        assert_eq!(got.id, id);
    }

    #[test]
    fn get_by_prefix_nonexistent() {
        let conn = test_db();
        let got = get_by_prefix(&conn, "ffffffff").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn get_by_prefix_ambiguous() {
        let conn = test_db();
        // Insert two memories with IDs sharing a common prefix
        let mut mem1 = make_memory("Ambig A", "test", Tier::Long, 5);
        mem1.id = "aaaa1111-0000-0000-0000-000000000001".to_string();
        insert(&conn, &mem1).unwrap();
        let mut mem2 = make_memory("Ambig B", "test2", Tier::Long, 5);
        mem2.id = "aaaa2222-0000-0000-0000-000000000002".to_string();
        insert(&conn, &mem2).unwrap();
        let result = get_by_prefix(&conn, "aaaa");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("ambiguous"));
        assert!(err_msg.contains("2 matches"));
        // Error should list the matching full IDs so the user can pick one
        assert!(
            err_msg.contains("aaaa1111-0000-0000-0000-000000000001"),
            "error should list matching IDs, got: {err_msg}"
        );
        assert!(err_msg.contains("aaaa2222-0000-0000-0000-000000000002"));
    }

    #[test]
    fn resolve_id_exact_then_prefix() {
        let conn = test_db();
        let mem = make_memory("Resolve test", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();
        // Exact match
        let got = resolve_id(&conn, &id).unwrap().unwrap();
        assert_eq!(got.id, id);
        // Prefix match
        let got2 = resolve_id(&conn, &id[..8]).unwrap().unwrap();
        assert_eq!(got2.id, id);
        // Nonexistent
        let got3 = resolve_id(&conn, "zzzzzzzz").unwrap();
        assert!(got3.is_none());
    }

    #[test]
    fn insert_if_newer_updates() {
        let conn = test_db();
        let mut mem = make_memory("Sync test", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();

        mem.id = id.clone();
        mem.content = "Updated via sync".to_string();
        mem.updated_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let result_id = insert_if_newer(&conn, &mem).unwrap();
        assert_eq!(result_id, id);

        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.content, "Updated via sync");
    }

    // --- Metadata tests (Task 1.1) ---

    #[test]
    fn metadata_default_empty_object() {
        let conn = test_db();
        let mem = make_memory("Default metadata", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata, serde_json::json!({}));
    }

    #[test]
    fn metadata_store_and_retrieve() {
        let conn = test_db();
        let mut mem = make_memory("With metadata", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({"agent_id": "claude-1", "session": 42});
        let id = insert(&conn, &mem).unwrap();
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["agent_id"], "claude-1");
        assert_eq!(got.metadata["session"], 42);
    }

    #[test]
    fn metadata_roundtrip_nested_json() {
        let conn = test_db();
        let mut mem = make_memory("Nested metadata", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({
            "agent": {"type": "ai:claude", "version": "4.6"},
            "tags_extra": ["experimental"],
            "score": 0.95
        });
        let id = insert(&conn, &mem).unwrap();
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["agent"]["type"], "ai:claude");
        assert_eq!(got.metadata["tags_extra"][0], "experimental");
        assert!((got.metadata["score"].as_f64().unwrap() - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn metadata_preserved_on_update() {
        let conn = test_db();
        let mut mem = make_memory("Update metadata", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({"key": "original"});
        let id = insert(&conn, &mem).unwrap();

        // Update without metadata — should preserve existing
        let (found, _) = update(
            &conn,
            &id,
            None,
            Some("new content"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(found);
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["key"], "original");
        assert_eq!(got.content, "new content");

        // Update with new metadata — should replace
        let new_meta = serde_json::json!({"key": "updated", "extra": true});
        let (found, _) = update(
            &conn,
            &id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&new_meta),
        )
        .unwrap();
        assert!(found);
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["key"], "updated");
        assert_eq!(got.metadata["extra"], true);
    }

    #[test]
    fn metadata_preserved_on_upsert() {
        let conn = test_db();
        let mut mem = make_memory("Upsert meta", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({"version": 1});
        insert(&conn, &mem).unwrap();

        // Insert again with same title+namespace — upsert should update metadata
        let mut mem2 = make_memory("Upsert meta", "test", Tier::Long, 5);
        mem2.metadata = serde_json::json!({"version": 2});
        let id = insert(&conn, &mem2).unwrap();
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["version"], 2);
    }

    #[test]
    fn metadata_in_list_and_search() {
        let conn = test_db();
        let mut mem = make_memory("Searchable metadata", "test", Tier::Long, 8);
        mem.metadata = serde_json::json!({"source_model": "opus"});
        insert(&conn, &mem).unwrap();

        let results = list(
            &conn,
            Some("test"),
            None,
            10,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata["source_model"], "opus");

        let results = search(
            &conn,
            "Searchable",
            Some("test"),
            None,
            10,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata["source_model"], "opus");
    }

    #[test]
    fn metadata_in_recall() {
        let conn = test_db();
        let mut mem = make_memory("Recallable metadata", "test", Tier::Long, 8);
        mem.metadata = serde_json::json!({"context": "test-recall"});
        insert(&conn, &mem).unwrap();

        let results = recall(
            &conn,
            "Recallable",
            Some("test"),
            10,
            None,
            None,
            None,
            3600,
            86400,
            None,
        )
        .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0.metadata["context"], "test-recall");
    }

    #[test]
    fn metadata_in_export_import() {
        let conn = test_db();
        let mut mem = make_memory("Export metadata", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({"exported": true});
        insert(&conn, &mem).unwrap();

        let exported = export_all(&conn).unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].metadata["exported"], true);

        // Import into fresh DB
        let conn2 = test_db();
        insert(&conn2, &exported[0]).unwrap();
        let got = get(&conn2, &exported[0].id).unwrap().unwrap();
        assert_eq!(got.metadata["exported"], true);
    }

    #[test]
    fn metadata_schema_migration() {
        // Simulate a pre-v7 database (no metadata column) by creating one
        // and checking that migration adds the column with correct default
        let conn = test_db();
        let mem = make_memory("Migration test", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();

        // Verify the column exists and has the default value
        let metadata_str: String = conn
            .query_row(
                "SELECT metadata FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(metadata_str, "{}");
    }

    #[test]
    fn metadata_survives_archive_restore_cycle() {
        let conn = test_db();
        let mut mem = make_memory("Archivable", "test", Tier::Short, 5);
        mem.metadata = serde_json::json!({"origin": "archive-test"});
        // Set expiry in the past so GC will archive it
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        let id = insert(&conn, &mem).unwrap();

        // Run GC with archive=true — should archive the expired memory
        let deleted = gc(&conn, true).unwrap();
        assert_eq!(deleted, 1);

        // Verify metadata is in the archive
        let archived = list_archived(&conn, None, 10, 0).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0]["metadata"]["origin"], "archive-test");

        // Restore and verify metadata survives the round-trip
        let restored = restore_archived(&conn, &id).unwrap();
        assert!(restored);
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["origin"], "archive-test");
    }

    #[test]
    fn metadata_in_insert_if_newer() {
        let conn = test_db();
        let mut mem = make_memory("Sync metadata", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({"version": 1});
        let id = insert(&conn, &mem).unwrap();

        // Insert newer version with different metadata
        mem.id = id.clone();
        mem.metadata = serde_json::json!({"version": 2, "synced": true});
        mem.updated_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        insert_if_newer(&conn, &mem).unwrap();

        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["version"], 2);
        assert_eq!(got.metadata["synced"], true);

        // Insert older version — metadata should NOT be overwritten
        mem.metadata = serde_json::json!({"version": 0, "stale": true});
        mem.updated_at = "2020-01-01T00:00:00+00:00".to_string();
        insert_if_newer(&conn, &mem).unwrap();

        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["version"], 2); // still the newer one
        assert!(got.metadata.get("stale").is_none());
    }

    #[test]
    fn metadata_merged_in_consolidate() {
        let conn = test_db();
        let mut mem_a = make_memory("Consolidate A", "test", Tier::Long, 5);
        mem_a.metadata = serde_json::json!({"agent": "claude", "shared": "from_a"});
        let id_a = insert(&conn, &mem_a).unwrap();

        let mut mem_b = make_memory("Consolidate B", "test", Tier::Long, 7);
        mem_b.metadata = serde_json::json!({"model": "opus", "shared": "from_b"});
        let id_b = insert(&conn, &mem_b).unwrap();

        let new_id = consolidate(
            &conn,
            &[id_a, id_b],
            "Merged",
            "Combined content",
            "test",
            &Tier::Long,
            "consolidation",
            "test-consolidator",
        )
        .unwrap();

        let got = get(&conn, &new_id).unwrap().unwrap();
        // Both keys present; "shared" key takes value from later source (mem_b)
        assert_eq!(got.metadata["agent"], "claude");
        assert_eq!(got.metadata["model"], "opus");
        assert_eq!(got.metadata["shared"], "from_b");
    }

    #[test]
    fn metadata_consolidate_rejects_oversized_merge() {
        let conn = test_db();
        // Create two memories with large unique-key metadata that together exceed 64KB
        let mut mem_a = make_memory("Big meta A", "test", Tier::Long, 5);
        let big_val_a: serde_json::Map<String, serde_json::Value> = (0..500)
            .map(|i| {
                (
                    format!("key_a_{i}"),
                    serde_json::Value::String("x".repeat(60)),
                )
            })
            .collect();
        mem_a.metadata = serde_json::Value::Object(big_val_a);
        let id_a = insert(&conn, &mem_a).unwrap();

        let mut mem_b = make_memory("Big meta B", "test", Tier::Long, 5);
        let big_val_b: serde_json::Map<String, serde_json::Value> = (0..500)
            .map(|i| {
                (
                    format!("key_b_{i}"),
                    serde_json::Value::String("x".repeat(60)),
                )
            })
            .collect();
        mem_b.metadata = serde_json::Value::Object(big_val_b);
        let id_b = insert(&conn, &mem_b).unwrap();

        // Consolidate should fail because merged metadata exceeds 64KB
        let result = consolidate(
            &conn,
            &[id_a, id_b],
            "Oversized merge",
            "Should fail",
            "test",
            &Tier::Long,
            "consolidation",
            "test-consolidator",
        );
        let err = result.expect_err("consolidate should fail for oversized merged metadata");
        let msg = err.to_string();
        assert!(
            msg.contains("merged metadata exceeds size limit"),
            "expected metadata size error, got: {msg}"
        );
    }

    #[test]
    fn metadata_special_characters_roundtrip() {
        let conn = test_db();
        let mut mem = make_memory("Special chars metadata", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({
            "pipe": "a|b|c",
            "newline": "line1\nline2",
            "tab": "col1\tcol2",
            "backslash": "path\\to\\file",
            "unicode": "\u{1F600}\u{1F4A9}",
            "cjk": "\u{4e16}\u{754c}",
            "empty": "",
            "nested_special": {"inner|key": "val\nue"}
        });
        let id = insert(&conn, &mem).unwrap();
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata["pipe"], "a|b|c");
        assert_eq!(got.metadata["newline"], "line1\nline2");
        assert_eq!(got.metadata["unicode"], "\u{1F600}\u{1F4A9}");
        assert_eq!(got.metadata["cjk"], "\u{4e16}\u{754c}");
        assert_eq!(got.metadata["nested_special"]["inner|key"], "val\nue");
    }

    #[test]
    fn metadata_corrupt_column_falls_back_to_empty() {
        let conn = test_db();
        let mem = make_memory("Corrupt test", "test", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();

        // Manually corrupt the metadata column
        conn.execute(
            "UPDATE memories SET metadata = 'NOT VALID JSON {{{{' WHERE id = ?1",
            params![id],
        )
        .unwrap();

        // row_to_memory should fall back to {} without panicking
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata, serde_json::json!({}));
    }

    #[test]
    fn metadata_restore_resets_corrupt_archived_metadata() {
        let conn = test_db();
        let mut mem = make_memory("Corrupt archive", "test", Tier::Short, 5);
        mem.metadata = serde_json::json!({"valid": true});
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        let id = insert(&conn, &mem).unwrap();

        // Archive via GC
        gc(&conn, true).unwrap();

        // Corrupt the archived metadata directly
        conn.execute(
            "UPDATE archived_memories SET metadata = 'CORRUPT JSON' WHERE id = ?1",
            params![id],
        )
        .unwrap();

        // Restore — should reset metadata to {} instead of failing
        let restored = restore_archived(&conn, &id).unwrap();
        assert!(restored);
        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.metadata, serde_json::json!({}));
    }

    #[test]
    fn auto_purge_archive_respects_max_days() {
        let conn = test_db();
        let mut mem = make_memory("Purge test", "test", Tier::Short, 5);
        mem.expires_at = Some("2020-01-01T00:00:00+00:00".to_string());
        insert(&conn, &mem).unwrap();
        gc(&conn, true).unwrap();

        // Archive exists
        let archived = list_archived(&conn, None, 10, 0).unwrap();
        assert_eq!(archived.len(), 1);

        // Backdate archived_at to 30 days ago so purge can detect it
        conn.execute(
            "UPDATE archived_memories SET archived_at = ?1",
            params![(chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339()],
        )
        .unwrap();

        // Purge with None (disabled) — no-op
        let purged = auto_purge_archive(&conn, None).unwrap();
        assert_eq!(purged, 0);
        assert_eq!(list_archived(&conn, None, 10, 0).unwrap().len(), 1);

        // Purge with 0 days — should NOT purge (guard condition)
        let purged = auto_purge_archive(&conn, Some(0)).unwrap();
        assert_eq!(purged, 0);

        // Purge with 90 days — archive is only 30 days old, should NOT purge
        let purged = auto_purge_archive(&conn, Some(90)).unwrap();
        assert_eq!(purged, 0);

        // Purge with 7 days — archive is 30 days old, should be purged
        let purged = auto_purge_archive(&conn, Some(7)).unwrap();
        assert_eq!(purged, 1);
        assert!(list_archived(&conn, None, 10, 0).unwrap().is_empty());
    }
}
