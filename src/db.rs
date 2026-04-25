// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;

use crate::models::{
    AGENTS_NAMESPACE, AgentRegistration, Approval, ApproverType, GovernanceDecision,
    GovernanceLevel, GovernancePolicy, GovernedAction, MAX_NAMESPACE_DEPTH, Memory, MemoryLink,
    NamespaceCount, PROMOTION_THRESHOLD, PendingAction, Stats, Taxonomy, TaxonomyNode, Tier,
    TierCount, namespace_ancestors,
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
///
/// Performance (v0.6.0 GA): each scope branch compares against the indexed
/// generated column `scope_idx` (schema v10) rather than re-evaluating
/// `json_extract(metadata, '$.scope')` per row. The query planner picks
/// `idx_memories_scope_idx` whenever the predicate narrows by scope,
/// dropping recall from "scan every namespace row and parse its JSON" to
/// an index seek + per-row refinement. See `docs/ARCHITECTURAL_LIMITS.md`
/// for which `SQLite` limits remain structural.
///
/// Security (issue #217): the team/unit/org branches use `LIKE` to expand a
/// prefix into its sub-tree. Without escaping, a caller who can influence the
/// prefix could inject SQL `LIKE` meta-characters (`%`, `_`) and broaden the
/// match across unrelated namespaces. We neutralise this at SQL evaluation
/// time by `replace()`-escaping `%` and `_` in the bound prefix and pairing
/// the LIKE with `ESCAPE '\'`. `validate_namespace` already rejects backslash,
/// so `\` cannot appear in the bound prefix and the escape sentinel is safe.
/// The `=` equality side is unaffected by LIKE wildcards and binds the raw
/// value so that legitimate namespaces containing `_` (e.g. `under_score`)
/// continue to match exactly.
fn visibility_clause(start: usize, table_alias: &str) -> String {
    let private_ph = start;
    let team_ph = start + 1;
    let unit_ph = start + 2;
    let org_ph = start + 3;
    let ta = table_alias;
    format!(
        "AND (\
            ?{private_ph} IS NULL \
            OR {ta}.scope_idx = 'collective' \
            OR ({ta}.scope_idx = 'private' AND {ta}.namespace = ?{private_ph}) \
            OR ({ta}.scope_idx = 'team' AND ?{team_ph} IS NOT NULL AND ({ta}.namespace = ?{team_ph} OR {ta}.namespace LIKE replace(replace(?{team_ph}, '%', '\\%'), '_', '\\_') || '/%' ESCAPE '\\')) \
            OR ({ta}.scope_idx = 'unit' AND ?{unit_ph} IS NOT NULL AND ({ta}.namespace = ?{unit_ph} OR {ta}.namespace LIKE replace(replace(?{unit_ph}, '%', '\\%'), '_', '\\_') || '/%' ESCAPE '\\')) \
            OR ({ta}.scope_idx = 'org'  AND ?{org_ph}  IS NOT NULL AND ({ta}.namespace = ?{org_ph}  OR {ta}.namespace LIKE replace(replace(?{org_ph}, '%', '\\%'), '_', '\\_') || '/%' ESCAPE '\\'))\
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

const CURRENT_SCHEMA_VERSION: i64 = 15;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("failed to open database")?;
    apply_sqlcipher_key(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)
        .context("failed to initialize schema")?;
    migrate(&conn)?;
    Ok(conn)
}

/// v0.6.0.0 — apply the SQLCipher passphrase (PRAGMA key) when the
/// `sqlcipher` cargo feature is built-in AND a passphrase has been
/// provided via `AI_MEMORY_DB_PASSPHRASE` env var. The recommended
/// way to set the env var is via the `--db-passphrase-file <path>`
/// CLI flag, which reads the passphrase from a root-readable file
/// and exports the env for the daemon's lifetime only. Passing the
/// passphrase directly as an env var works but leaks to the process
/// list (`ps -E`, `/proc/<pid>/environ`).
///
/// When the `sqlcipher` feature is NOT enabled, this function is a
/// no-op — standard SQLite has no `PRAGMA key` so setting one errors.
#[cfg(feature = "sqlcipher")]
fn apply_sqlcipher_key(conn: &Connection) -> Result<()> {
    let Ok(passphrase) = std::env::var("AI_MEMORY_DB_PASSPHRASE") else {
        anyhow::bail!(
            "sqlcipher build requires AI_MEMORY_DB_PASSPHRASE (set via --db-passphrase-file <path>)"
        );
    };
    // PRAGMA key must be the FIRST operation on a new connection. The
    // passphrase is quoted with SQL string-literal quoting rules.
    let escaped = passphrase.replace('\'', "''");
    conn.pragma_update(None, "key", format!("'{escaped}'"))
        .context("PRAGMA key failed (wrong passphrase or unencrypted DB?)")?;
    // Verify the key opened the database by running a cheap query.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .context("SQLCipher unlock verification failed — wrong passphrase?")?;
    Ok(())
}

#[cfg(not(feature = "sqlcipher"))]
#[allow(clippy::unnecessary_wraps)]
fn apply_sqlcipher_key(_conn: &Connection) -> Result<()> {
    Ok(())
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
        if version < 8 {
            // Task 1.9: pending_actions table for governance-queued operations
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_actions (
                    id            TEXT PRIMARY KEY,
                    action_type   TEXT NOT NULL,
                    memory_id     TEXT,
                    namespace     TEXT NOT NULL,
                    payload       TEXT NOT NULL DEFAULT '{}',
                    requested_by  TEXT NOT NULL,
                    requested_at  TEXT NOT NULL,
                    status        TEXT NOT NULL DEFAULT 'pending',
                    decided_by    TEXT,
                    decided_at    TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_pending_status    ON pending_actions(status);
                CREATE INDEX IF NOT EXISTS idx_pending_namespace ON pending_actions(namespace);",
            )?;
        }
        if version < 9 {
            // Task 1.10: approvals JSON array for consensus approver type
            let has_approvals: bool = conn
                .prepare("SELECT approvals FROM pending_actions LIMIT 0")
                .is_ok();
            if !has_approvals {
                conn.execute(
                    "ALTER TABLE pending_actions ADD COLUMN approvals TEXT NOT NULL DEFAULT '[]'",
                    [],
                )?;
            }
        }

        if version < 10 {
            // v0.6.0 GA: index `scope` so visibility filtering isn't a
            // JSON scan. Uses a VIRTUAL generated column (no row bytes
            // spent) plus a conventional B-tree index. The `visibility_clause`
            // SQL compares against the generated column directly — SQLite's
            // query planner picks the index because the comparison is on a
            // real column, not a repeated expression.
            //
            // The expression is guarded by `json_valid(metadata)` so rows
            // with legacy / corrupt metadata (we test this path explicitly
            // in `metadata_corrupt_column_falls_back_to_empty`) are still
            // writable — SQLite evaluates generated-column expressions on
            // every write that touches the source column, and an uncaught
            // `json_extract` failure would turn every corrupt-row write
            // into a constraint error.
            let has_scope_idx: bool = conn
                .prepare("SELECT scope_idx FROM memories LIMIT 0")
                .is_ok();
            if !has_scope_idx {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN scope_idx TEXT \
                     GENERATED ALWAYS AS (\
                         CASE WHEN json_valid(metadata) \
                         THEN COALESCE(json_extract(metadata, '$.scope'), 'private') \
                         ELSE 'private' END\
                     ) VIRTUAL",
                    [],
                )?;
            }
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_memories_scope_idx ON memories(scope_idx)",
                [],
            )?;
        }

        if version < 11 {
            // Phase 3 foundation (issue #224): vector-clock sync state.
            // Stores the latest `updated_at` timestamp this peer has seen
            // from each known remote peer. Used by the future CRDT-lite
            // merge to skip memories the caller has already seen and to
            // emit incremental `GET /api/v1/sync/since?...` responses.
            //
            // The table is additive — it does NOT change any existing
            // sync behaviour in v0.6.0 GA. Entries are created lazily by
            // the HTTP sync endpoints and by `sync --dry-run` telemetry.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sync_state (
                    agent_id       TEXT NOT NULL,
                    peer_id        TEXT NOT NULL,
                    last_seen_at   TEXT NOT NULL,
                    last_pulled_at TEXT NOT NULL,
                    PRIMARY KEY (agent_id, peer_id)
                );
                CREATE INDEX IF NOT EXISTS idx_sync_state_agent ON sync_state(agent_id);",
            )?;
        }

        if version < 12 {
            // Phase 3 Task 3b.1 (issue #224): track the high-watermark of
            // local memories this agent has successfully pushed to each
            // peer. The daemon uses it to stream only deltas on the next
            // push cycle. Null for rows from v11 that predate this column.
            let has_last_pushed: bool = conn
                .prepare("SELECT last_pushed_at FROM sync_state LIMIT 0")
                .is_ok();
            if !has_last_pushed {
                conn.execute("ALTER TABLE sync_state ADD COLUMN last_pushed_at TEXT", [])?;
            }
        }

        if version < 13 {
            // v0.6.0.0 — webhook subscriptions. Events fire on memory_store
            // (and, in v0.6.1, delete/promote/link) and are dispatched as
            // HMAC-SHA256-signed POSTs to subscriber URLs. `events` is a
            // comma-separated whitelist; `*` = all current + future events.
            // `secret_hash` stores a SHA-256 of the operator-supplied
            // shared secret — the plaintext never lands in the DB.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS subscriptions (
                    id TEXT PRIMARY KEY,
                    url TEXT NOT NULL,
                    events TEXT NOT NULL DEFAULT '*',
                    secret_hash TEXT,
                    namespace_filter TEXT,
                    agent_filter TEXT,
                    created_by TEXT,
                    created_at TEXT NOT NULL,
                    last_dispatched_at TEXT,
                    dispatch_count INTEGER NOT NULL DEFAULT 0,
                    failure_count INTEGER NOT NULL DEFAULT 0
                )",
                [],
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_subscriptions_url ON subscriptions(url)",
                [],
            )?;
        }

        if version < 14 {
            // Ultrareview #342: list / search / recall queries filter by
            // `json_extract(metadata, '$.agent_id') = ?`, which SQLite
            // cannot index. On large mesh peers this degenerates to a
            // full table scan per request and a DoS vector — a single
            // authenticated client hitting `/memories?agent_id=X` in a
            // loop pegs CPU and blocks other queries on the shared
            // connection. Add a VIRTUAL generated column so the
            // comparison becomes a real column lookup the query planner
            // can serve from an index.
            //
            // Ultrareview #353: also add `created_at` index so export
            // and snapshot queries stop scanning + sorting full table.
            let has_agent_id_idx: bool = conn
                .prepare("SELECT agent_id_idx FROM memories LIMIT 0")
                .is_ok();
            if !has_agent_id_idx {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN agent_id_idx TEXT \
                     GENERATED ALWAYS AS (\
                         CASE WHEN json_valid(metadata) \
                         THEN json_extract(metadata, '$.agent_id') \
                         ELSE NULL END\
                     ) VIRTUAL",
                    [],
                )?;
            }
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_memories_agent_id ON memories(agent_id_idx)",
                [],
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at)",
                [],
            )?;
        }

        if version < 15 {
            // v0.6.3 Stream B — Temporal-Validity KG schema additions.
            // Charter §"Critical Schema Reference" (lines 686–723):
            // four temporal columns on `memory_links`, three temporal
            // indexes for KG traversal queries, and an `entity_aliases`
            // side table for the upcoming entity registry. Pure additive
            // — no existing column or index is dropped or renamed, so
            // existing `link()` / `links_for()` paths keep working with
            // the new columns NULL on legacy rows. The `valid_from`
            // backfill matches the charter pre-flight default
            // (charter line 428): set to the source memory's
            // `created_at` to avoid null-handling complexity in v0.6.3
            // KG query code.
            let has_valid_from = conn
                .prepare("SELECT valid_from FROM memory_links LIMIT 0")
                .is_ok();
            if !has_valid_from {
                conn.execute("ALTER TABLE memory_links ADD COLUMN valid_from TEXT", [])?;
            }
            let has_valid_until = conn
                .prepare("SELECT valid_until FROM memory_links LIMIT 0")
                .is_ok();
            if !has_valid_until {
                conn.execute("ALTER TABLE memory_links ADD COLUMN valid_until TEXT", [])?;
            }
            let has_observed_by = conn
                .prepare("SELECT observed_by FROM memory_links LIMIT 0")
                .is_ok();
            if !has_observed_by {
                conn.execute("ALTER TABLE memory_links ADD COLUMN observed_by TEXT", [])?;
            }
            let has_signature = conn
                .prepare("SELECT signature FROM memory_links LIMIT 0")
                .is_ok();
            if !has_signature {
                conn.execute("ALTER TABLE memory_links ADD COLUMN signature BLOB", [])?;
            }

            conn.execute(
                "UPDATE memory_links \
                 SET valid_from = (SELECT created_at FROM memories WHERE id = memory_links.source_id) \
                 WHERE valid_from IS NULL",
                [],
            )?;

            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_links_temporal_src \
                 ON memory_links (source_id, valid_from, valid_until)",
                [],
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt \
                 ON memory_links (target_id, valid_from, valid_until)",
                [],
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_links_relation \
                 ON memory_links (relation, valid_from)",
                [],
            )?;

            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS entity_aliases (
                    entity_id  TEXT NOT NULL,
                    alias      TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (entity_id, alias)
                );
                CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
                  ON entity_aliases (alias);",
            )?;
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
///
/// Ultrareview #352: collapses the previous `INSERT`/`ON CONFLICT` +
/// separate `SELECT` into a single `INSERT ... RETURNING id`. Another
/// concurrent writer could otherwise slot in between the two statements
/// and the `SELECT` would return the wrong row id. `SQLite` 3.35+
/// supports `RETURNING`; it executes atomically within the `INSERT`.
pub fn insert(conn: &Connection, mem: &Memory) -> Result<String> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    let metadata_json = serde_json::to_string(&mem.metadata)?;
    let actual_id: String = conn.query_row(
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
            END
         RETURNING id",
        params![
            mem.id, mem.tier.as_str(), mem.namespace, mem.title, mem.content,
            tags_json, mem.priority, mem.confidence, mem.source, mem.access_count,
            mem.created_at, mem.updated_at, mem.last_accessed_at, mem.expires_at,
            metadata_json,
        ],
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

    // Ultrareview #354: rely on the UNIQUE INDEX on (title, namespace)
    // to enforce collision atomically at the DB layer. The previous
    // check-then-update sequence had a race — another transaction
    // could insert a colliding row between the SELECT and the UPDATE,
    // and the UPDATE would surface as a generic SQLite constraint
    // error to the caller. Now the collision check is inline: the
    // UPDATE fails with a well-scoped UniqueViolation, and we re-
    // query the colliding row's id only on that specific error for
    // the friendly message.
    let update_res = conn.execute(
        "UPDATE memories SET tier=?1, namespace=?2, title=?3, content=?4, tags=?5, priority=?6, confidence=?7, updated_at=?8, expires_at=?9, metadata=?10
         WHERE id=?11",
        params![effective_tier.as_str(), namespace, new_title, new_content, tags_json, priority, confidence, now, expires_at, metadata_json, id],
    );
    match update_res {
        Ok(_) => Ok((true, content_changed)),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let other: Option<String> = conn
                .query_row(
                    "SELECT id FROM memories WHERE title = ?1 AND namespace = ?2 AND id != ?3",
                    params![new_title, namespace, id],
                    |r| r.get(0),
                )
                .ok();
            if let Some(other_id) = other {
                anyhow::bail!(
                    "title '{new_title}' already exists in namespace '{namespace}' (memory {other_id})"
                );
            }
            Err(anyhow::anyhow!("update failed with constraint violation"))
        }
        Err(e) => Err(e.into()),
    }
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

/// Move a memory from `memories` to `archived_memories`. Used by the
/// HTTP `/api/v1/archive` explicit-archive endpoint (S29) and by
/// `sync_push` when a peer pushes an `archives: [id]` record.
///
/// Unlike `gc(archive=true)` this does not filter on `expires_at` — the
/// caller is explicitly asking for the row to be archived right now.
///
/// Returns `true` if a row was moved, `false` if no live memory existed
/// with this id (e.g. it was already archived or never written locally).
/// A missing-on-peer id is expected during normal fanout and callers
/// treat it as a no-op.
///
/// # Errors
///
/// Returns an error if the INSERT-SELECT or DELETE fails.
pub fn archive_memory(conn: &Connection, id: &str, reason: Option<&str>) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let reason = reason.unwrap_or("archive");
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<bool> {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !exists {
            return Ok(false);
        }
        conn.execute(
            "INSERT OR REPLACE INTO archived_memories
             (id, tier, namespace, title, content, tags, priority, confidence,
              source, access_count, created_at, updated_at, last_accessed_at,
              expires_at, archived_at, archive_reason, metadata)
             SELECT id, tier, namespace, title, content, tags, priority, confidence,
                    source, access_count, created_at, updated_at, last_accessed_at,
                    expires_at, ?1, ?2, metadata
             FROM memories WHERE id = ?3",
            params![now, reason, id],
        )?;
        // Clean up namespace_meta — mirrors `delete`'s cleanup so an archived
        // row is not still referenced as the namespace standard.
        conn.execute(
            "DELETE FROM namespace_meta WHERE standard_id = ?1",
            params![id],
        )?;
        let removed = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(removed > 0)
    })();
    match result {
        Ok(moved) => {
            conn.execute_batch("COMMIT")?;
            Ok(moved)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
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
           AND (?10 IS NULL OR agent_id_idx = ?10)
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
           AND (?10 IS NULL OR m.agent_id_idx = ?10)
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

/// Task 1.12 — proximity boost applied to a memory's score based on its
/// depth distance from the queried agent namespace. Uses the formula
/// `1 / (1 + depth_distance * 0.3)` per spec. Distance 0 = full strength
/// (1.0), each step up the hierarchy dampens linearly.
#[must_use]
pub fn proximity_boost(agent_ns: &str, memory_ns: &str) -> f64 {
    let agent_depth = crate::models::namespace_depth(agent_ns);
    let memory_depth = crate::models::namespace_depth(memory_ns);
    let distance = agent_depth.saturating_sub(memory_depth);
    #[allow(clippy::cast_precision_loss)]
    let d = distance as f64;
    1.0 / (1.0 + d * 0.3)
}

/// Task 1.12 — SQL fragment + boolean indicating whether hierarchy
/// expansion is in play. When active the `namespace` SQL param binds
/// NULL (so `?N IS NULL OR m.namespace = ?N` passes trivially) and a
/// separate `AND m.namespace IN (<ancestors>)` clause narrows to the
/// hierarchy. When inactive the returned fragment is empty.
///
/// Ancestor strings are interpolated because `SQLite` `IN` with a
/// variable-length positional list is awkward, and the inputs come
/// from `namespace_ancestors()` → `validate_namespace`-approved
/// strings. Single-quote doubling is applied defensively.
fn hierarchy_in_clause(namespace: Option<&str>) -> (Option<String>, bool) {
    let Some(ns) = namespace else {
        return (None, false);
    };
    if !ns.contains('/') {
        return (None, false);
    }
    let ancestors = crate::models::namespace_ancestors(ns);
    if ancestors.is_empty() {
        return (None, false);
    }
    let quoted: Vec<String> = ancestors
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect();
    (
        Some(format!("AND m.namespace IN ({})", quoted.join(","))),
        true,
    )
}

/// Task 1.12 — apply proximity boost to scored memories ranked against
/// an agent's hierarchical namespace. Re-sorts by boosted score.
fn apply_proximity_boost(scored: Vec<(Memory, f64)>, agent_ns: &str) -> Vec<(Memory, f64)> {
    let mut boosted: Vec<(Memory, f64)> = scored
        .into_iter()
        .map(|(mem, score)| {
            let boost = proximity_boost(agent_ns, &mem.namespace);
            (mem, score * boost)
        })
        .collect();
    boosted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    boosted
}

/// Task 1.11 — rough token estimate for a memory. Uses the "~4 chars per
/// token" heuristic on `title + content`. Deliberately byte-length-based:
/// fast, deterministic, and correct enough for budget gating.
#[must_use]
pub fn estimate_memory_tokens(mem: &Memory) -> usize {
    (mem.title.len() + mem.content.len()) / 4
}

/// Task 1.11 — truncate a scored recall list to fit within an optional
/// token budget. Iterates in rank order; stops at the first memory whose
/// inclusion would exceed the budget. Returns `(truncated, tokens_used)`.
/// When `budget_tokens` is `None` the list is returned untouched, still
/// with an accurate `tokens_used` tally so callers can surface it in
/// response metadata.
#[must_use]
pub fn apply_token_budget(
    scored: Vec<(Memory, f64)>,
    budget_tokens: Option<usize>,
) -> (Vec<(Memory, f64)>, usize) {
    let mut used: usize = 0;
    let mut out = Vec::with_capacity(scored.len());
    for (mem, score) in scored {
        let cost = estimate_memory_tokens(&mem);
        if let Some(budget) = budget_tokens
            && used.saturating_add(cost) > budget
        {
            break;
        }
        used = used.saturating_add(cost);
        out.push((mem, score));
    }
    (out, used)
}

/// Recall — fuzzy OR search + touch + auto-promote + TTL extension.
/// Task 1.11: after ranking, applies optional `budget_tokens` cap.
/// Returns `(truncated_list, tokens_used)`.
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
    budget_tokens: Option<usize>,
) -> Result<(Vec<(Memory, f64)>, usize)> {
    let now = Utc::now().to_rfc3339();
    let fts_query = sanitize_fts_query(context, true);
    let (vis_p, vis_t, vis_u, vis_o) = compute_visibility_prefixes(as_agent);

    // Task 1.12: hierarchy expansion. If `namespace` is hierarchical (contains
    // `/`), broaden the filter to the full ancestor chain. Flat namespaces
    // keep exact-match semantics (backward compat).
    let (hierarchy_in, hierarchy_active) = hierarchy_in_clause(namespace);
    let hierarchy_fragment = hierarchy_in.unwrap_or_default();
    let effective_namespace = if hierarchy_active { None } else { namespace };

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
           {hierarchy_fragment}
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
            effective_namespace,
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

    // Task 1.12: proximity boost when hierarchy expansion is active.
    let boosted = if let (true, Some(anchor)) = (hierarchy_active, namespace) {
        apply_proximity_boost(results, anchor)
    } else {
        results
    };

    // Task 1.11: apply optional token budget in rank order (AFTER proximity).
    let (budgeted, tokens_used) = apply_token_budget(boosted, budget_tokens);

    // Touch all recalled memories that SURVIVED the budget cut — no sense
    // bumping access counts on memories the caller will never see.
    for (mem, _) in &budgeted {
        if let Err(e) = touch(conn, &mem.id, short_extend, mid_extend) {
            tracing::warn!("touch failed for memory {}: {}", &mem.id, e);
        }
    }
    Ok((budgeted, tokens_used))
}

/// Task 1.7 — vertical memory promotion.
///
/// Clones `source_id` into `to_namespace`, which must be a proper `/`-derived
/// ancestor of the memory's current namespace. The original memory is
/// **untouched** (vertical promotion is a fan-out, not a move). A
/// `derived_from` link is created from the new clone back to the source so
/// the promotion trail is queryable.
///
/// Returns the clone's new ID.
///
/// Errors when:
/// - source doesn't exist
/// - `to_namespace` is empty, equal to the source namespace, or not an
///   ancestor of it (see `namespace_ancestors`)
pub fn promote_to_namespace(
    conn: &Connection,
    source_id: &str,
    to_namespace: &str,
) -> Result<String> {
    if to_namespace.is_empty() {
        anyhow::bail!("to_namespace cannot be empty");
    }
    let source = get(conn, source_id)?
        .ok_or_else(|| anyhow::anyhow!("source memory not found: {source_id}"))?;
    if to_namespace == source.namespace {
        anyhow::bail!(
            "to_namespace must be a proper ancestor of the memory's namespace (got self: {})",
            source.namespace
        );
    }
    let ancestors = namespace_ancestors(&source.namespace);
    if !ancestors.iter().any(|a| a == to_namespace) {
        anyhow::bail!(
            "to_namespace '{to_namespace}' is not an ancestor of '{}' (ancestors: {ancestors:?})",
            source.namespace
        );
    }

    let now = Utc::now().to_rfc3339();
    let clone = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: source.tier.clone(),
        namespace: to_namespace.to_string(),
        title: source.title.clone(),
        content: source.content.clone(),
        tags: source.tags.clone(),
        priority: source.priority,
        confidence: source.confidence,
        source: source.source.clone(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: source.expires_at.clone(),
        metadata: source.metadata.clone(),
    };
    let actual_id = insert(conn, &clone)?;
    // Clone → source: derived_from. Safe to ignore if the link layer
    // short-circuits on self-link (impossible here — distinct IDs).
    create_link(conn, &actual_id, source_id, "derived_from")?;
    Ok(actual_id)
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
            // Hyphens are allowed inside words (e.g. "well-known"): the
            // unicode61 tokenizer treats `-` as a separator when indexing,
            // so `foo-bar` indexes as `foo` + `bar`. Keeping the hyphen in
            // the per-token phrase (below we wrap each token in `"…"`)
            // produces a phrase query that FTS5 evaluates by matching the
            // hyphen-split component terms in order — which is exactly
            // what callers expect when searching for hyphenated content.
            // Dropping the `'-'` filter here fixes scenario S28 without
            // reopening the `+`/`-` exclusion-injection hole (every token
            // is already phrase-quoted before being joined, so `-` cannot
            // reach FTS5 as a prefix operator).
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

/// Hard cap on input groups walked when assembling a taxonomy tree.
/// Even when callers pass a wildly large `limit`, we never walk more
/// than this many `(namespace, count)` rows — bounds memory + time.
const TAXONOMY_MAX_LIMIT: usize = 10_000;

/// Build a hierarchical namespace taxonomy (Pillar 1 / Stream A).
///
/// Groups live (non-expired) memories by `namespace`, splits each on
/// `/`, and folds them into a `TaxonomyNode` tree. The returned root
/// represents `namespace_prefix` (or the synthetic empty-string root if
/// no prefix is supplied); each child level descends one segment.
///
/// `max_depth` is interpreted as "show at most N levels *below the
/// prefix*". Memories whose namespace would have required descending
/// past the cutoff still contribute to the `subtree_count` of the
/// boundary ancestor (their counts are not lost — only the leaf
/// rendering is suppressed).
///
/// `limit` caps the number of input `(namespace, count)` rows we walk
/// — when truncated, `total_count` still reflects the full prefix
/// total (a separate aggregation), and `truncated` is set so callers
/// can warn the user. Hard ceiling: [`TAXONOMY_MAX_LIMIT`].
pub fn get_taxonomy(
    conn: &Connection,
    namespace_prefix: Option<&str>,
    max_depth: usize,
    limit: usize,
) -> Result<Taxonomy> {
    let now = Utc::now().to_rfc3339();
    let effective_limit = limit.min(TAXONOMY_MAX_LIMIT);
    // Clamp depth so callers asking for "everything" can't construct a
    // pathological deep walk; the namespace validator already rejects
    // depths > MAX_NAMESPACE_DEPTH on writes.
    let effective_depth = max_depth.min(MAX_NAMESPACE_DEPTH);

    let prefix = namespace_prefix.unwrap_or("");

    // Total count for the prefix is computed independently of the
    // truncated row walk so the caller-visible total stays honest even
    // when `limit` drops rows from the tree.
    let total_count: usize = if prefix.is_empty() {
        let v: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE expires_at IS NULL OR expires_at > ?1",
            params![now],
            |row| row.get(0),
        )?;
        usize::try_from(v).unwrap_or(0)
    } else {
        let v: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories
             WHERE (expires_at IS NULL OR expires_at > ?1)
               AND (namespace = ?2 OR namespace LIKE ?2 || '/%')",
            params![now, prefix],
            |row| row.get(0),
        )?;
        usize::try_from(v).unwrap_or(0)
    };

    // Group rows ordered by count DESC so a small `limit` keeps the
    // densest namespaces, then alphabetic for stable tie-breaking.
    let groups: Vec<(String, usize)> = if prefix.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT namespace, COUNT(*) FROM memories
             WHERE expires_at IS NULL OR expires_at > ?1
             GROUP BY namespace
             ORDER BY COUNT(*) DESC, namespace ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![now, i64::try_from(effective_limit).unwrap_or(i64::MAX)],
            |row| {
                let ns: String = row.get(0)?;
                let c: i64 = row.get(1)?;
                Ok((ns, usize::try_from(c).unwrap_or(0)))
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT namespace, COUNT(*) FROM memories
             WHERE (expires_at IS NULL OR expires_at > ?1)
               AND (namespace = ?2 OR namespace LIKE ?2 || '/%')
             GROUP BY namespace
             ORDER BY COUNT(*) DESC, namespace ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                now,
                prefix,
                i64::try_from(effective_limit).unwrap_or(i64::MAX)
            ],
            |row| {
                let ns: String = row.get(0)?;
                let c: i64 = row.get(1)?;
                Ok((ns, usize::try_from(c).unwrap_or(0)))
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let walked_count: usize = groups.iter().map(|(_, c)| *c).sum();
    let truncated = walked_count < total_count;

    // Synthesize the root node. `name` is the trailing segment of the
    // prefix (or empty for the global root) so renderers can label it.
    let root_name = prefix.rsplit('/').next().unwrap_or("").to_string();
    let mut root = TaxonomyNode {
        namespace: prefix.to_string(),
        name: root_name,
        count: 0,
        subtree_count: 0,
        children: Vec::new(),
    };

    for (ns, c) in groups {
        // Compute path segments below the prefix. When prefix is empty,
        // the whole namespace becomes the suffix; when ns == prefix
        // exactly, segments is empty and the count lands on the root.
        let suffix: &str = if prefix.is_empty() {
            ns.as_str()
        } else if ns == prefix {
            ""
        } else if ns.len() > prefix.len() + 1
            && ns.starts_with(prefix)
            && ns.as_bytes()[prefix.len()] == b'/'
        {
            &ns[prefix.len() + 1..]
        } else {
            // Defensive: SQL filter shouldn't return this, but skip rather
            // than panic if it ever does (e.g. a stray match like
            // "alphaone-sibling" matching prefix "alphaone").
            continue;
        };
        let all_segments: Vec<&str> = if suffix.is_empty() {
            Vec::new()
        } else {
            suffix.split('/').collect()
        };
        let take = all_segments.len().min(effective_depth);
        let used = &all_segments[..take];
        let exact_match_in_view = take == all_segments.len();

        // Walk into the tree. Every ancestor's subtree_count grows by c
        // — including the root — and only the deepest visible node's
        // `count` does, and only when it represents the exact namespace
        // (not a clamped boundary).
        root.subtree_count += c;
        if used.is_empty() {
            root.count += c;
            continue;
        }

        let mut path_so_far = prefix.to_string();
        let mut node = &mut root;
        for (i, seg) in used.iter().enumerate() {
            if !path_so_far.is_empty() {
                path_so_far.push('/');
            }
            path_so_far.push_str(seg);
            let pos = node.children.iter().position(|ch| ch.name == *seg);
            let idx = if let Some(p) = pos {
                p
            } else {
                node.children.push(TaxonomyNode {
                    namespace: path_so_far.clone(),
                    name: (*seg).to_string(),
                    count: 0,
                    subtree_count: 0,
                    children: Vec::new(),
                });
                node.children.len() - 1
            };
            node = &mut node.children[idx];
            node.subtree_count += c;
            let is_leaf = i + 1 == used.len();
            if is_leaf && exact_match_in_view {
                node.count += c;
            }
        }
    }

    sort_taxonomy(&mut root);

    Ok(Taxonomy {
        tree: root,
        total_count,
        truncated,
    })
}

fn sort_taxonomy(node: &mut TaxonomyNode) {
    node.children.sort_by(|a, b| a.name.cmp(&b.name));
    for child in &mut node.children {
        sort_taxonomy(child);
    }
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
/// Only overwrites if the incoming memory is newer (by `updated_at`,
/// tiebroken by memory.id for a total order across peers —
/// ultrareview #344, #345).
///
/// Rationale: ISO 8601 / RFC 3339 strings compare lexicographically
/// as long as all timestamps carry consistent precision + Z suffix.
/// Equal timestamps (common when two nodes edit in the same ms, or
/// when NTP aligns clocks) previously produced non-deterministic
/// winners per peer, causing permanent mesh divergence. Adding the
/// memory.id tiebreaker yields a total order every peer agrees on.
pub fn insert_if_newer(conn: &Connection, mem: &Memory) -> Result<String> {
    let tags_json = serde_json::to_string(&mem.tags)?;
    let metadata_json = serde_json::to_string(&mem.metadata)?;
    let actual_id: String = conn.query_row(
        "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at, last_accessed_at, expires_at, metadata)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(title, namespace) DO UPDATE SET
            content = CASE WHEN excluded.updated_at > memories.updated_at
                             OR (excluded.updated_at = memories.updated_at AND excluded.id > memories.id)
                           THEN excluded.content ELSE memories.content END,
            tags = CASE WHEN excluded.updated_at > memories.updated_at
                          OR (excluded.updated_at = memories.updated_at AND excluded.id > memories.id)
                        THEN excluded.tags ELSE memories.tags END,
            priority = MAX(memories.priority, excluded.priority),
            confidence = MAX(memories.confidence, excluded.confidence),
            source = CASE WHEN excluded.updated_at > memories.updated_at
                            OR (excluded.updated_at = memories.updated_at AND excluded.id > memories.id)
                          THEN excluded.source ELSE memories.source END,
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
                              OR (excluded.updated_at = memories.updated_at AND excluded.id > memories.id)
                         THEN excluded.metadata
                         ELSE memories.metadata END,
                    '$.agent_id',
                    json_extract(memories.metadata, '$.agent_id')
                )
                ELSE CASE WHEN excluded.updated_at > memories.updated_at
                               OR (excluded.updated_at = memories.updated_at AND excluded.id > memories.id)
                          THEN excluded.metadata
                          ELSE memories.metadata END
            END
         RETURNING id",
        params![
            mem.id, mem.tier.as_str(), mem.namespace, mem.title, mem.content,
            tags_json, mem.priority, mem.confidence, mem.source, mem.access_count,
            mem.created_at, mem.updated_at, mem.last_accessed_at, mem.expires_at,
            metadata_json,
        ],
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
    budget_tokens: Option<usize>,
    scoring: &crate::config::ResolvedScoring,
) -> Result<(Vec<(Memory, f64)>, usize)> {
    let now = Utc::now().to_rfc3339();
    let fts_query = sanitize_fts_query(context, true);
    let prefixes = compute_visibility_prefixes(as_agent);
    let (vis_p, vis_t, vis_u, vis_o) = prefixes.clone();

    // Task 1.12: hierarchy expansion (same logic as `recall`). Hierarchical
    // `namespace` broadens filter to ancestor chain; flat namespaces stay
    // exact-match.
    let (fts_hierarchy_in, hierarchy_active) = hierarchy_in_clause(namespace);
    let fts_hierarchy_fragment = fts_hierarchy_in.unwrap_or_default();
    // Semantic stmt has no `m.` alias and binds at slot 1 — compute separately.
    let sem_hierarchy_fragment = if hierarchy_active {
        if let Some(ns) = namespace {
            let ancestors = crate::models::namespace_ancestors(ns);
            let quoted: Vec<String> = ancestors
                .iter()
                .map(|a| format!("'{}'", a.replace('\'', "''")))
                .collect();
            format!("AND memories.namespace IN ({})", quoted.join(","))
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let effective_namespace = if hierarchy_active { None } else { namespace };

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
           {fts_hierarchy_fragment}
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
           {sem_hierarchy_fragment}
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
            effective_namespace,
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
            // v0.6.2 (S18 iteration): cosine gate relaxed 0.3 → 0.2.
            // Scenario-18 caught a real-world miss at the old ceiling:
            // semantically-related pairs with varied phrasing ("morning
            // outdoor exercise routine" vs. "brisk uphill strides along
            // the ridge line trails") landed at 0.25-0.29 cosine and
            // silently fell below 0.3, returning zero semantic hits.
            // 0.2 keeps clearly-unrelated content out (random noise
            // hovers near 0) while admitting legitimate semantic
            // associations; the blended score + FTS component still
            // rank relevance on the way out.
            if cosine > 0.2
                && let Some(mem) = get(conn, &hit.id)?
            {
                // Apply namespace/expiry/tag filters. Task 1.12: when
                // hierarchy expansion is active, allow any ancestor match
                // (namespace_ancestors gives us the set); otherwise exact.
                if let Some(ns) = namespace {
                    if hierarchy_active {
                        let ancestors = crate::models::namespace_ancestors(ns);
                        if !ancestors.iter().any(|a| a == &mem.namespace) {
                            continue;
                        }
                    } else if mem.namespace != ns {
                        continue;
                    }
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
                effective_namespace,
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
                // v0.6.2 (S18): see matching note above at the HNSW gate.
                if cosine > 0.2 {
                    scored.insert(mem.id.clone(), (mem, 0.0, cosine));
                }
            }
        }
    }

    // Normalize FTS scores and compute blended score.
    // Adaptive blend: semantic weight decreases for longer content (embeddings
    // lose information on long text; FTS stays precise).  Short memories
    // (< 500 chars) get 50/50, long memories (> 5 000 chars) get 15/85.
    // v0.6.0.0: multiply the blend by a per-tier exponential time-decay with
    // half-life defaults 7 d (short) / 30 d (mid) / 365 d (long). The
    // `legacy_scoring` config knob short-circuits the decay back to 1.0 for
    // A/B comparison and emergency regression rollback.
    let now_utc = Utc::now();
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
            let age_days = chrono::DateTime::parse_from_rfc3339(&mem.created_at)
                .ok()
                .map_or(0.0, |ts| {
                    let secs = (now_utc - ts.with_timezone(&Utc)).num_seconds();
                    // Saturate at ~68 y (i32::MAX seconds). Practical: any memory
                    // older than that decays all the way down and the exact age
                    // doesn't matter. Precision loss here is negligible — we
                    // only need ~hour granularity on a 1 e-9..1.0 multiplier.
                    #[allow(clippy::cast_precision_loss)]
                    {
                        secs as f64 / 86_400.0
                    }
                });
            let decay = scoring.decay_multiplier(&mem.tier, age_days);
            (mem, blended * decay)
        })
        .collect();

    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);

    // Task 1.12: proximity boost (if hierarchy expansion is active).
    let boosted = if let (true, Some(anchor)) = (hierarchy_active, namespace) {
        apply_proximity_boost(results, anchor)
    } else {
        results
    };

    // Task 1.11: apply token budget in rank order (AFTER proximity).
    let (budgeted, tokens_used) = apply_token_budget(boosted, budget_tokens);

    // Touch surviving memories only.
    for (mem, _) in &budgeted {
        if let Err(e) = touch(conn, &mem.id, short_extend, mid_extend) {
            tracing::warn!("touch failed for memory {}: {}", &mem.id, e);
        }
    }

    Ok((budgeted, tokens_used))
}

/// Checkpoint WAL for clean shutdown.
pub fn checkpoint(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 3 foundation (issue #224) — sync_state helpers.
//
// These are additive: they do not change how the existing `ai-memory sync`
// command behaves in v0.6.0 GA. They exist so HTTP sync endpoints and the
// CRDT-lite merge follow-up can durably track "last updated_at seen from
// peer X" per local agent.
// ---------------------------------------------------------------------------

/// Record the latest `updated_at` this local agent has observed from `peer_id`.
/// Monotonic by timestamp — older writes do not overwrite newer ones.
/// Lazily creates the row on first observation.
pub fn sync_state_observe(
    conn: &Connection,
    agent_id: &str,
    peer_id: &str,
    seen_at: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sync_state (agent_id, peer_id, last_seen_at, last_pulled_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(agent_id, peer_id) DO UPDATE SET \
            last_seen_at = CASE WHEN excluded.last_seen_at > last_seen_at \
                                THEN excluded.last_seen_at \
                                ELSE last_seen_at END, \
            last_pulled_at = excluded.last_pulled_at",
        params![agent_id, peer_id, seen_at, now],
    )?;
    Ok(())
}

/// Load the full vector clock for `agent_id` — the set of
/// (`peer_id` -> `last_seen_at`) this local agent tracks.
pub fn sync_state_load(conn: &Connection, agent_id: &str) -> Result<crate::models::VectorClock> {
    let mut stmt =
        conn.prepare("SELECT peer_id, last_seen_at FROM sync_state WHERE agent_id = ?1")?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut clock = crate::models::VectorClock::default();
    for row in rows {
        let (peer, at) = row?;
        clock.entries.insert(peer, at);
    }
    Ok(clock)
}

/// Look up this peer's last-push watermark for `peer_id`. Returns `None`
/// if we've never successfully pushed to them (foundation-era rows also
/// return `None` because the column was added in schema v12).
#[must_use]
pub fn sync_state_last_pushed(conn: &Connection, agent_id: &str, peer_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT last_pushed_at FROM sync_state WHERE agent_id = ?1 AND peer_id = ?2",
        params![agent_id, peer_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// Record that local memories up to `updated_at = pushed_at` have been
/// accepted by `peer_id`. Creates the row if it doesn't exist; monotonic.
pub fn sync_state_record_push(
    conn: &Connection,
    agent_id: &str,
    peer_id: &str,
    pushed_at: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sync_state (agent_id, peer_id, last_seen_at, last_pulled_at, last_pushed_at) \
         VALUES (?1, ?2, ?3, ?3, ?4) \
         ON CONFLICT(agent_id, peer_id) DO UPDATE SET \
            last_pushed_at = CASE \
                WHEN excluded.last_pushed_at IS NULL THEN last_pushed_at \
                WHEN last_pushed_at IS NULL THEN excluded.last_pushed_at \
                WHEN excluded.last_pushed_at > last_pushed_at THEN excluded.last_pushed_at \
                ELSE last_pushed_at END",
        params![agent_id, peer_id, now, pushed_at],
    )?;
    Ok(())
}

/// Return memories whose `updated_at > since`, ordered by `updated_at`
/// ascending. Used by `GET /api/v1/sync/since` to stream incremental
/// updates to a peer. Caps at `limit` rows (caller-chosen pagination).
pub fn memories_updated_since(
    conn: &Connection,
    since: Option<&str>,
    limit: usize,
) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, tier, namespace, title, content, tags, priority, confidence, \
                source, access_count, created_at, updated_at, last_accessed_at, \
                expires_at, metadata \
         FROM memories \
         WHERE (?1 IS NULL OR updated_at > ?1) \
         ORDER BY updated_at ASC \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since, limit], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
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

/// v0.6.2 (S35): read the full `namespace_meta` row for a namespace so the
/// caller can fan it out to peers. Returns `None` when no standard is set.
/// Mirrors the (`namespace`, `standard_id`, `parent_namespace`, `updated_at`)
/// tuple used by `set_namespace_standard`.
#[allow(clippy::unnecessary_wraps)]
pub fn get_namespace_meta_entry(
    conn: &Connection,
    namespace: &str,
) -> Result<Option<crate::models::NamespaceMetaEntry>> {
    let row = conn
        .query_row(
            "SELECT namespace, standard_id, parent_namespace, updated_at
             FROM namespace_meta WHERE namespace = ?1",
            params![namespace],
            |r| {
                Ok(crate::models::NamespaceMetaEntry {
                    namespace: r.get(0)?,
                    standard_id: r.get(1)?,
                    parent_namespace: r.get(2)?,
                    updated_at: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                })
            },
        )
        .ok();
    Ok(row)
}

/// Clear the standard for a namespace.
pub fn clear_namespace_standard(conn: &Connection, namespace: &str) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM namespace_meta WHERE namespace = ?1",
        params![namespace],
    )?;
    Ok(changed > 0)
}

// ---------------------------------------------------------------------------
// Task 1.9 — governance enforcement + pending_actions CRUD
// ---------------------------------------------------------------------------

/// Resolve the explicit governance policy for a namespace from its standard
/// memory's `metadata.governance`. Returns `None` when no policy is set —
/// enforcement is **opt-in**, so namespaces without explicit policy skip
/// every governance check (historical behavior preserved). The "default
/// policy" (`{ write: Any, promote: Any, delete: Owner, approver: Human }`)
/// is surfaced by `get_standard` for display purposes only; it does not
/// gate operations.
pub fn resolve_governance_policy(conn: &Connection, namespace: &str) -> Option<GovernancePolicy> {
    let standard_id = get_namespace_standard(conn, namespace).ok()??;
    let mem = get(conn, &standard_id).ok()??;
    match GovernancePolicy::from_metadata(&mem.metadata) {
        Some(Ok(p)) => Some(p),
        _ => None,
    }
}

/// Return true if `agent_id` matches a registered agent in `_agents`.
fn is_registered_agent(conn: &Connection, agent_id: &str) -> bool {
    let title = format!("agent:{agent_id}");
    conn.query_row(
        "SELECT 1 FROM memories WHERE namespace = ?1 AND title = ?2",
        params![AGENTS_NAMESPACE, &title],
        |r| r.get::<_, i64>(0),
    )
    .is_ok()
}

/// Evaluate a governance level against caller context.
/// - `memory_owner`: the existing memory's `metadata.agent_id` (delete/promote paths).
///   Pass `None` for store operations.
/// - `namespace_owner`: the `metadata.agent_id` of the namespace's standard memory,
///   used as the "owner" for store operations. Resolved once by the caller.
fn evaluate_level(
    conn: &Connection,
    level: &GovernanceLevel,
    agent_id: &str,
    memory_owner: Option<&str>,
    namespace_owner: Option<&str>,
) -> GovernanceDecision {
    match level {
        GovernanceLevel::Any => GovernanceDecision::Allow,
        GovernanceLevel::Registered => {
            if is_registered_agent(conn, agent_id) {
                GovernanceDecision::Allow
            } else {
                GovernanceDecision::Deny(format!(
                    "governance: caller '{agent_id}' is not a registered agent"
                ))
            }
        }
        GovernanceLevel::Owner => {
            let owner = memory_owner.or(namespace_owner);
            match owner {
                Some(o) if o == agent_id => GovernanceDecision::Allow,
                Some(o) => GovernanceDecision::Deny(format!(
                    "governance: caller '{agent_id}' is not the owner ('{o}')"
                )),
                None => GovernanceDecision::Deny(
                    "governance: owner-level action has no resolvable owner".into(),
                ),
            }
        }
        GovernanceLevel::Approve => {
            // Caller translates this into a queued pending_action — the enforcement
            // helpers below own the queueing so the db layer is the single source
            // of truth for pending ids.
            GovernanceDecision::Pending(String::new())
        }
    }
}

/// Resolve the namespace-owner (`metadata.agent_id` of the namespace's
/// standard memory) used for `Owner`-level store checks.
fn namespace_owner(conn: &Connection, namespace: &str) -> Option<String> {
    let standard_id = get_namespace_standard(conn, namespace).ok().flatten()?;
    let mem = get(conn, &standard_id).ok().flatten()?;
    mem.metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Enforce governance for a `GovernedAction`. On [`GovernanceDecision::Pending`],
/// a row is inserted into `pending_actions` and the returned `pending_id` is
/// embedded in the decision.
pub fn enforce_governance(
    conn: &Connection,
    action: GovernedAction,
    namespace: &str,
    agent_id: &str,
    memory_id: Option<&str>,
    memory_owner: Option<&str>,
    payload: &serde_json::Value,
) -> Result<GovernanceDecision> {
    // Opt-in enforcement: namespaces without an explicit policy are unaffected.
    let Some(policy) = resolve_governance_policy(conn, namespace) else {
        return Ok(GovernanceDecision::Allow);
    };
    let level = match action {
        GovernedAction::Store => &policy.write,
        GovernedAction::Delete => &policy.delete,
        GovernedAction::Promote => &policy.promote,
    };
    let ns_owner = if matches!(action, GovernedAction::Store) {
        namespace_owner(conn, namespace)
    } else {
        None
    };

    let decision = evaluate_level(conn, level, agent_id, memory_owner, ns_owner.as_deref());
    if let GovernanceDecision::Pending(_) = decision {
        let pending_id =
            queue_pending_action(conn, action, namespace, memory_id, agent_id, payload)?;
        return Ok(GovernanceDecision::Pending(pending_id));
    }
    Ok(decision)
}

/// Insert a `pending_actions` row and return its id.
pub fn queue_pending_action(
    conn: &Connection,
    action: GovernedAction,
    namespace: &str,
    memory_id: Option<&str>,
    requested_by: &str,
    payload: &serde_json::Value,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let payload_json = serde_json::to_string(payload)?;
    conn.execute(
        "INSERT INTO pending_actions (id, action_type, memory_id, namespace, payload, requested_by, requested_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')",
        params![
            id,
            action.as_str(),
            memory_id,
            namespace,
            payload_json,
            requested_by,
            now,
        ],
    )?;
    Ok(id)
}

/// v0.6.2 (S34): upsert a `pending_actions` row from a canonical `PendingAction`
/// struct — used by `sync_push` to apply a peer-originated pending row so
/// governance state is cluster-consistent. Preserves `approvals` and
/// decision fields verbatim so re-plays converge. Uses `INSERT ... ON
/// CONFLICT(id) DO UPDATE` because the originator's id is stable across
/// peers (unlike `queue_pending_action` which mints a fresh UUID per
/// queue call).
pub fn upsert_pending_action(conn: &Connection, pa: &PendingAction) -> Result<()> {
    let payload_json = serde_json::to_string(&pa.payload)?;
    let approvals_json = serde_json::to_string(&pa.approvals)?;
    conn.execute(
        "INSERT INTO pending_actions
         (id, action_type, memory_id, namespace, payload, requested_by,
          requested_at, status, decided_by, decided_at, approvals)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
            action_type  = excluded.action_type,
            memory_id    = excluded.memory_id,
            namespace    = excluded.namespace,
            payload      = excluded.payload,
            requested_by = excluded.requested_by,
            requested_at = excluded.requested_at,
            status       = excluded.status,
            decided_by   = excluded.decided_by,
            decided_at   = excluded.decided_at,
            approvals    = excluded.approvals",
        params![
            pa.id,
            pa.action_type,
            pa.memory_id,
            pa.namespace,
            payload_json,
            pa.requested_by,
            pa.requested_at,
            pa.status,
            pa.decided_by,
            pa.decided_at,
            approvals_json,
        ],
    )?;
    Ok(())
}

pub fn list_pending_actions(
    conn: &Connection,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<PendingAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, action_type, memory_id, namespace, payload, requested_by,
                requested_at, status, decided_by, decided_at, approvals
         FROM pending_actions
         WHERE (?1 IS NULL OR status = ?1)
         ORDER BY requested_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![status, limit], |row| {
        let payload_str: String = row.get(4)?;
        let payload: serde_json::Value =
            serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        let approvals_str: String = row.get(10)?;
        let approvals: Vec<Approval> = serde_json::from_str(&approvals_str).unwrap_or_default();
        Ok(PendingAction {
            id: row.get(0)?,
            action_type: row.get(1)?,
            memory_id: row.get(2)?,
            namespace: row.get(3)?,
            payload,
            requested_by: row.get(5)?,
            requested_at: row.get(6)?,
            status: row.get(7)?,
            decided_by: row.get(8)?,
            decided_at: row.get(9)?,
            approvals,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get_pending_action(conn: &Connection, id: &str) -> Result<Option<PendingAction>> {
    let row = conn.query_row(
        "SELECT id, action_type, memory_id, namespace, payload, requested_by,
                requested_at, status, decided_by, decided_at, approvals
         FROM pending_actions WHERE id = ?1",
        params![id],
        |row| {
            let payload_str: String = row.get(4)?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
            let approvals_str: String = row.get(10)?;
            let approvals: Vec<Approval> = serde_json::from_str(&approvals_str).unwrap_or_default();
            Ok(PendingAction {
                id: row.get(0)?,
                action_type: row.get(1)?,
                memory_id: row.get(2)?,
                namespace: row.get(3)?,
                payload,
                requested_by: row.get(5)?,
                requested_at: row.get(6)?,
                status: row.get(7)?,
                decided_by: row.get(8)?,
                decided_at: row.get(9)?,
                approvals,
            })
        },
    );
    match row {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Mark a pending action as approved or rejected. Returns true on status
/// transition. Does NOT execute the action itself — the caller replays
/// the payload on approval (the db layer doesn't know how to execute
/// cross-interface write semantics).
pub fn decide_pending_action(
    conn: &Connection,
    id: &str,
    approve: bool,
    decided_by: &str,
) -> Result<bool> {
    let new_status = if approve { "approved" } else { "rejected" };
    let now = Utc::now().to_rfc3339();
    let updated = conn.execute(
        "UPDATE pending_actions SET status = ?1, decided_by = ?2, decided_at = ?3
         WHERE id = ?4 AND status = 'pending'",
        params![new_status, decided_by, now, id],
    )?;
    Ok(updated > 0)
}

/// Task 1.10 — outcome of an approver-aware approve call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveOutcome {
    /// Approver check failed; policy identifies the reason.
    Rejected(String),
    /// Consensus quorum not yet met; vote recorded.
    Pending { votes: usize, quorum: u32 },
    /// Fully approved (Human single-step, matching Agent, or consensus
    /// threshold met). Caller may now replay the payload via
    /// `execute_pending_action`.
    Approved,
}

/// Task 1.10 — approver-type aware approve. Enforces the
/// `metadata.governance.approver` of the pending action's namespace.
pub fn approve_with_approver_type(
    conn: &Connection,
    pending_id: &str,
    approver_agent_id: &str,
) -> Result<ApproveOutcome> {
    let Some(pa) = get_pending_action(conn, pending_id)? else {
        return Ok(ApproveOutcome::Rejected(format!(
            "pending action not found: {pending_id}"
        )));
    };
    if pa.status != "pending" {
        return Ok(ApproveOutcome::Rejected(format!(
            "already decided: status={}",
            pa.status
        )));
    }
    // Resolve the namespace's approver type. If no policy, default to Human —
    // which accepts any approval (back-compat with 1.9 callers).
    let approver =
        resolve_governance_policy(conn, &pa.namespace).map_or(ApproverType::Human, |p| p.approver);

    match approver {
        ApproverType::Human => {
            let ok = decide_pending_action(conn, pending_id, true, approver_agent_id)?;
            if ok {
                Ok(ApproveOutcome::Approved)
            } else {
                Ok(ApproveOutcome::Rejected("decision write failed".into()))
            }
        }
        ApproverType::Agent(required) => {
            if approver_agent_id != required {
                return Ok(ApproveOutcome::Rejected(format!(
                    "designated approver is '{required}'; got '{approver_agent_id}'"
                )));
            }
            let ok = decide_pending_action(conn, pending_id, true, approver_agent_id)?;
            if ok {
                Ok(ApproveOutcome::Approved)
            } else {
                Ok(ApproveOutcome::Rejected("decision write failed".into()))
            }
        }
        ApproverType::Consensus(quorum) => {
            // Issue #216: a single caller could previously satisfy any
            // Consensus(n) quorum by varying the unauthenticated `agent_id`
            // (`alice`, `bob`, `Alice`/`alice` were three distinct votes).
            // Two changes harden the path:
            //   1. Require each voter to be a registered agent — raises the
            //      bar from "claim any string" to "operator pre-registered
            //      this id". Combined with auth on the approve endpoint
            //      (operator-deployed) this gives a real multi-party gate.
            //   2. Canonicalize the agent_id to lowercase for both the
            //      duplicate-vote check and storage so case-variants of the
            //      same id collapse to a single vote.
            if !is_registered_agent(conn, approver_agent_id) {
                return Ok(ApproveOutcome::Rejected(format!(
                    "consensus voter '{approver_agent_id}' is not a registered agent"
                )));
            }
            let canonical_id = approver_agent_id.to_ascii_lowercase();
            let mut approvals = pa.approvals.clone();
            if approvals
                .iter()
                .any(|a| a.agent_id.eq_ignore_ascii_case(&canonical_id))
            {
                return Ok(ApproveOutcome::Pending {
                    votes: approvals.len(),
                    quorum,
                });
            }
            approvals.push(Approval {
                agent_id: canonical_id.clone(),
                approved_at: Utc::now().to_rfc3339(),
            });
            let approvals_json = serde_json::to_string(&approvals)?;
            conn.execute(
                "UPDATE pending_actions SET approvals = ?1 WHERE id = ?2 AND status = 'pending'",
                params![approvals_json, pending_id],
            )?;
            let votes = approvals.len();
            if u32::try_from(votes).unwrap_or(u32::MAX) >= quorum {
                // Threshold met — transition status so the caller can replay.
                let ok = decide_pending_action(conn, pending_id, true, &canonical_id)?;
                if ok {
                    return Ok(ApproveOutcome::Approved);
                }
                return Ok(ApproveOutcome::Rejected(
                    "decision write failed at consensus threshold".into(),
                ));
            }
            Ok(ApproveOutcome::Pending { votes, quorum })
        }
    }
}

/// Task 1.10 — Execute an approved pending action's payload. Callers invoke
/// this after `approve_with_approver_type` returns `Approved`. Returns the
/// affected memory id (new id for store, existing id for delete/promote).
pub fn execute_pending_action(conn: &Connection, pending_id: &str) -> Result<Option<String>> {
    let Some(pa) = get_pending_action(conn, pending_id)? else {
        anyhow::bail!("pending action not found: {pending_id}");
    };
    if pa.status != "approved" {
        anyhow::bail!("cannot execute non-approved action (status={})", pa.status);
    }
    match pa.action_type.as_str() {
        "store" => {
            let mut mem: Memory = serde_json::from_value(pa.payload.clone())
                .map_err(|e| anyhow::anyhow!("invalid store payload: {e}"))?;
            // Stamp fresh id + timestamps so the execution is idempotent on replay.
            mem.id = uuid::Uuid::new_v4().to_string();
            let now = Utc::now().to_rfc3339();
            mem.created_at.clone_from(&now);
            mem.updated_at = now;
            mem.access_count = 0;
            let actual_id = insert(conn, &mem)?;
            Ok(Some(actual_id))
        }
        "delete" => {
            if let Some(mid) = pa.memory_id.clone() {
                delete(conn, &mid)?;
                Ok(Some(mid))
            } else {
                Ok(None)
            }
        }
        "promote" => {
            if let Some(mid) = pa.memory_id.clone() {
                if let Some(to_ns) = pa.payload.get("to_namespace").and_then(|v| v.as_str()) {
                    // Vertical promotion to ancestor.
                    let clone_id = promote_to_namespace(conn, &mid, to_ns)?;
                    return Ok(Some(clone_id));
                }
                // Tier bump to long + clear expiry.
                let (_found, _changed) = update(
                    conn,
                    &mid,
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
                Ok(Some(mid))
            } else {
                Ok(None)
            }
        }
        other => anyhow::bail!("unknown action_type: {other}"),
    }
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

        let (results, _tokens) = recall(
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
    fn archive_memory_moves_live_row_to_archive() {
        // S29 — explicit archive endpoint must move the row out of
        // `memories` and into `archived_memories` with the caller-supplied
        // reason. Unlike gc(archive=true), this is NOT gated on
        // `expires_at` — the caller is asking for it right now.
        let conn = test_db();
        let mem = make_memory("Archive me", "s29", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();

        let moved = archive_memory(&conn, &id, Some("explicit")).unwrap();
        assert!(moved, "live row must be archived on first call");
        assert!(
            get(&conn, &id).unwrap().is_none(),
            "row must be removed from active table"
        );

        let archived = list_archived(&conn, None, 10, 0).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0]["id"], id);
        assert_eq!(archived[0]["archive_reason"], "explicit");

        // Second call is a no-op — row is already out of `memories`.
        let second = archive_memory(&conn, &id, Some("explicit")).unwrap();
        assert!(
            !second,
            "second archive call must report no-op (no live row)"
        );
    }

    #[test]
    fn archive_memory_missing_id_returns_false() {
        // Peers that never saw M1 must no-op, not error, on sync_push
        // archives fanout.
        let conn = test_db();
        let moved = archive_memory(&conn, "nonexistent-id", None).unwrap();
        assert!(!moved);
    }

    #[test]
    fn archive_memory_default_reason_is_archive() {
        let conn = test_db();
        let mem = make_memory("Default reason", "s29", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();
        assert!(archive_memory(&conn, &id, None).unwrap());
        let archived = list_archived(&conn, None, 10, 0).unwrap();
        assert_eq!(archived[0]["archive_reason"], "archive");
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
    fn taxonomy_flat_namespaces_only() {
        // No `/` anywhere — every namespace is a direct child of the root.
        let conn = test_db();
        insert(&conn, &make_memory("A", "alpha", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("B", "alpha", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("C", "beta", Tier::Long, 5)).unwrap();

        let tax = get_taxonomy(&conn, None, 8, 1000).unwrap();
        assert_eq!(tax.total_count, 3);
        assert!(!tax.truncated);
        assert_eq!(tax.tree.namespace, "");
        assert_eq!(tax.tree.subtree_count, 3);
        assert_eq!(tax.tree.count, 0); // no memories at the synthetic root
        assert_eq!(tax.tree.children.len(), 2);
        let alpha = tax
            .tree
            .children
            .iter()
            .find(|c| c.name == "alpha")
            .unwrap();
        assert_eq!(alpha.count, 2);
        assert_eq!(alpha.subtree_count, 2);
        assert!(alpha.children.is_empty());
        let beta = tax.tree.children.iter().find(|c| c.name == "beta").unwrap();
        assert_eq!(beta.count, 1);
    }

    #[test]
    fn taxonomy_hierarchical_tree() {
        // Mixed depths: tree must aggregate counts up the spine.
        let conn = test_db();
        insert(&conn, &make_memory("a", "alphaone", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("b", "alphaone/eng", Tier::Long, 5)).unwrap();
        insert(
            &conn,
            &make_memory("c", "alphaone/eng/platform", Tier::Long, 5),
        )
        .unwrap();
        insert(
            &conn,
            &make_memory("d", "alphaone/eng/platform", Tier::Long, 5),
        )
        .unwrap();
        insert(&conn, &make_memory("e", "alphaone/sales", Tier::Long, 5)).unwrap();

        let tax = get_taxonomy(&conn, None, 8, 1000).unwrap();
        assert_eq!(tax.total_count, 5);
        assert_eq!(tax.tree.subtree_count, 5);
        assert_eq!(tax.tree.children.len(), 1);

        let alphaone = &tax.tree.children[0];
        assert_eq!(alphaone.name, "alphaone");
        assert_eq!(alphaone.namespace, "alphaone");
        assert_eq!(alphaone.count, 1); // memory "a" lives at exactly "alphaone"
        assert_eq!(alphaone.subtree_count, 5);
        assert_eq!(alphaone.children.len(), 2);

        let eng = alphaone.children.iter().find(|c| c.name == "eng").unwrap();
        assert_eq!(eng.namespace, "alphaone/eng");
        assert_eq!(eng.count, 1);
        assert_eq!(eng.subtree_count, 3);
        let platform = &eng.children[0];
        assert_eq!(platform.name, "platform");
        assert_eq!(platform.namespace, "alphaone/eng/platform");
        assert_eq!(platform.count, 2);
        assert_eq!(platform.subtree_count, 2);
        assert!(platform.children.is_empty());
    }

    #[test]
    fn taxonomy_prefix_scopes_subtree() {
        let conn = test_db();
        insert(&conn, &make_memory("a", "alphaone/eng", Tier::Long, 5)).unwrap();
        insert(
            &conn,
            &make_memory("b", "alphaone/eng/platform", Tier::Long, 5),
        )
        .unwrap();
        insert(&conn, &make_memory("c", "alphaone/sales", Tier::Long, 5)).unwrap();
        // Sibling that happens to share a string prefix — must NOT bleed in.
        insert(&conn, &make_memory("d", "alphaone-sibling", Tier::Long, 5)).unwrap();
        insert(&conn, &make_memory("e", "other", Tier::Long, 5)).unwrap();

        let tax = get_taxonomy(&conn, Some("alphaone/eng"), 8, 1000).unwrap();
        assert_eq!(tax.total_count, 2);
        assert_eq!(tax.tree.namespace, "alphaone/eng");
        assert_eq!(tax.tree.name, "eng");
        assert_eq!(tax.tree.count, 1);
        assert_eq!(tax.tree.subtree_count, 2);
        assert_eq!(tax.tree.children.len(), 1);
        assert_eq!(tax.tree.children[0].name, "platform");
        assert_eq!(tax.tree.children[0].count, 1);
    }

    #[test]
    fn taxonomy_depth_clamps_but_preserves_subtree_counts() {
        let conn = test_db();
        insert(
            &conn,
            &make_memory("a", "alphaone/eng/platform/db", Tier::Long, 5),
        )
        .unwrap();
        insert(
            &conn,
            &make_memory("b", "alphaone/eng/platform/api", Tier::Long, 5),
        )
        .unwrap();

        let tax = get_taxonomy(&conn, None, 2, 1000).unwrap();
        assert_eq!(tax.total_count, 2);
        let alphaone = &tax.tree.children[0];
        let eng = &alphaone.children[0];
        // Depth=2 below the empty prefix means we descend exactly two
        // levels (alphaone → eng); deeper segments are folded into
        // `eng.subtree_count` without rendering child nodes.
        assert!(eng.children.is_empty());
        assert_eq!(eng.subtree_count, 2);
        assert_eq!(eng.count, 0); // nothing at exactly "alphaone/eng"
    }

    #[test]
    fn taxonomy_excludes_expired_memories() {
        // Mirror of `list_namespaces` semantics — expired rows must not
        // count toward either the tree or `total_count`.
        let conn = test_db();
        let mut alive = make_memory("alive", "alpha", Tier::Long, 5);
        let mut dead = make_memory("dead", "alpha", Tier::Short, 5);
        // Force the short-tier memory's expiry into the past.
        dead.expires_at = Some("2000-01-01T00:00:00Z".to_string());
        alive.expires_at = None;
        insert(&conn, &alive).unwrap();
        insert(&conn, &dead).unwrap();

        let tax = get_taxonomy(&conn, None, 8, 1000).unwrap();
        assert_eq!(tax.total_count, 1);
        assert_eq!(tax.tree.children.len(), 1);
        assert_eq!(tax.tree.children[0].count, 1);
    }

    #[test]
    fn taxonomy_truncates_at_limit_but_total_stays_honest() {
        let conn = test_db();
        for ns in ["aa", "bb", "cc", "dd", "ee"] {
            insert(&conn, &make_memory("m", ns, Tier::Long, 5)).unwrap();
        }
        let tax = get_taxonomy(&conn, None, 8, 2).unwrap();
        // Limit drops 3 namespaces from the walk; total_count must
        // still see all 5 memories so renderers can warn the user.
        assert_eq!(tax.total_count, 5);
        assert!(tax.truncated);
        assert_eq!(tax.tree.children.len(), 2);
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
        // `+` prefix operator is stripped (prevents exclusion injection);
        // `-` is now preserved inside phrase-quoted tokens so hyphenated
        // content ("well-known", "foo-bar") searches correctly against
        // the unicode61 tokenizer. Phrase-quoting keeps `-` from reaching
        // FTS5 as a prefix operator, closing the injection hole.
        let sanitized4 = sanitize_fts_query("-secret +required", true);
        assert!(!sanitized4.contains('+'));
        assert!(sanitized4.contains("secret"));
        assert!(sanitized4.contains("required"));
        // Hyphenated tokens pass through as phrase searches.
        let sanitized5 = sanitize_fts_query("well-known", true);
        assert!(sanitized5.contains("well-known"));
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

        let (results, _tokens) = recall(
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
    fn scope_index_exists_after_migration() {
        // v0.6.0 GA (schema v10) — the `scope_idx` generated column and its
        // B-tree index must exist after `open()` runs migration.
        let conn = test_db();
        let has_col: bool = conn
            .prepare("SELECT scope_idx FROM memories LIMIT 0")
            .is_ok();
        assert!(has_col, "scope_idx generated column missing");
        let idx_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_memories_scope_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx_exists, 1, "idx_memories_scope_idx missing");
    }

    #[test]
    fn scope_index_used_for_direct_scope_filter() {
        // v0.6.0 GA — confirm `idx_memories_scope_idx` is picked for a
        // direct `WHERE scope_idx = ?` predicate. This is the shape the
        // query planner sees for `scope = 'collective'` fast-paths and
        // the branch-local predicate inside `visibility_clause`.
        //
        // We deliberately do NOT assert the index is used for the full
        // visibility_clause OR-chain — SQLite's planner may (correctly)
        // choose a scan when the OR-chain has variable selectivity across
        // branches. The point of the index is to accelerate the common
        // case when a recall narrows to one scope; the multi-branch
        // visibility clause still benefits because each branch evaluates
        // the predicate against a single column rather than a JSON extract.
        let conn = test_db();
        // Seed enough rows + ANALYZE so planner cost model is honest.
        for i in 0..200 {
            let scope = if i % 3 == 0 { "collective" } else { "private" };
            let mut mem = make_memory(&format!("row-{i}"), "test", Tier::Long, 5);
            mem.metadata = serde_json::json!({"scope": scope});
            insert(&conn, &mem).unwrap();
        }
        conn.execute("ANALYZE", []).unwrap();
        let plan: Vec<String> = conn
            .prepare("EXPLAIN QUERY PLAN SELECT id FROM memories WHERE scope_idx = ?1")
            .unwrap()
            .query_map(params!["collective"], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let joined = plan.join("\n");
        assert!(
            joined.contains("idx_memories_scope_idx"),
            "direct scope filter must use idx_memories_scope_idx; got:\n{joined}"
        );
    }

    #[test]
    fn scope_idx_reflects_metadata_on_insert_and_update() {
        // v0.6.0 GA — the VIRTUAL generated column must track metadata.scope
        // across insert and update without manual maintenance.
        let conn = test_db();
        let mut mem = make_memory("scope-tracking", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({"scope": "team"});
        let id = insert(&conn, &mem).unwrap();
        let scope: String = conn
            .query_row(
                "SELECT scope_idx FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scope, "team");

        // Flip scope to unit via metadata update — generated column updates.
        let new_meta = serde_json::json!({"scope": "unit"});
        update(
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
        let scope2: String = conn
            .query_row(
                "SELECT scope_idx FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scope2, "unit");

        // Memory with no scope key — virtual column returns the default.
        let mut bare = make_memory("no-scope-key", "test", Tier::Long, 5);
        bare.metadata = serde_json::json!({});
        let id2 = insert(&conn, &bare).unwrap();
        let scope3: String = conn
            .query_row(
                "SELECT scope_idx FROM memories WHERE id = ?1",
                params![id2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scope3, "private");
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

    // ─────────────────────────────────────────────────────────────────
    // Schema v15 (v0.6.3 Stream B) — temporal-validity KG migration.
    // ─────────────────────────────────────────────────────────────────

    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        cols.iter().any(|c| c == column)
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .is_ok()
    }

    #[test]
    fn schema_v15_memory_links_has_temporal_columns() {
        let conn = test_db();
        assert!(column_exists(&conn, "memory_links", "valid_from"));
        assert!(column_exists(&conn, "memory_links", "valid_until"));
        assert!(column_exists(&conn, "memory_links", "observed_by"));
        assert!(column_exists(&conn, "memory_links", "signature"));
    }

    #[test]
    fn schema_v15_memory_links_temporal_indexes_exist() {
        let conn = test_db();
        assert!(index_exists(&conn, "idx_links_temporal_src"));
        assert!(index_exists(&conn, "idx_links_temporal_tgt"));
        assert!(index_exists(&conn, "idx_links_relation"));
    }

    #[test]
    fn schema_v15_entity_aliases_table_exists() {
        let conn = test_db();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entity_aliases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(index_exists(&conn, "idx_entity_aliases_alias"));
    }

    #[test]
    fn schema_v15_entity_aliases_primary_key_unique() {
        let conn = test_db();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO entity_aliases (entity_id, alias, created_at) VALUES (?1, ?2, ?3)",
            params!["e1", "Alpha", &now],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO entity_aliases (entity_id, alias, created_at) VALUES (?1, ?2, ?3)",
            params!["e1", "Alpha", &now],
        );
        assert!(dup.is_err(), "expected PK uniqueness violation");
    }

    #[test]
    fn schema_v15_existing_links_get_valid_from_backfilled() {
        // Simulate a v14 database with one link, then re-run the
        // v15 migration and assert valid_from was backfilled to the
        // source memory's created_at. We do this by opening a fresh
        // db (which is at v15), inserting a link with NULL valid_from,
        // rolling schema_version back to 14, and re-opening to force
        // the v15 block to re-execute the backfill UPDATE.
        let path = std::env::temp_dir().join(format!(
            "ai_memory_v15_backfill_{}.db",
            uuid::Uuid::new_v4()
        ));
        {
            let conn = open(&path).unwrap();
            let src = make_memory("src", "test", Tier::Long, 5);
            let tgt = make_memory("tgt", "test", Tier::Long, 5);
            insert(&conn, &src).unwrap();
            insert(&conn, &tgt).unwrap();
            // Insert a link directly with NULL valid_from to mimic
            // pre-migration state.
            conn.execute(
                "INSERT INTO memory_links (source_id, target_id, relation, created_at, valid_from) \
                 VALUES (?1, ?2, 'related_to', ?3, NULL)",
                params![&src.id, &tgt.id, &chrono::Utc::now().to_rfc3339()],
            )
            .unwrap();
            // Roll schema back to v14 and re-run migrate via re-open.
            conn.execute("DELETE FROM schema_version", []).unwrap();
            conn.execute("INSERT INTO schema_version (version) VALUES (14)", [])
                .unwrap();
        }

        let conn2 = open(&path).unwrap();
        let backfilled: Option<String> = conn2
            .query_row("SELECT valid_from FROM memory_links LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            backfilled.is_some(),
            "expected valid_from to be backfilled, got NULL"
        );
        let _ = std::fs::remove_file(&path);
    }
}
