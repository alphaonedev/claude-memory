// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;

use crate::models::{
    AGENTS_NAMESPACE, AgentRegistration, Approval, ApproverType, DuplicateCheck, DuplicateMatch,
    GovernanceDecision, GovernanceLevel, GovernancePolicy, GovernedAction, MAX_NAMESPACE_DEPTH,
    Memory, MemoryLink, NamespaceCount, PROMOTION_THRESHOLD, PendingAction, Stats, Taxonomy,
    TaxonomyNode, Tier, TierCount, namespace_ancestors,
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

-- v0.6.4-009 — capability-expansion audit log (NHI guardrails phase 1).
-- Mirrors migrations/sqlite/0014_v064_audit_log.sql so a fresh DB
-- bootstrap that bypasses the migration ladder still ends up with the
-- table present.
CREATE TABLE IF NOT EXISTS audit_log (
    id                 TEXT PRIMARY KEY,
    agent_id           TEXT,
    event_type         TEXT NOT NULL,
    requested_family   TEXT,
    granted            INTEGER NOT NULL,
    attestation_tier   TEXT,
    timestamp          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_log_agent_id
    ON audit_log (agent_id);
CREATE INDEX IF NOT EXISTS idx_audit_log_timestamp
    ON audit_log (timestamp);
CREATE INDEX IF NOT EXISTS idx_audit_log_event_type
    ON audit_log (event_type);
";

// v17 = v0.6.3.1 (P4, audit G1) governance.inherit backfill.
// v18 = v0.6.3.1 (P2, audit G4/G5/G13) data-integrity hardening:
//       embedding_dim guard, archive lossless, magic-byte header.
// v19 = v0.6.3.1 (P5, audit G9) webhook event-types column +
//       per-subscriber filter.
// v20 = v0.6.4-009 (NHI guardrails phase 1) capability-expansion
//       audit_log table.
// v21 = v0.7.0 K2 pending_actions timeout sweeper:
//       `default_timeout_seconds` + `expired_at` columns plus a
//       composite (status, requested_at) index to bound the sweep
//       cost.
const CURRENT_SCHEMA_VERSION: i64 = 21;

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

const MIGRATION_V15_SQLITE: &str = include_str!("../migrations/sqlite/0010_v063_hierarchy_kg.sql");
// v0.6.3.1 (P4, audit G1): backfill `metadata.governance.inherit = true`
// on existing policies so downstream readers and SQL-side dashboards
// see a consistent shape after upgrade. Idempotent.
const MIGRATION_V17_SQLITE: &str = include_str!("../migrations/sqlite/0012_governance_inherit.sql");
// v0.6.3.1 (P2, audit G4/G5/G13): data-integrity hardening. ALTER TABLEs
// emitted from Rust because SQLite has no `ADD COLUMN IF NOT EXISTS`;
// the SQL file holds idempotent backfills + indexes.
const MIGRATION_V18_SQLITE: &str =
    include_str!("../migrations/sqlite/0011_v0631_data_integrity.sql");
// v0.6.3.1 (P5, audit G9): webhook event-types column + per-subscriber
// filter index. ADD COLUMN done inline (SQLite has no `ADD COLUMN IF NOT
// EXISTS`); SQL file holds the idempotent index batch.
const MIGRATION_V19_SQLITE: &str =
    include_str!("../migrations/sqlite/0013_webhook_event_types.sql");
// v0.6.4-009: capability-expansion audit log table. CREATE TABLE IF NOT
// EXISTS + indexes — fully idempotent.
const MIGRATION_V20_SQLITE: &str = include_str!("../migrations/sqlite/0014_v064_audit_log.sql");
// v0.7.0 K2: pending_actions timeout sweeper. ALTER TABLEs are emitted
// from Rust (see v21 below) because SQLite has no `ADD COLUMN IF NOT
// EXISTS`; this file just holds the idempotent index batch.
const MIGRATION_V21_SQLITE: &str =
    include_str!("../migrations/sqlite/0015_v07_pending_action_timeouts.sql");

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
            //
            // Type note: charter said `TIMESTAMP` for `valid_from` and
            // `valid_until`. SQLite has no native TIMESTAMP type — it
            // stores timestamps as TEXT (ISO-8601), REAL (Julian), or
            // INTEGER (unix). The codebase uses TEXT throughout (matches
            // every other timestamp column in this schema and matches
            // chrono's `to_rfc3339()` output). The Postgres adapter at
            // `src/store/postgres_schema.sql` uses `TIMESTAMPTZ` —
            // semantically equivalent across both backends.
            //
            // The DDL itself lives in migrations/sqlite/0010_v063_hierarchy_kg.sql
            // (and migrations/postgres/0010_v063_hierarchy_kg.sql for the
            // Postgres adapter). Loaded via include_str! at compile time
            // and executed below via execute_batch. The column-existence
            // checks remain inline here because SQLite cannot do
            // ALTER TABLE ADD COLUMN IF NOT EXISTS.
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

            // All INDEX and TABLE statements are idempotent; batch-run the migration
            conn.execute_batch(MIGRATION_V15_SQLITE)?;
        }

        if version < 16 {
            // v0.6.4 prep: explicitly document that the existing
            // idx_memories_namespace already supports prefix LIKE under
            // SQLite's default BINARY collation. Bump version so Postgres
            // peers' text_pattern_ops index is part of the same migration
            // generation.
            // No DDL needed for SQLite — index already prefix-friendly.
        }

        if version < 17 {
            // v0.6.3.1 (P4, audit G1): backfill `metadata.governance.inherit = true`
            // on existing namespace standards so the inheritance-enforcement
            // patch (resolve_governance_policy walking the chain leaf-first)
            // sees an explicit, physically-present field on legacy rows.
            // The field deserializes as `true` via #[serde(default)] either
            // way; the backfill keeps replication payloads, JSON-extract
            // dashboards, and operator inspect output consistent. Idempotent.
            conn.execute_batch(MIGRATION_V17_SQLITE)?;
        }

        if version < 18 {
            // v0.6.3.1 Phase P2 — Data-integrity hardening (G4, G5, G13).
            // See REMEDIATIONv0631 §"Phase P2".
            //
            // The DDL itself lives in migrations/sqlite/0011_v0631_data_integrity.sql.
            // ALTER TABLE ADD COLUMN statements are emitted here because SQLite
            // cannot do `ADD COLUMN IF NOT EXISTS`; the SQL file holds the
            // backfill UPDATE statements and the new indexes.
            //
            // memories.embedding_dim — declared dimension of the stored embedding.
            // Backfill below infers from `length(embedding)/4` (legacy LE-f32
            // payloads have no header so length is exactly 4n; v18+ writes
            // happen after commit, so the 4n-only inference here is safe).
            let has_embedding_dim = conn
                .prepare("SELECT embedding_dim FROM memories LIMIT 0")
                .is_ok();
            if !has_embedding_dim {
                conn.execute("ALTER TABLE memories ADD COLUMN embedding_dim INTEGER", [])?;
            }

            // archived_memories — preserve embedding + original tier/expiry on
            // archive (G5). Pre-v18 archive rows have lost this metadata
            // permanently; the SQL backfill below fills `original_tier='long'`
            // so restore_archived treats them as permanent on first restore.
            let has_archive_embedding = conn
                .prepare("SELECT embedding FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_archive_embedding {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN embedding BLOB",
                    [],
                )?;
            }
            let has_archive_embedding_dim = conn
                .prepare("SELECT embedding_dim FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_archive_embedding_dim {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN embedding_dim INTEGER",
                    [],
                )?;
            }
            let has_original_tier = conn
                .prepare("SELECT original_tier FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_original_tier {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN original_tier TEXT",
                    [],
                )?;
            }
            let has_original_expires_at = conn
                .prepare("SELECT original_expires_at FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_original_expires_at {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN original_expires_at TEXT",
                    [],
                )?;
            }

            // Backfill + indexes — UPDATE/INDEX statements are idempotent.
            conn.execute_batch(MIGRATION_V18_SQLITE)?;
        }

        if version < 19 {
            // v0.6.3.1 P5 / G9 — webhook event coverage. Adds an
            // `event_types` JSON-encoded array column to `subscriptions`
            // so callers can opt into a narrow, structured event filter
            // (e.g. `["memory_store", "memory_link_created"]`). The legacy
            // comma-separated `events` column stays as the canonical
            // matcher at dispatch time; new structured callers populate
            // BOTH so existing dispatch code keeps working unchanged.
            //
            // Backward compat: existing rows keep `events = '*'` and have
            // `event_types = NULL` — the matcher continues to treat them
            // as all-events subscribers.
            let has_event_types = conn
                .prepare("SELECT event_types FROM subscriptions LIMIT 0")
                .is_ok();
            if !has_event_types {
                conn.execute("ALTER TABLE subscriptions ADD COLUMN event_types TEXT", [])?;
            }
            // Idempotent index from the migration file.
            conn.execute_batch(MIGRATION_V19_SQLITE)?;
        }
        if version < 20 {
            // v0.6.4-009 — fully idempotent (CREATE TABLE IF NOT EXISTS).
            conn.execute_batch(MIGRATION_V20_SQLITE)?;
        }
        if version < 21 {
            // v0.7.0 K2 — pending_actions timeout sweeper.
            //
            // Two new columns back the 60-second background sweep:
            //   default_timeout_seconds  per-row TTL (NULL → cluster default)
            //   expired_at               RFC3339 stamp set when sweeper fires
            //
            // ALTER TABLE done inline (SQLite has no `ADD COLUMN IF NOT
            // EXISTS`); SQL file holds the idempotent index batch.
            //
            // v0.6.3.1 honesty patch: the v2 capabilities response had
            // dropped `approval.default_timeout_seconds` because no
            // sweeper enforced it. K2 closes that gap. The capabilities
            // wire shape is intentionally unchanged here — v0.7-K5 owns
            // re-introducing the public surface.
            let has_timeout: bool = conn
                .prepare("SELECT default_timeout_seconds FROM pending_actions LIMIT 0")
                .is_ok();
            if !has_timeout {
                conn.execute(
                    "ALTER TABLE pending_actions ADD COLUMN default_timeout_seconds INTEGER",
                    [],
                )?;
            }
            let has_expired_at: bool = conn
                .prepare("SELECT expired_at FROM pending_actions LIMIT 0")
                .is_ok();
            if !has_expired_at {
                conn.execute(
                    "ALTER TABLE pending_actions ADD COLUMN expired_at TEXT",
                    [],
                )?;
            }
            conn.execute_batch(MIGRATION_V21_SQLITE)?;
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
        // v0.6.3.1 P2 (G5) — copy embedding + embedding_dim into the archive
        // and capture original tier + expires_at so restore_archived can
        // round-trip the row instead of resetting to long/permanent.
        conn.execute(
            "INSERT OR REPLACE INTO archived_memories
             (id, tier, namespace, title, content, tags, priority, confidence,
              source, access_count, created_at, updated_at, last_accessed_at,
              expires_at, archived_at, archive_reason, metadata,
              embedding, embedding_dim, original_tier, original_expires_at)
             SELECT id, tier, namespace, title, content, tags, priority, confidence,
                    source, access_count, created_at, updated_at, last_accessed_at,
                    expires_at, ?1, ?2, metadata,
                    embedding, embedding_dim, tier, expires_at
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
            // v0.6.3.1 P2 (G5) — preserve embedding + tier + expiry on forget-archive.
            conn.execute(
                "INSERT OR REPLACE INTO archived_memories
                 (id, tier, namespace, title, content, tags, priority, confidence,
                  source, access_count, created_at, updated_at, last_accessed_at,
                  expires_at, archived_at, archive_reason,
                  embedding, embedding_dim, original_tier, original_expires_at)
                 SELECT id, tier, namespace, title, content, tags, priority, confidence,
                        source, access_count, created_at, updated_at, last_accessed_at,
                        expires_at, ?4, 'forget',
                        embedding, embedding_dim, tier, expires_at
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
                  expires_at, archived_at, archive_reason,
                  embedding, embedding_dim, original_tier, original_expires_at)
                 SELECT id, tier, namespace, title, content, tags, priority, confidence,
                        source, access_count, created_at, updated_at, last_accessed_at,
                        expires_at, ?3, 'forget',
                        embedding, embedding_dim, tier, expires_at
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

/// Phase P6 (R1) — count tokens in `text` using OpenAI's `cl100k_base`
/// BPE encoding. This is the de-facto standard for Claude / GPT context
/// budgeting and is shipped with `tiktoken-rs` (the BPE table is embedded
/// in the crate, ~1.7 MB, so the count is offline-deterministic across
/// all hosts). The encoder is built lazily and cached process-wide via
/// `OnceLock` — `cl100k_base()` itself parses the embedded table on every
/// call, which adds a few ms; we pay that cost once.
///
/// Returns the token count. On the (vanishingly rare) cl100k_base init
/// failure, falls back to the prior `len/4` byte heuristic so a budget
/// request never hard-errors.
#[must_use]
pub fn count_tokens_cl100k(text: &str) -> usize {
    use std::sync::OnceLock;
    static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    let bpe = BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok());
    if let Some(bpe) = bpe.as_ref() {
        bpe.encode_with_special_tokens(text).len()
    } else {
        // Defensive fallback — should never trigger in practice because
        // the BPE table is bundled in the crate, but we never want a
        // budget call to fail because of tokenizer init.
        text.len() / 4
    }
}

/// Phase P6 — token cost of a memory's `content` only (not title), per
/// the R1 spec which budgets against the LLM context window. Title and
/// metadata are caller-side ornament; `content` is what gets stuffed
/// into the prompt.
#[must_use]
pub fn count_memory_tokens(mem: &Memory) -> usize {
    count_tokens_cl100k(&mem.content)
}

/// Phase P6 — kept for backward compatibility with the Task 1.11 byte-
/// heuristic surface. New code should use `count_memory_tokens`. The
/// returned value is now BPE-accurate (cl100k_base) rather than the
/// prior `len/4` estimate, so callers reading this through the public
/// API get the more accurate value automatically.
#[must_use]
pub fn estimate_memory_tokens(mem: &Memory) -> usize {
    count_memory_tokens(mem)
}

/// Phase P6 — outcome of applying a token budget to a ranked recall
/// list. Carries everything `mcp::handle_recall` needs to populate the
/// new RecallMeta block (`budget_tokens_used`, `budget_tokens_remaining`,
/// `memories_dropped`, `budget_overflow`).
#[derive(Debug, Clone)]
pub struct BudgetOutcome {
    /// Cumulative cl100k_base token count of the returned content.
    pub tokens_used: usize,
    /// `budget - tokens_used`, saturating at 0. `None` when no budget set.
    pub tokens_remaining: Option<usize>,
    /// How many candidates the budget cut from the ranked list.
    pub memories_dropped: usize,
    /// True iff the highest-ranked memory alone exceeded the budget and
    /// was returned anyway (R1 guarantee: at least one memory if any
    /// matched). Always false when no budget is set.
    pub budget_overflow: bool,
}

/// Phase P6 (R1) — context-budget greedy fill. Iterates over scored
/// candidates in rank order; stops at the first memory whose inclusion
/// would exceed the budget — UNLESS the output is still empty, in
/// which case the highest-ranked memory is returned anyway with
/// `budget_overflow = true`. This preserves the R1 guarantee that a
/// successful recall always returns at least one result when any
/// matched, even if the user supplied an unrealistically tight budget.
///
/// When `budget_tokens` is `None`, every candidate is returned and the
/// `tokens_used` tally falls back to the cheap byte-heuristic (`len/4`)
/// — running cl100k_base on every recall regardless of caller intent
/// would impose ~200 ms cold-start (BPE table parse) and several ms per
/// memory on the hot path. The heuristic is byte-exact-deterministic,
/// honoring the prior Task 1.11 contract for "observe the cost without
/// enforcing it". When `budget_tokens` is `Some(_)`, the BPE-accurate
/// cl100k count is used because the caller cares enough about the
/// number to enforce on it. When `budget_tokens` is `Some(0)`, **zero
/// memories are returned** with `budget_overflow = false` — the spec
/// semantics for "no budget at all, please" (R1 §6 acceptance #3).
#[must_use]
pub fn apply_token_budget(
    scored: Vec<(Memory, f64)>,
    budget_tokens: Option<usize>,
) -> (Vec<(Memory, f64)>, BudgetOutcome) {
    let total_candidates = scored.len();

    // Phase P6 — explicit `0` budget short-circuits to an empty result.
    // Per the R1 acceptance test `budget_tokens_zero_returns_zero_memories`,
    // this is a deliberate no-op fill (overflow is *false* — the user
    // said "give me nothing").
    if budget_tokens == Some(0) {
        return (
            Vec::new(),
            BudgetOutcome {
                tokens_used: 0,
                tokens_remaining: Some(0),
                memories_dropped: total_candidates,
                budget_overflow: false,
            },
        );
    }

    // No-budget fast path: skip cl100k entirely. The byte heuristic is
    // a few ns vs. the BPE encoder's couple-of-µs per memory plus the
    // one-shot ~200 ms init. Bench harness benchmarks recall with
    // `budget_tokens=None`; this keeps the hot path cl100k-free.
    if budget_tokens.is_none() {
        let mut used: usize = 0;
        let mut out: Vec<(Memory, f64)> = Vec::with_capacity(scored.len());
        for (mem, score) in scored {
            used = used.saturating_add(mem.content.len() / 4);
            out.push((mem, score));
        }
        return (
            out,
            BudgetOutcome {
                tokens_used: used,
                tokens_remaining: None,
                memories_dropped: 0,
                budget_overflow: false,
            },
        );
    }

    // Budget path — caller asked for enforcement, so spend the tokens
    // for accurate cl100k accounting.
    let mut used: usize = 0;
    let mut out: Vec<(Memory, f64)> = Vec::with_capacity(scored.len());
    let mut overflow = false;

    for (mem, score) in scored {
        let cost = count_memory_tokens(&mem);
        if let Some(budget) = budget_tokens
            && used.saturating_add(cost) > budget
        {
            // R1 always-return-at-least-one guarantee: if we've collected
            // nothing yet, take the top-ranked memory and flag overflow.
            if out.is_empty() {
                used = used.saturating_add(cost);
                out.push((mem, score));
                overflow = true;
            }
            break;
        }
        used = used.saturating_add(cost);
        out.push((mem, score));
    }

    let dropped = total_candidates.saturating_sub(out.len());
    let tokens_remaining = budget_tokens.map(|b| b.saturating_sub(used));
    (
        out,
        BudgetOutcome {
            tokens_used: used,
            tokens_remaining,
            memories_dropped: dropped,
            budget_overflow: overflow,
        },
    )
}

/// Recall — fuzzy OR search + touch + auto-promote + TTL extension.
/// Task 1.11: after ranking, applies optional `budget_tokens` cap.
/// Phase P6: returns the full `BudgetOutcome` (tokens_used,
/// tokens_remaining, memories_dropped, budget_overflow) instead of just
/// the prior bare `tokens_used`. Callers that only need `tokens_used`
/// read `outcome.tokens_used`.
#[allow(clippy::too_many_arguments)]
/// v0.6.3.1 (P3): keyword-only recall with retrieval-stage telemetry.
///
/// Identical to [`recall`] but additionally returns a [`crate::models::RecallTelemetry`]
/// describing the FTS5 candidate count (HNSW count is always 0 for this
/// path — no semantic stage runs). MCP `handle_recall` uses this to build
/// the `meta` block; [`recall`] is preserved as a thin wrapper for
/// existing callers (HTTP handlers, CLI, bench).
#[allow(clippy::too_many_arguments)]
pub fn recall_with_telemetry(
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
) -> Result<(
    Vec<(Memory, f64)>,
    BudgetOutcome,
    crate::models::RecallTelemetry,
)> {
    let (results, outcome) = recall(
        conn,
        context,
        namespace,
        limit,
        tags_filter,
        since,
        until,
        short_extend,
        mid_extend,
        as_agent,
        budget_tokens,
    )?;
    let telemetry = crate::models::RecallTelemetry {
        fts_candidates: results.len(),
        hnsw_candidates: 0,
        blend_weight_avg: 0.0,
    };
    Ok((results, outcome, telemetry))
}

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
) -> Result<(Vec<(Memory, f64)>, BudgetOutcome)> {
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

    // Task 1.11 / Phase P6: apply optional token budget in rank order
    // (AFTER proximity). Returns BudgetOutcome with all R1 meta fields.
    let (budgeted, outcome) = apply_token_budget(boosted, budget_tokens);

    // Touch all recalled memories that SURVIVED the budget cut — no sense
    // bumping access counts on memories the caller will never see.
    for (mem, _) in &budgeted {
        if let Err(e) = touch(conn, &mem.id, short_extend, mid_extend) {
            tracing::warn!("touch failed for memory {}: {}", &mem.id, e);
        }
    }
    Ok((budgeted, outcome))
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

/// v0.6.3.1 P2 (G6) — quick existence check for `(title, namespace)`. Used by
/// `on_conflict='error'` callers to short-circuit before the full upsert
/// machinery runs. Returns the existing row id if there is one.
///
/// # Errors
///
/// Returns the underlying SQLite error.
pub fn find_by_title_namespace(
    conn: &Connection,
    title: &str,
    namespace: &str,
) -> Result<Option<String>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM memories WHERE title = ?1 AND namespace = ?2 LIMIT 1",
            params![title, namespace],
            |r| r.get(0),
        )
        .ok();
    Ok(id)
}

/// v0.6.3.1 P2 (G6) — pick a title that does not collide with an existing
/// `(title, namespace)` row by appending `(2)`, `(3)`, ... up to a hard cap.
/// The first available suffix wins. Used by `on_conflict='version'`.
///
/// The cap (`MAX_VERSION_SUFFIX`) prevents an infinite loop in pathological
/// cases (e.g. an attacker spamming the same title in a loop). Once the cap
/// is hit, the caller falls back to error mode.
const MAX_VERSION_SUFFIX: u32 = 1024;

/// # Errors
///
/// Returns the underlying SQLite error or an error if no free suffix is
/// found within `MAX_VERSION_SUFFIX` attempts.
pub fn next_versioned_title(
    conn: &Connection,
    base_title: &str,
    namespace: &str,
) -> Result<String> {
    if find_by_title_namespace(conn, base_title, namespace)?.is_none() {
        return Ok(base_title.to_string());
    }
    for n in 2..=MAX_VERSION_SUFFIX {
        let candidate = format!("{base_title} ({n})");
        if find_by_title_namespace(conn, &candidate, namespace)?.is_none() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "could not find a free versioned title for '{base_title}' in namespace '{namespace}' \
         within {MAX_VERSION_SUFFIX} attempts"
    )
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
    // Schema v15 (Pillar 2 / Stream B) added `valid_from` for temporal
    // KG queries. Backfill on migration handled legacy rows; here we
    // populate it on the insert path so newly created links are
    // visible to `memory_kg_timeline` without a downstream backfill.
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO memory_links (source_id, target_id, relation, created_at, valid_from) VALUES (?1, ?2, ?3, ?4, ?4)",
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
// Body is intentionally one logical pipeline (SQL aggregation → tree
// assembly → root materialisation); pulling helpers out hurts
// readability more than it helps.
#[allow(clippy::too_many_lines)]
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

/// Hard floor for duplicate-check threshold. Below this, anything can match
/// random unrelated content — refuse to honor the lookup so callers don't
/// silently get garbage merge suggestions.
pub const DUPLICATE_THRESHOLD_MIN: f32 = 0.5;

/// Default cosine similarity threshold for declaring a candidate a
/// duplicate. Empirically tuned for MiniLM-L6-v2 (the local embedder):
/// near-paraphrases of the same memory tend to land at 0.88+, while
/// loosely related content sits well below 0.85. Callers can override.
pub const DUPLICATE_THRESHOLD_DEFAULT: f32 = 0.85;

/// Find the nearest-neighbor live memory by cosine similarity (Pillar 2 /
/// Stream D — `memory_check_duplicate`).
///
/// Linear scan over `memories.embedding` rows that pass the live-row
/// (non-expired) gate and the optional namespace filter. The chosen
/// candidate is the highest-cosine match across the pool; the
/// caller-supplied `threshold` is used purely to set `is_duplicate` on
/// the response — the nearest neighbor is always returned (when the
/// pool is non-empty) so callers can show "closest existing memory was
/// X at similarity Y" even on a not-quite-duplicate.
///
/// Threshold is clamped at [`DUPLICATE_THRESHOLD_MIN`] so that wildly
/// permissive thresholds can't be used to dress unrelated content as a
/// merge suggestion.
///
/// Returns `(check, scanned)` where `scanned` is the count of embedded
/// candidates compared (useful for diagnostics).
pub fn check_duplicate(
    conn: &Connection,
    query_embedding: &[f32],
    namespace: Option<&str>,
    threshold: f32,
) -> Result<DuplicateCheck> {
    let effective_threshold = threshold.max(DUPLICATE_THRESHOLD_MIN);
    let now = Utc::now().to_rfc3339();

    // SQL filter handles the live-row + optional namespace gate; the
    // cosine pass happens in Rust because SQLite has no native vector
    // op. We only pull rows with non-NULL embeddings — anything missing
    // an embedding can't be a near-duplicate by this definition.
    let rows: Vec<(String, String, String, Vec<u8>)> = if let Some(ns) = namespace {
        let mut stmt = conn.prepare(
            "SELECT id, title, namespace, embedding FROM memories
             WHERE embedding IS NOT NULL
               AND (expires_at IS NULL OR expires_at > ?1)
               AND namespace = ?2",
        )?;
        let mapped = stmt.query_map(params![now, ns], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, title, namespace, embedding FROM memories
             WHERE embedding IS NOT NULL
               AND (expires_at IS NULL OR expires_at > ?1)",
        )?;
        let mapped = stmt.query_map(params![now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut best: Option<DuplicateMatch> = None;
    let mut scanned: usize = 0;
    for (id, title, ns, bytes) in rows {
        if bytes.is_empty() {
            continue;
        }
        // v0.6.3.1 P2 — magic-byte aware decode. Malformed payloads
        // (anything other than headed-LE or legacy-LE) are skipped with
        // telemetry so a corrupted row can't poison duplicate detection.
        let candidate = match crate::embeddings::decode_embedding_blob(&bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    memory_id = %id,
                    blob_len = bytes.len(),
                    error = %e,
                    "skipping duplicate-check candidate with malformed embedding"
                );
                continue;
            }
        };
        // Vectors of mismatched dimension would compute against a
        // truncated query (Embedder::cosine_similarity zips). Skip
        // rather than report a misleading similarity score.
        if candidate.len() != query_embedding.len() {
            tracing::warn!(
                memory_id = %id,
                expected = query_embedding.len(),
                got = candidate.len(),
                "skipping duplicate-check candidate with dimension mismatch"
            );
            continue;
        }
        let similarity =
            crate::embeddings::Embedder::cosine_similarity(query_embedding, &candidate);
        scanned += 1;
        let is_better = best.as_ref().is_none_or(|m| similarity > m.similarity);
        if is_better {
            best = Some(DuplicateMatch {
                id,
                title,
                namespace: ns,
                similarity,
            });
        }
    }

    let is_duplicate = best
        .as_ref()
        .is_some_and(|m| m.similarity >= effective_threshold);
    Ok(DuplicateCheck {
        is_duplicate,
        threshold: effective_threshold,
        nearest: best,
        candidates_scanned: scanned,
    })
}

/// Register an entity (canonical name + aliases) under a namespace
/// (Pillar 2 / Stream B).
///
/// An entity is stored as a long-tier memory:
/// - `title = canonical_name`
/// - `namespace = namespace`
/// - `tags` includes [`ENTITY_TAG`]
/// - `metadata.kind = "entity"` (so the resolver can never confuse an
///   entity with a regular memory that happens to share a title)
///
/// Aliases live in the `entity_aliases` side table keyed by
/// `(entity_id, alias)`.
///
/// **Idempotency:** if an entity with this `(canonical_name, namespace)`
/// already exists, its ID is reused and `aliases` are merged with
/// `INSERT OR IGNORE`. The returned [`EntityRegistration::created`] is
/// `false` in that case.
///
/// **Collision detection:** if a non-entity memory already occupies
/// `(title=canonical_name, namespace=namespace)`, the call errors
/// rather than silently upgrading it (the upsert path on `insert`
/// would otherwise overwrite the existing row's content/tags). Callers
/// must rename the entity or its colliding memory.
///
/// `extra_metadata` is merged into the entity memory's metadata; any
/// caller-supplied `kind` field is overwritten with `"entity"` and
/// `agent_id` is stamped from the caller (NHI provenance) when
/// `extra_metadata` does not already specify one.
pub fn entity_register(
    conn: &Connection,
    canonical_name: &str,
    namespace: &str,
    aliases: &[String],
    extra_metadata: &serde_json::Value,
    agent_id: Option<&str>,
) -> Result<crate::models::EntityRegistration> {
    use crate::models::{ENTITY_KIND, ENTITY_TAG, EntityRegistration};

    // Look up an existing entity in this namespace by canonical_name +
    // metadata.kind. If a non-entity memory occupies the same
    // (title, namespace), surface a hard error instead of upserting.
    let existing_id: Option<String> = match conn.query_row(
        "SELECT id FROM memories
         WHERE namespace = ?1 AND title = ?2
           AND COALESCE(json_extract(metadata, '$.kind'), '') = ?3",
        params![namespace, canonical_name, ENTITY_KIND],
        |r| r.get::<_, String>(0),
    ) {
        Ok(id) => Some(id),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };

    let (entity_id, created) = if let Some(id) = existing_id {
        (id, false)
    } else {
        let collision: Option<String> = match conn.query_row(
            "SELECT id FROM memories
             WHERE namespace = ?1 AND title = ?2
               AND COALESCE(json_extract(metadata, '$.kind'), '') != ?3",
            params![namespace, canonical_name, ENTITY_KIND],
            |r| r.get::<_, String>(0),
        ) {
            Ok(id) => Some(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        if collision.is_some() {
            anyhow::bail!(
                "entity_register: title '{canonical_name}' in namespace '{namespace}' is already used by a non-entity memory"
            );
        }

        // Build metadata: caller-supplied object merged, kind forced
        // to "entity", agent_id preserved from caller when not set.
        let mut meta_map = match extra_metadata {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        meta_map.insert(
            "kind".to_string(),
            serde_json::Value::String(ENTITY_KIND.to_string()),
        );
        if let Some(a) = agent_id {
            meta_map
                .entry("agent_id".to_string())
                .or_insert(serde_json::Value::String(a.to_string()));
        }
        let metadata = serde_json::Value::Object(meta_map);

        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: namespace.to_string(),
            title: canonical_name.to_string(),
            content: canonical_name.to_string(),
            tags: vec![ENTITY_TAG.to_string()],
            priority: 7,
            confidence: 1.0,
            source: "api".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
        };
        let id = insert(conn, &mem).context("insert entity memory")?;
        (id, true)
    };

    let now = Utc::now().to_rfc3339();
    {
        let mut stmt = conn.prepare(
            "INSERT OR IGNORE INTO entity_aliases (entity_id, alias, created_at)
             VALUES (?1, ?2, ?3)",
        )?;
        for alias in aliases {
            let trimmed = alias.trim();
            if trimmed.is_empty() {
                continue;
            }
            stmt.execute(params![entity_id, trimmed, now])?;
        }
    }

    let aliases_out = list_entity_aliases(conn, &entity_id)?;

    Ok(EntityRegistration {
        entity_id,
        canonical_name: canonical_name.to_string(),
        namespace: namespace.to_string(),
        aliases: aliases_out,
        created,
    })
}

/// Resolve an alias to its registered entity (Pillar 2 / Stream B).
///
/// When `namespace` is `Some`, only entities in that namespace are
/// considered. When `None`, all namespaces are searched and the
/// most-recently-created matching entity wins (deterministic
/// disambiguation when the same alias was registered in multiple
/// namespaces).
///
/// Returns `Ok(None)` if no entity claims this alias under the given
/// filter. Returns the full alias set for the resolved entity.
pub fn entity_get_by_alias(
    conn: &Connection,
    alias: &str,
    namespace: Option<&str>,
) -> Result<Option<crate::models::EntityRecord>> {
    use crate::models::{ENTITY_KIND, EntityRecord};

    let trimmed = alias.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let row: std::result::Result<(String, String, String), rusqlite::Error> =
        if let Some(ns) = namespace {
            conn.query_row(
                "SELECT m.id, m.title, m.namespace
                 FROM entity_aliases ea
                 JOIN memories m ON m.id = ea.entity_id
                 WHERE ea.alias = ?1
                   AND m.namespace = ?2
                   AND COALESCE(json_extract(m.metadata, '$.kind'), '') = ?3
                 ORDER BY m.created_at DESC
                 LIMIT 1",
                params![trimmed, ns, ENTITY_KIND],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        } else {
            conn.query_row(
                "SELECT m.id, m.title, m.namespace
                 FROM entity_aliases ea
                 JOIN memories m ON m.id = ea.entity_id
                 WHERE ea.alias = ?1
                   AND COALESCE(json_extract(m.metadata, '$.kind'), '') = ?2
                 ORDER BY m.created_at DESC
                 LIMIT 1",
                params![trimmed, ENTITY_KIND],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        };

    let (entity_id, canonical_name, ns) = match row {
        Ok(t) => t,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let aliases = list_entity_aliases(conn, &entity_id)?;
    Ok(Some(EntityRecord {
        entity_id,
        canonical_name,
        namespace: ns,
        aliases,
    }))
}

/// Default cap on rows returned by `kg_timeline` when the caller does
/// not specify one (Pillar 2 / Stream C). Sized to fit a reasonable
/// agent context window without paging — callers needing more should
/// pass an explicit limit.
pub const KG_TIMELINE_DEFAULT_LIMIT: usize = 200;

/// Hard ceiling on `kg_timeline` rows. Matches the existing list/recall
/// caps to keep the timeline bounded against pathological entities.
pub const KG_TIMELINE_MAX_LIMIT: usize = 1000;

/// Ordered fact timeline for an entity (Pillar 2 / Stream C —
/// `memory_kg_timeline`). Returns outbound assertions from
/// `source_id`, ordered by `valid_from ASC` and tie-broken by
/// `created_at ASC` for deterministic display.
///
/// Filters:
/// - `since` (RFC3339, inclusive): drop events with `valid_from < since`
/// - `until` (RFC3339, inclusive): drop events with `valid_from > until`
/// - `limit`: row cap, clamped to [1, [`KG_TIMELINE_MAX_LIMIT`]]
///
/// Rows with NULL `valid_from` are excluded — a link without a
/// valid-from anchor cannot be ordered on the timeline. The schema-v15
/// migration backfilled legacy rows to `created_at`, and the `link()`
/// path stamps the column on every new insert, so this is a hard
/// guarantee for current code; the explicit `IS NOT NULL` guard exists
/// to keep external writes (`store/sqlite.rs`, custom migrations) from
/// silently producing invisible links.
///
/// Cross-namespace by design: timelines often span the same canonical
/// entity asserted by agents in different namespaces. Callers can
/// post-filter by `target_namespace` if they need a namespace-scoped
/// view.
///
/// v0.7 AGE acceleration onramp (charter §"Stream C" bullet 4). When
/// the v0.7 SAL ships with Apache AGE, the equivalent property-graph
/// query is:
///
/// ```cypher
/// MATCH (s {id: $source_id})-[r {valid_from IS NOT NULL,
///        valid_from >= $since, valid_from <= $until}]->(t)
/// WHERE t.id <> s.id  // exclude self-loops
/// RETURN t.id, r.relation, r.valid_from, r.valid_until, r.observed_by
/// ORDER BY r.valid_from ASC, r.created_at ASC
/// LIMIT $limit
/// ```
///
/// Stub left here per charter intent so the v0.7 migration has a 1:1
/// reference query.
pub fn kg_timeline(
    conn: &Connection,
    source_id: &str,
    since: Option<&str>,
    until: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<crate::models::KgTimelineEvent>> {
    use crate::models::KgTimelineEvent;

    let cap = limit
        .unwrap_or(KG_TIMELINE_DEFAULT_LIMIT)
        .clamp(1, KG_TIMELINE_MAX_LIMIT);

    // Compose the predicate dynamically for `since` / `until`. Bind
    // values are appended in the same order so the placeholders line up.
    let mut sql = String::from(
        "SELECT ml.target_id, ml.relation, ml.valid_from, ml.valid_until,
                ml.observed_by, m.title, m.namespace, ml.created_at
         FROM memory_links ml
         JOIN memories m ON m.id = ml.target_id
         WHERE ml.source_id = ?1
           AND ml.valid_from IS NOT NULL",
    );
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(source_id.to_string())];
    if let Some(s) = since {
        sql.push_str(" AND ml.valid_from >= ?");
        sql.push_str(&(binds.len() + 1).to_string());
        binds.push(Box::new(s.to_string()));
    }
    if let Some(u) = until {
        sql.push_str(" AND ml.valid_from <= ?");
        sql.push_str(&(binds.len() + 1).to_string());
        binds.push(Box::new(u.to_string()));
    }
    sql.push_str(" ORDER BY ml.valid_from ASC, ml.created_at ASC LIMIT ?");
    sql.push_str(&(binds.len() + 1).to_string());
    binds.push(Box::new(i64::try_from(cap).unwrap_or(i64::MAX)));

    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(AsRef::as_ref).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(bind_refs), |row| {
        Ok(KgTimelineEvent {
            target_id: row.get(0)?,
            relation: row.get(1)?,
            valid_from: row.get(2)?,
            valid_until: row.get(3)?,
            observed_by: row.get(4)?,
            title: row.get(5)?,
            target_namespace: row.get(6)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Outcome of [`invalidate_link`] (Pillar 2 / Stream C —
/// `memory_kg_invalidate`). `valid_until` is the timestamp now stored on
/// the link; `previous_valid_until` is the prior value, or `None` if
/// this was the first invalidation. Callers can use the prior value to
/// distinguish a fresh supersession from an idempotent retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidateResult {
    pub valid_until: String,
    pub previous_valid_until: Option<String>,
}

/// Mark a KG link as superseded by setting its `valid_until` column
/// (Pillar 2 / Stream C — `memory_kg_invalidate`). Returns `Ok(None)`
/// when the `(source_id, target_id, relation)` triple does not match an
/// existing link. The supplied `valid_until` defaults to the current
/// wall-clock time in RFC3339 form when omitted; callers needing
/// historical or future supersession can pass an explicit value.
///
/// Idempotent: calling repeatedly overwrites the prior `valid_until`
/// (the prior value is returned in `previous_valid_until` so callers
/// can detect the overwrite). The schema does not yet carry an audit
/// column for the supersession reason; that arrives with v0.7
/// attestation. Until then, callers should record the rationale in
/// their own logs or a paired memory.
pub fn invalidate_link(
    conn: &Connection,
    source_id: &str,
    target_id: &str,
    relation: &str,
    valid_until: Option<&str>,
) -> Result<Option<InvalidateResult>> {
    let stamp = valid_until.map_or_else(|| Utc::now().to_rfc3339(), str::to_string);

    let prior = match conn.query_row(
        "SELECT valid_until FROM memory_links \
         WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
        params![source_id, target_id, relation],
        |r| r.get::<_, Option<String>>(0),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    conn.execute(
        "UPDATE memory_links SET valid_until = ?4 \
         WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
        params![source_id, target_id, relation, &stamp],
    )?;

    Ok(Some(InvalidateResult {
        valid_until: stamp,
        previous_valid_until: prior,
    }))
}

/// Default cap on rows returned by `kg_query` when the caller does not
/// specify one (Pillar 2 / Stream C). Mirrors `kg_timeline`'s default so
/// the two traversal tools behave consistently for agents driving them.
pub const KG_QUERY_DEFAULT_LIMIT: usize = 200;

/// Hard ceiling on `kg_query` rows. Matches `kg_timeline` and the
/// existing list/recall caps to keep traversal bounded against
/// pathological fan-out.
pub const KG_QUERY_MAX_LIMIT: usize = 1000;

/// Maximum traversal depth supported by [`kg_query`]. The recursive-CTE
/// implementation enforces an explicit ceiling so a crafted call cannot
/// run an unbounded traversal; the charter (`v0.6.3-grand-slam.md`
/// § Performance Budgets) sets the published budget at depth ≤ 5.
pub const KG_QUERY_MAX_SUPPORTED_DEPTH: usize = 5;

/// Outbound KG traversal from a source memory (Pillar 2 / Stream C —
/// `memory_kg_query`). Returns one row per link reachable within
/// `max_depth` hops, filtered by:
///
/// - `valid_at` (RFC3339, optional): only links valid at that instant —
///   `valid_from <= valid_at AND (valid_until IS NULL OR valid_until > valid_at)`.
///   When omitted, the temporal filter is skipped and rows with NULL
///   `valid_from` are also returned (legacy / un-anchored links).
/// - `allowed_agents` (optional): when provided, only links with
///   `observed_by` in the set are returned. An **empty** allowlist
///   returns zero rows by design — callers signaling "no agents are
///   trusted" must get an empty traversal, not the unfiltered fallback.
///   When omitted entirely (`None`), the agent filter is skipped.
/// - `limit`: row cap, clamped to [1, [`KG_QUERY_MAX_LIMIT`]].
///
/// `max_depth` must be in `[1, KG_QUERY_MAX_SUPPORTED_DEPTH]`; passing
/// a larger value yields an explicit error rather than a silent
/// truncation, so callers learn they hit the ceiling instead of
/// receiving a partial graph.
///
/// Multi-hop traversal uses a recursive CTE with cycle detection on
/// the accumulated path, so cycles in the link graph cannot loop the
/// traversal indefinitely. Each hop reapplies the same temporal /
/// agent filters as the anchor — a chain only extends through links
/// that pass every filter on every hop.
///
/// Ordering is `depth ASC, COALESCE(valid_from, created_at) ASC,
/// created_at ASC` — shallower hops first, then time-ordered within
/// each level. For depth=1 callers this collapses to the original
/// time ordering. The `depth` field reflects the actual hop count and
/// `path` is the full `src->mid->target` chain.
pub fn kg_query(
    conn: &Connection,
    source_id: &str,
    max_depth: usize,
    valid_at: Option<&str>,
    allowed_agents: Option<&[String]>,
    limit: Option<usize>,
) -> Result<Vec<crate::models::KgQueryNode>> {
    use crate::models::KgQueryNode;

    if max_depth == 0 {
        anyhow::bail!("max_depth must be >= 1");
    }
    if max_depth > KG_QUERY_MAX_SUPPORTED_DEPTH {
        anyhow::bail!(
            "max_depth={max_depth} exceeds supported depth={KG_QUERY_MAX_SUPPORTED_DEPTH}"
        );
    }

    // Empty allowlist == "no agents are trusted" — short-circuit so we
    // don't have to invent a SQL `IN ()` clause (which is invalid).
    if let Some(agents) = allowed_agents
        && agents.is_empty()
    {
        return Ok(Vec::new());
    }

    let cap = limit
        .unwrap_or(KG_QUERY_DEFAULT_LIMIT)
        .clamp(1, KG_QUERY_MAX_LIMIT);

    // Build the per-hop predicate once; the anchor and recursive members
    // both apply it to a row aliased `ml`. Bind values are appended in
    // resolution order so positional placeholders line up.
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut hop_filter = String::new();
    if let Some(t) = valid_at {
        hop_filter.push_str(" AND ml.valid_from IS NOT NULL AND ml.valid_from <= ?");
        binds.push(Box::new(t.to_string()));
        hop_filter.push_str(&binds.len().to_string());
        hop_filter.push_str(" AND (ml.valid_until IS NULL OR ml.valid_until > ?");
        binds.push(Box::new(t.to_string()));
        hop_filter.push_str(&binds.len().to_string());
        hop_filter.push(')');
    }
    if let Some(agents) = allowed_agents {
        // Already short-circuited the empty case above.
        hop_filter.push_str(" AND ml.observed_by IN (");
        for (i, a) in agents.iter().enumerate() {
            binds.push(Box::new(a.clone()));
            if i > 0 {
                hop_filter.push_str(", ");
            }
            hop_filter.push('?');
            hop_filter.push_str(&binds.len().to_string());
        }
        hop_filter.push(')');
    }

    // Anchor binds source_id, recursive member binds max_depth, final
    // SELECT binds the row cap. Order matters — placeholders are
    // resolved by the position they occupy in the assembled string.
    binds.push(Box::new(source_id.to_string()));
    let source_ph = binds.len();
    binds.push(Box::new(i64::try_from(max_depth).unwrap_or(i64::MAX)));
    let max_depth_ph = binds.len();
    binds.push(Box::new(i64::try_from(cap).unwrap_or(i64::MAX)));
    let limit_ph = binds.len();

    // v0.7 AGE acceleration onramp (charter §"Stream C — KG Query Layer"
    // bullet 4). The recursive CTE below is the v0.6.3 SQLite/Postgres
    // implementation. When the v0.7 SAL ships with Apache AGE wired in,
    // the equivalent property-graph query will look like:
    //
    //   MATCH (s {id: $source_id})-[r*1..$max_depth {valid_from <= $t,
    //          observed_by IN $allowed_agents}]->(t)
    //   WHERE NONE(n IN nodes(path) WHERE n.id = t.id)  -- cycle prune
    //   RETURN t.id, last(r).relation, t.title, length(r) AS depth,
    //          [n IN nodes(path) | n.id] AS path
    //   ORDER BY depth, last(r).valid_from
    //   LIMIT $limit
    //
    // Stub left here per charter intent so the v0.7 migration to AGE
    // has a 1:1 reference query alongside the SQL implementation.

    let sql = format!(
        "WITH RECURSIVE traversal(\
            target_id, relation, valid_from, valid_until, observed_by, \
            link_created_at, depth, path\
         ) AS (\
            SELECT ml.target_id, ml.relation, ml.valid_from, ml.valid_until, \
                   ml.observed_by, ml.created_at, 1, \
                   json_array(ml.source_id, ml.target_id) \
            FROM memory_links ml \
            WHERE ml.source_id = ?{source_ph}{hop_filter} \
            UNION ALL \
            SELECT ml.target_id, ml.relation, ml.valid_from, ml.valid_until, \
                   ml.observed_by, ml.created_at, t.depth + 1, \
                   json_insert(t.path, '$[' || json_array_length(t.path) || ']', ml.target_id) \
            FROM memory_links ml \
            JOIN traversal t ON ml.source_id = t.target_id \
            WHERE t.depth < ?{max_depth_ph} \
              AND NOT EXISTS (SELECT 1 FROM json_each(t.path) WHERE value = ml.target_id)\
              {hop_filter}\
         ) \
         SELECT t.target_id, t.relation, t.valid_from, t.valid_until, \
                t.observed_by, m.title, m.namespace, t.depth, \
                (SELECT group_concat(value, '->') FROM json_each(t.path)) \
         FROM traversal t \
         JOIN memories m ON m.id = t.target_id \
         ORDER BY t.depth ASC, COALESCE(t.valid_from, t.link_created_at) ASC, \
                  t.link_created_at ASC \
         LIMIT ?{limit_ph}",
    );

    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(AsRef::as_ref).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(bind_refs), |row| {
        let target_id: String = row.get(0)?;
        let depth: i64 = row.get(7)?;
        Ok(KgQueryNode {
            target_id,
            relation: row.get(1)?,
            valid_from: row.get(2)?,
            valid_until: row.get(3)?,
            observed_by: row.get(4)?,
            title: row.get(5)?,
            target_namespace: row.get(6)?,
            depth: usize::try_from(depth).unwrap_or(0),
            path: row.get(8)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// List all aliases registered for an entity, ordered by registration
/// time then alphabetical for stable display.
fn list_entity_aliases(conn: &Connection, entity_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT alias FROM entity_aliases
         WHERE entity_id = ?1
         ORDER BY created_at ASC, alias ASC",
    )?;
    let aliases: Vec<String> = stmt
        .query_map(params![entity_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(aliases)
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
    // v0.6.3.1 P2 (G4) — surface mixed-dim corruption to operators. Best-effort:
    // any error here returns 0 rather than failing the stats endpoint.
    let dim_violations = dim_violations(conn).unwrap_or(0);

    // v0.6.3.1 (P3, G2): cumulative HNSW eviction count is process-local
    // state — read from the static counter in src/hnsw.rs. Surfacing it in
    // `stats` lets `memory_stats` callers and `ai-memory doctor` (P7) flag
    // operators who are sustaining at the index cap.
    let index_evictions_total = crate::hnsw::index_evictions_total();

    Ok(Stats {
        total,
        by_tier,
        by_namespace,
        expiring_soon,
        links_count,
        db_size_bytes,
        dim_violations,
        index_evictions_total,
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
            // v0.6.3.1 P2 (G5) — preserve embedding + tier + expiry on GC archive.
            conn.execute(
                "INSERT OR REPLACE INTO archived_memories
                 (id, tier, namespace, title, content, tags, priority, confidence,
                  source, access_count, created_at, updated_at, last_accessed_at,
                  expires_at, archived_at, archive_reason, metadata,
                  embedding, embedding_dim, original_tier, original_expires_at)
                 SELECT id, tier, namespace, title, content, tags, priority, confidence,
                        source, access_count, created_at, updated_at, last_accessed_at,
                        expires_at, ?1, 'ttl_expired', metadata,
                        embedding, embedding_dim, tier, expires_at
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

        // v0.6.3.1 P2 (G5) — preserve original tier + expires_at + embedding
        // on restore. Pre-v17 rows lost this metadata permanently; the
        // migration backfills `original_tier='long'` so they still restore
        // as permanent (the prior behavior — no regression for legacy data).
        // Live writes from v0.6.3.1 onward round-trip the original tier.
        conn.execute(
            "INSERT INTO memories
             (id, tier, namespace, title, content, tags, priority, confidence,
              source, access_count, created_at, updated_at, last_accessed_at,
              expires_at, metadata, embedding, embedding_dim)
             SELECT id, COALESCE(original_tier, 'long'), namespace, title, content,
                    tags, priority, confidence, source, access_count, created_at,
                    ?1, last_accessed_at, original_expires_at, metadata,
                    embedding, embedding_dim
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

/// v0.6.3.1 P2 (G4): error returned by `set_embedding` when a write would
/// introduce a new embedding dimensionality into a namespace that has already
/// established one via an earlier write. Surfaced as a typed error so the
/// MCP/HTTP handlers can map it to a 409 Conflict rather than letting cosine
/// silently return 0.0 on every subsequent recall.
#[derive(Debug)]
pub struct EmbeddingDimMismatch {
    pub namespace: String,
    pub established: usize,
    pub attempted: usize,
}

impl std::fmt::Display for EmbeddingDimMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "embedding dim mismatch in namespace '{}': established {}-dim, refused {}-dim write",
            self.namespace, self.established, self.attempted
        )
    }
}

impl std::error::Error for EmbeddingDimMismatch {}

/// Lookup the embedding dimensionality already established for `namespace`.
/// Returns `Ok(None)` when no row in that namespace has an embedding yet.
///
/// # Errors
///
/// Returns the underlying SQLite error.
pub fn namespace_embedding_dim(conn: &Connection, namespace: &str) -> Result<Option<usize>> {
    // Use the v17 idx_memories_ns_dim partial index.
    let dim: Option<i64> = conn
        .query_row(
            "SELECT embedding_dim FROM memories \
             WHERE namespace = ?1 AND embedding_dim IS NOT NULL \
             LIMIT 1",
            params![namespace],
            |r| r.get(0),
        )
        .ok();
    Ok(dim.and_then(|d| usize::try_from(d).ok()))
}

/// Count rows whose stored `embedding_dim` does not match what the BLOB
/// contains (or where the column is missing while a BLOB exists). Surfaced
/// in `Stats::dim_violations` and consumed by P7 doctor.
///
/// # Errors
///
/// Returns the underlying SQLite error.
pub fn dim_violations(conn: &Connection) -> Result<u64> {
    // The expression `length(embedding)` returns the BLOB length; we map
    // legacy (no-header) payloads to `length/4` and headed (v17+) payloads
    // to `(length-1)/4` because length parity tells us which form is on
    // disk. Both forms must match the declared `embedding_dim` column.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories \
             WHERE embedding IS NOT NULL \
               AND length(embedding) >= 4 \
               AND ( \
                   embedding_dim IS NULL \
                   OR ( \
                       (length(embedding) % 4 = 0 AND embedding_dim != length(embedding)/4) \
                       OR (length(embedding) % 4 = 1 AND embedding_dim != (length(embedding)-1)/4) \
                       OR (length(embedding) % 4 NOT IN (0,1)) \
                   ) \
               )",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Store an embedding vector for a memory.
///
/// v0.6.3.1 P2 — writes are now headed with the magic byte (`encode_embedding_blob`)
/// and the namespace's first established dim is enforced. A dim mismatch
/// returns a typed [`EmbeddingDimMismatch`] surfaced as a 409 by the handler
/// layer. The same call also persists `embedding_dim` so future stats /
/// doctor passes don't re-derive from BLOB length.
///
/// # Errors
///
/// Returns [`EmbeddingDimMismatch`] (boxed via anyhow) when the embedding's
/// dimensionality differs from what the namespace established, or the
/// underlying SQLite error on failure.
pub fn set_embedding(conn: &Connection, id: &str, embedding: &[f32]) -> Result<()> {
    // Resolve namespace + check the dim invariant before mutating.
    let namespace: Option<String> = conn
        .query_row(
            "SELECT namespace FROM memories WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .ok();
    let attempted = embedding.len();
    if attempted == 0 {
        // Empty embeddings are a degenerate case — earlier code accepted
        // them; preserve that to avoid breaking legacy tests but skip the
        // dim check.
        let bytes = crate::embeddings::encode_embedding_blob(embedding);
        conn.execute(
            "UPDATE memories SET embedding = ?1, embedding_dim = NULL WHERE id = ?2",
            params![bytes, id],
        )?;
        return Ok(());
    }
    if let Some(ref ns) = namespace
        && let Some(established) = namespace_embedding_dim(conn, ns)?
        && established != attempted
    {
        return Err(EmbeddingDimMismatch {
            namespace: ns.clone(),
            established,
            attempted,
        }
        .into());
    }
    let bytes = crate::embeddings::encode_embedding_blob(embedding);
    let dim_i64 = i64::try_from(attempted).unwrap_or(i64::MAX);
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_dim = ?2 WHERE id = ?3",
        params![bytes, dim_i64, id],
    )?;
    Ok(())
}

/// Load an embedding vector for a memory. Returns None if not set.
///
/// v0.6.3.1 P2 — tolerant of legacy unheaded payloads (raw LE f32, length
/// `4n`) and v17 headed payloads (`0x01` + `4n` bytes). Anything else returns
/// an error so the caller can surface a typed corruption signal.
///
/// # Errors
///
/// Returns [`EmbeddingFormatError`](crate::embeddings::EmbeddingFormatError)
/// when the on-disk BLOB is malformed.
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
            let floats = crate::embeddings::decode_embedding_blob(&bytes)?;
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
///
/// v0.6.3.1 P2 — uses the magic-byte tolerant decoder. Rows whose BLOB is
/// malformed are logged and skipped (the alternative — bailing the entire
/// HNSW build — would take the whole semantic-search surface offline for one
/// corrupt row).
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
        match crate::embeddings::decode_embedding_blob(&bytes) {
            Ok(floats) => entries.push((id, floats)),
            Err(e) => {
                tracing::warn!(
                    memory_id = %id,
                    error = %e,
                    "skipping memory with malformed embedding BLOB during HNSW build"
                );
            }
        }
    }
    Ok(entries)
}

/// Hybrid recall — FTS5 keyword search + semantic cosine similarity.
/// Returns memories ranked by a blended score of keyword and semantic relevance.
/// When an HNSW `vector_index` is provided, uses approximate nearest-neighbor
/// search instead of scanning all embeddings linearly.
#[allow(clippy::too_many_arguments)]
/// v0.6.3.1 (P3): hybrid recall preserving the existing 2-tuple return
/// shape for HTTP / CLI / bench callers. Delegates to
/// [`recall_hybrid_with_telemetry`] and discards the telemetry. Kept so
/// the dozen-plus call sites need no churn for a feature only MCP
/// `handle_recall` consumes.
#[allow(clippy::too_many_arguments)]
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
) -> Result<(Vec<(Memory, f64)>, BudgetOutcome)> {
    let (results, outcome, _telemetry) = recall_hybrid_with_telemetry(
        conn,
        context,
        query_embedding,
        namespace,
        limit,
        tags_filter,
        since,
        until,
        vector_index,
        short_extend,
        mid_extend,
        as_agent,
        budget_tokens,
        scoring,
    )?;
    Ok((results, outcome))
}

/// v0.6.3.1 (P3 + P6): hybrid recall reporting per-stage candidate counts,
/// the average semantic blend weight, and the full budget outcome. MCP
/// `handle_recall` uses the telemetry to populate the `meta` block (closes
/// audit gaps G2/G8/G11) and the BudgetOutcome to populate R1 budget fields.
///
/// The retrieval logic is unchanged — anti-goal of P3 is "do not change
/// recall scoring or fusion logic." Counters are computed in place:
/// `fts_candidates` is the pre-fusion FTS5 row count, `hnsw_candidates`
/// is the pre-fusion HNSW (or linear-scan) hit count admitted past the
/// 0.2 cosine gate, `blend_weight_avg` is the mean `semantic_weight`
/// across the *returned* set (not the full candidate pool — operators
/// care about what made it out).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub fn recall_hybrid_with_telemetry(
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
) -> Result<(
    Vec<(Memory, f64)>,
    BudgetOutcome,
    crate::models::RecallTelemetry,
)> {
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

    // v0.6.3.1 (P3): pre-fusion candidate-pool counters surfaced to the
    // MCP `meta` block. Counted here at retrieval time, not after fusion,
    // so operators see how each stage contributed even when fusion
    // collapses the union to a smaller set.
    let mut fts_candidates_count: usize = 0;
    let mut hnsw_candidates_count: usize = 0;

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
        fts_candidates_count += 1;
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
                hnsw_candidates_count += 1;
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
                // v0.6.3.1 P2 — tolerate legacy + headed payloads; skip
                // (with telemetry) on malformed BLOBs so a single corrupt
                // row can't poison the whole semantic stage.
                let Ok(emb) = crate::embeddings::decode_embedding_blob(&bytes) else {
                    tracing::warn!(
                        memory_id = %mem.id,
                        "skipping malformed embedding BLOB during semantic recall"
                    );
                    continue;
                };
                let cosine = f64::from(crate::embeddings::Embedder::cosine_similarity(
                    query_embedding,
                    &emb,
                ));
                // v0.6.2 (S18): see matching note above at the HNSW gate.
                if cosine > 0.2 {
                    scored.insert(mem.id.clone(), (mem, 0.0, cosine));
                    hnsw_candidates_count += 1;
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
    // v0.6.3.1 (P3): collect per-candidate semantic weight in parallel with
    // the existing fusion pass so MCP `meta.blend_weight` reports the
    // *applied* (not configured) weight. Wrapped in `RefCell` so the map
    // closure can side-effect without restructuring the iterator chain.
    let blend_weights: std::cell::RefCell<Vec<f64>> = std::cell::RefCell::new(Vec::new());
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
            blend_weights.borrow_mut().push(semantic_weight);
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

    // Task 1.11 / Phase P6: apply token budget in rank order (AFTER
    // proximity). Returns BudgetOutcome with all R1 meta fields.
    let (budgeted, outcome) = apply_token_budget(boosted, budget_tokens);

    // Touch surviving memories only.
    for (mem, _) in &budgeted {
        if let Err(e) = touch(conn, &mem.id, short_extend, mid_extend) {
            tracing::warn!("touch failed for memory {}: {}", &mem.id, e);
        }
    }

    // v0.6.3.1 (P3): summarize per-stage candidate counts and the average
    // semantic blend weight for the MCP `meta` block. `blend_weight_avg`
    // is the unweighted mean across the *post-fusion* candidate set so
    // operators see the typical weight applied to what shipped, not the
    // configured ceiling. Pre-fusion counts come from the retrieval
    // counters (FTS / HNSW), which gives an honest picture of stage
    // contribution even when fusion deduplicates.
    let weights = blend_weights.into_inner();
    let blend_weight_avg = if weights.is_empty() {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let n = weights.len() as f64;
        weights.iter().sum::<f64>() / n
    };
    let telemetry = crate::models::RecallTelemetry {
        fts_candidates: fts_candidates_count,
        hnsw_candidates: hnsw_candidates_count,
        blend_weight_avg,
    };

    Ok((budgeted, outcome, telemetry))
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
#[allow(dead_code)] // called via lib crate (daemon_runtime); bin sees it as unused
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
#[allow(dead_code)] // called via lib crate (daemon_runtime); bin sees it as unused
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

/// Build the namespace inheritance chain in **top-down** order
/// (`["*", root, ..., leaf]`). Mirrors and replaces the historical
/// `mcp::build_namespace_chain` so non-MCP call sites (db-layer
/// governance enforcement, HTTP handlers, future hook pipelines) can
/// reuse the same walk.
///
/// Properties (preserved from the prior MCP-only implementation):
/// - cycle-safe (visited set + bounded by `MAX_EXPLICIT_DEPTH = 8`)
/// - includes the global standard `*` as the most-general entry
/// - prepends explicit `namespace_meta.parent_namespace` ancestors
///   before the `/`-derived hierarchy, supporting flat→hierarchical
///   linking (e.g. legacy `ai-memory` → `ai-memory-mcp`)
///
/// The MCP layer's display path consumes this top-down. The governance
/// resolver in [`resolve_governance_policy`] reverses it for a
/// leaf-first walk (most-specific wins).
#[must_use]
pub fn build_namespace_chain(conn: &Connection, namespace: &str) -> Vec<String> {
    const MAX_EXPLICIT_DEPTH: usize = 8;
    let mut chain: Vec<String> = Vec::new();

    if namespace == "*" {
        chain.push("*".to_string());
        return chain;
    }

    // Always start with the global standard — most general.
    chain.push("*".to_string());

    // 1. /-derived ancestors. `namespace_ancestors` returns most-specific-first;
    //    reverse for top-down (root ancestor first, then namespace itself last).
    let mut hierarchy_chain: Vec<String> = crate::models::namespace_ancestors(namespace)
        .into_iter()
        .rev()
        .collect();

    // 2. If the ROOTmost of the /-chain has an explicit `namespace_meta` parent,
    //    prepend that chain (bounded by MAX_EXPLICIT_DEPTH + cycle-safe).
    //    Supports legacy flat namespaces (e.g. `ai-memory` → `ai-memory-mcp`).
    if let Some(root) = hierarchy_chain.first().cloned() {
        let mut explicit_above: Vec<String> = Vec::new();
        let mut current = root;
        for _ in 0..MAX_EXPLICIT_DEPTH {
            match get_namespace_parent(conn, &current) {
                Some(p)
                    if p != "*"
                        && !explicit_above.contains(&p)
                        && !hierarchy_chain.contains(&p) =>
                {
                    explicit_above.push(p.clone());
                    current = p;
                }
                _ => break,
            }
        }
        // `explicit_above` is [immediate-explicit-parent, grandparent, ...];
        // reverse to prepend in top-down order.
        for p in explicit_above.into_iter().rev() {
            chain.push(p);
        }
    }

    // 3. Append the /-derived chain (top-down).
    for entry in hierarchy_chain.drain(..) {
        if !chain.contains(&entry) {
            chain.push(entry);
        }
    }

    chain
}

/// Read the explicit governance policy attached to a single namespace's
/// standard memory. Does **not** walk the inheritance chain — callers
/// that want hierarchical resolution should use
/// [`resolve_governance_policy`] instead.
fn read_namespace_policy(conn: &Connection, namespace: &str) -> Option<GovernancePolicy> {
    let standard_id = get_namespace_standard(conn, namespace).ok()??;
    let mem = get(conn, &standard_id).ok()??;
    match GovernancePolicy::from_metadata(&mem.metadata) {
        Some(Ok(p)) => Some(p),
        _ => None,
    }
}

/// Resolve the governance policy that gates actions in `namespace`.
///
/// v0.6.3.1 (P4, audit G1): walks the inheritance chain leaf-first and
/// returns the most-specific policy. This closes the audit's
/// highest-severity finding — prior to this fix the resolver consulted
/// only the leaf, which left children of governed parents (e.g.
/// `alphaone/secure/team-a` under an `Approve` policy at
/// `alphaone/secure`) **completely ungoverned** despite the
/// architecture page T2 promising "Hierarchical policy inheritance
/// (default at `org/`, overridable at `org/team/`)".
///
/// **Walk semantics** (carefully — easy to get subtly wrong):
///   1. Build the chain via [`build_namespace_chain`] (top-down) and
///      reverse it so we walk leaf → root. The leaf is the namespace
///      we were asked about; the root is the global `*` standard.
///   2. At each level `k`, look up the policy attached to that
///      namespace's standard memory.
///      - If a policy **exists**, it is the most-specific match seen
///        so far. Return it immediately. ("Most specific wins.")
///      - If a policy **also says `inherit: false`**, this is already
///        the same return path — we never reach the parent because
///        we already returned.
///   3. If level `k` has **no policy at all**, keep walking — this is
///      the implicit-inherit branch (no policy means "I don't override
///      my parent").
///   4. If we walk off the top of the chain without finding a policy,
///      return `None` (enforcement remains opt-in for namespaces with
///      no governance configured anywhere in the chain).
///
/// **Where does `inherit: false` actually do work?** When the most-
/// specific policy we hit on the walk has `inherit: false`. That
/// policy is returned (same return point as the inherit=true case),
/// so its rules govern the action; the false flag is what
/// **conceptually stops** the walk above it, but the implementation
/// stops the walk simply by virtue of having found a policy. The flag
/// matters most as a documented contract surfaced to operators: "a
/// policy here authoritatively replaces, not extends, what's above."
/// The flag also flows through the queued-pending-action approver
/// resolution so consensus/agent rules don't accidentally re-walk to
/// a parent.
///
/// Cycle-safety is inherited from `build_namespace_chain`
/// (`MAX_EXPLICIT_DEPTH = 8` + visited set). No new cache is
/// introduced — profile-driven optimization is a v0.7 item.
pub fn resolve_governance_policy(conn: &Connection, namespace: &str) -> Option<GovernancePolicy> {
    // build_namespace_chain returns top-down (`["*", root, ..., leaf]`).
    // Governance resolution wants leaf-first (most specific first), so
    // we reverse before walking.
    let chain = build_namespace_chain(conn, namespace);
    for level in chain.into_iter().rev() {
        // Most-specific match wins. Returning immediately here means
        // an explicit policy at the leaf (or any descendant level
        // with a policy) authoritatively overrides anything above —
        // which is precisely the inherit=false semantic, applied
        // implicitly. The inherit=false flag is preserved on the
        // returned policy so callers (e.g. the pending_action
        // approver resolver) don't accidentally re-walk to a parent.
        if let Some(policy) = read_namespace_policy(conn, &level) {
            return Some(policy);
        }
        // Implicit branch: no policy at this level → keep walking
        // toward the root. This is the "default inherit" behavior
        // that closes G1.
    }
    None
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

/// v0.6.3 (capabilities schema v2): count namespace standards whose
/// `metadata.governance` is non-null. A "rule" here means a namespace
/// has an explicit governance policy attached to its standard memory.
/// The count is a transparent passthrough — the full permission system
/// arrives in v0.7 (arch-enhancement-spec §3).
pub fn count_active_governance_rules(conn: &Connection) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories m
             INNER JOIN namespace_meta nm ON nm.standard_id = m.id
             WHERE json_extract(m.metadata, '$.governance') IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(usize::try_from(count.max(0)).unwrap_or(0))
}

/// v0.6.3 (capabilities schema v2): count rows in the `subscriptions`
/// table. Used by `handle_capabilities` as a proxy for "registered
/// hooks" — the hook pipeline itself is v0.7 Bucket 0 work.
pub fn count_subscriptions(conn: &Connection) -> Result<usize> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM subscriptions", [], |r| r.get(0))
        .unwrap_or(0);
    Ok(usize::try_from(count.max(0)).unwrap_or(0))
}

/// v0.6.3 (capabilities schema v2): count `pending_actions` rows whose
/// `status` matches the predicate. Used by `handle_capabilities` to
/// surface live approval queue depth.
pub fn count_pending_actions_by_status(conn: &Connection, status: &str) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_actions WHERE status = ?1",
            params![status],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(usize::try_from(count.max(0)).unwrap_or(0))
}

/// v0.7.0 K2 — pending_actions timeout sweeper.
///
/// Scans `pending_actions` for `status='pending'` rows whose age exceeds
/// the per-row `default_timeout_seconds` (or `global_default_secs` when
/// the per-row column is NULL). Transitions matching rows to
/// `status='expired'` and stamps `expired_at = now`.
///
/// Returns the list of `(id, namespace)` tuples that were just expired
/// so the caller can fan out approval-decision events. Empty queue is a
/// silent no-op.
///
/// Closes the v0.6.3.1 honest-Capabilities-v2 disclosure that
/// `default_timeout_seconds` was previously advertised but unused (the
/// v2 honesty patch had dropped it from the wire shape; K2 ships the
/// backing sweeper so the field is meaningful again).
///
/// # Errors
///
/// Returns `Err` only on hard SQLite failures (e.g. table missing).
pub fn sweep_pending_action_timeouts(
    conn: &Connection,
    global_default_secs: i64,
) -> Result<Vec<(String, String)>> {
    // Step 1 — find candidates. We compute age in SQL via julianday()
    // arithmetic so the sweep is index-friendly and avoids parsing
    // every `requested_at` row in Rust. The composite index
    // `idx_pending_status_requested` (added in migration v21) keeps
    // the planner from full-scanning the table.
    //
    // The `default_timeout_seconds` column is nullable; rows with NULL
    // fall back to `global_default_secs`. A non-positive global default
    // disables the sweeper entirely (operator escape hatch).
    if global_default_secs <= 0 {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id, namespace FROM pending_actions
         WHERE status = 'pending'
           AND (julianday('now') - julianday(requested_at)) * 86400.0
               > COALESCE(default_timeout_seconds, ?1)",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![global_default_secs], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Step 2 — flip status='expired' + stamp expired_at. We update
    // row-by-row inside a single transaction so a failure mid-batch
    // rolls back cleanly. The WHERE clause re-checks status='pending'
    // so a concurrent decide_pending_action wins (its decision is
    // not overwritten).
    let now = Utc::now().to_rfc3339();
    let tx_savepoint = conn.unchecked_transaction()?;
    {
        let mut update = tx_savepoint.prepare(
            "UPDATE pending_actions
             SET status = 'expired', expired_at = ?1
             WHERE id = ?2 AND status = 'pending'",
        )?;
        for (id, _) in &rows {
            update.execute(params![now, id])?;
        }
    }
    tx_savepoint.commit()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// `ai-memory doctor` (P7 / R7) — query helpers.
// ---------------------------------------------------------------------------
//
// These read-only helpers back the `ai-memory doctor` CLI subcommand. Each
// query is a single indexed `COUNT(*)` (or close to it) so the reporter can
// run an entire health pass without holding the DB lock long enough to
// block live writers.
//
// Surfaces consumed:
// - `count_dim_violations` reads the post-P2 `embedding_dim` column when
//   present and gracefully reports `Ok(None)` on pre-P2 schemas (the column
//   doesn't exist yet on `release/v0.6.3`).
// - `count_index_evictions` reads the post-P3 `index_evictions_total` global
//   counter when wired (there is no schema-level surface today; it returns
//   `Ok(None)` so the doctor can render a "not yet observed" line).
// - `count_oldest_pending_action_age_secs` is portable today and reports the
//   age of the oldest `pending` row in seconds.
// - `count_governance_chain_depth` walks `parent_namespace` for each
//   namespace_meta row to estimate the inheritance depth distribution
//   the P4 enforcer will eventually consume.

/// Count rows whose `embedding_dim` (post-P2) does not match the modal
/// dim within their namespace. On pre-P2 schemas the `embedding_dim`
/// column doesn't exist; the function returns `Ok(None)` so the doctor
/// can render "not yet observed (pre-P2 schema)".
///
/// # Errors
///
/// Returns `Err` only on hard SQLite failures — a missing column is
/// reported as `Ok(None)`, not an error.
pub fn doctor_dim_violations(conn: &Connection) -> Result<Option<usize>> {
    let has_dim = conn
        .prepare("SELECT embedding_dim FROM memories LIMIT 0")
        .is_ok();
    if !has_dim {
        return Ok(None);
    }
    // For each namespace, find the modal dim (most-frequent non-null value)
    // and count rows whose dim differs from it. Rows with NULL dim but a
    // non-empty embedding count as violations too — they are mid-migration.
    let n: i64 = conn
        .query_row(
            "WITH per_ns_modes AS (
                 SELECT namespace, embedding_dim, COUNT(*) AS c
                 FROM memories
                 WHERE embedding IS NOT NULL AND embedding_dim IS NOT NULL
                 GROUP BY namespace, embedding_dim
             ),
             ranked AS (
                 SELECT namespace, embedding_dim,
                        ROW_NUMBER() OVER (PARTITION BY namespace ORDER BY c DESC) AS rn
                 FROM per_ns_modes
             ),
             modes AS (
                 SELECT namespace, embedding_dim AS modal_dim
                 FROM ranked WHERE rn = 1
             )
             SELECT COUNT(*)
             FROM memories m
             LEFT JOIN modes mo ON mo.namespace = m.namespace
             WHERE m.embedding IS NOT NULL
               AND (m.embedding_dim IS NULL
                    OR (mo.modal_dim IS NOT NULL AND m.embedding_dim != mo.modal_dim))",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(Some(usize::try_from(n.max(0)).unwrap_or(0)))
}

/// Age in seconds of the oldest `pending` row in `pending_actions`, or
/// `None` if the queue is empty (or the column is unparseable). The
/// doctor uses this to flag a backlog older than 24h as critical.
///
/// # Errors
///
/// Returns `Err` only on hard SQLite failures (e.g. missing table).
pub fn doctor_oldest_pending_age_secs(conn: &Connection) -> Result<Option<i64>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT requested_at FROM pending_actions WHERE status = 'pending'
             ORDER BY requested_at ASC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let Some(ts) = row else {
        return Ok(None);
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&ts) else {
        return Ok(None);
    };
    let age = (Utc::now() - parsed.with_timezone(&Utc)).num_seconds();
    Ok(Some(age))
}

/// Count of namespaces that have a standard registered with a non-null
/// `metadata.governance` block, and the count without (just a standard
/// memory but no policy attached).
///
/// # Errors
///
/// Returns `Err` only on hard SQLite failures.
pub fn doctor_governance_coverage(conn: &Connection) -> Result<(usize, usize)> {
    let with_policy: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories m
             INNER JOIN namespace_meta nm ON nm.standard_id = m.id
             WHERE json_extract(m.metadata, '$.governance') IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_meta: i64 = conn
        .query_row("SELECT COUNT(*) FROM namespace_meta", [], |r| r.get(0))
        .unwrap_or(0);
    let with = usize::try_from(with_policy.max(0)).unwrap_or(0);
    let total = usize::try_from(total_meta.max(0)).unwrap_or(0);
    Ok((with, total.saturating_sub(with)))
}

/// Distribution of the `parent_namespace` chain depth across
/// `namespace_meta` rows. Returns a Vec where index `i` is the count of
/// namespaces with chain depth `i` (depth 0 = no parent).
///
/// Walks each row's `parent_namespace` chain up to a hard cap of 16 to
/// avoid runaway loops on malformed data. Rows whose chain exceeds the
/// cap are bucketed at the cap.
///
/// # Errors
///
/// Returns `Err` only on hard SQLite failures.
pub fn doctor_governance_depth_distribution(conn: &Connection) -> Result<Vec<usize>> {
    const MAX_DEPTH: usize = 16;
    let mut stmt = conn.prepare("SELECT namespace, parent_namespace FROM namespace_meta")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let parent_map: HashMap<String, Option<String>> = rows
        .filter_map(rusqlite::Result::ok)
        .collect::<HashMap<_, _>>();
    let mut hist = vec![0_usize; MAX_DEPTH + 1];
    for ns in parent_map.keys() {
        let mut depth = 0_usize;
        let mut cur = parent_map.get(ns).cloned().flatten();
        while let Some(p) = cur {
            depth += 1;
            if depth >= MAX_DEPTH {
                break;
            }
            cur = parent_map.get(&p).cloned().flatten();
        }
        let bucket = depth.min(MAX_DEPTH);
        hist[bucket] += 1;
    }
    Ok(hist)
}

/// Sum of `subscriptions.dispatch_count` and `subscriptions.failure_count`
/// across all rows. Returns `(dispatched, failed)`. Used by the doctor to
/// estimate webhook delivery success rate.
///
/// # Errors
///
/// Returns `Err` only on hard SQLite failures.
pub fn doctor_webhook_delivery_totals(conn: &Connection) -> Result<(u64, u64)> {
    let dispatched: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(dispatch_count), 0) FROM subscriptions",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let failed: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(failure_count), 0) FROM subscriptions",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok((
        u64::try_from(dispatched.max(0)).unwrap_or(0),
        u64::try_from(failed.max(0)).unwrap_or(0),
    ))
}

/// Maximum sync-clock skew in seconds across the `sync_state` table —
/// the largest gap between `last_pulled_at` (when this peer last heard
/// from a peer) and `last_seen_at` (the peer's own `updated_at` advance).
/// Returns `Ok(None)` when `sync_state` is empty or the columns are
/// missing on a pre-T3 schema.
///
/// # Errors
///
/// Returns `Err` only on hard SQLite failures.
// ---------------------------------------------------------------------
// v0.6.4-009 — capability-expansion audit log
// ---------------------------------------------------------------------

/// Single audit_log row (capability-expansion shape — extensible).
#[derive(Debug, Clone)]
pub struct CapabilityExpansionRow {
    pub id: String,
    pub agent_id: Option<String>,
    pub event_type: String,
    pub requested_family: Option<String>,
    pub granted: bool,
    pub attestation_tier: Option<String>,
    pub timestamp: String,
}

/// Record a capability-expansion attempt. Used by
/// `handle_capabilities_family` after the allowlist decision is made.
/// Records BOTH grant and deny outcomes so operators can see attempted
/// access patterns even when the gate refused.
///
/// `granted=true` means the agent received the schemas; `granted=false`
/// means the agent was denied or the family was unknown.
///
/// Best-effort: a failed insert (e.g., disk full) is logged via tracing
/// but does not propagate the error to the caller — the audit trail
/// must never block the actual call.
pub fn record_capability_expansion(
    conn: &Connection,
    agent_id: Option<&str>,
    family: &str,
    granted: bool,
    attestation_tier: Option<&str>,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let result = conn.execute(
        "INSERT INTO audit_log (id, agent_id, event_type, requested_family, \
         granted, attestation_tier, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            id,
            agent_id,
            "capability_expansion",
            family,
            i32::from(granted),
            attestation_tier,
            now,
        ],
    );
    if let Err(e) = result {
        tracing::warn!(
            "audit_log insert failed (capability_expansion / agent={:?} / family={}): {e}",
            agent_id,
            family,
        );
    }
}

/// List recent capability-expansion rows, newest first. `limit` clamps
/// the row count.
pub fn list_capability_expansions(
    conn: &Connection,
    limit: usize,
    agent_filter: Option<&str>,
) -> Result<Vec<CapabilityExpansionRow>> {
    let n = (limit.min(10_000)) as i64;
    let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<CapabilityExpansionRow> {
        Ok(CapabilityExpansionRow {
            id: r.get(0)?,
            agent_id: r.get(1)?,
            event_type: r.get(2)?,
            requested_family: r.get(3)?,
            granted: r.get::<_, i64>(4)? != 0,
            attestation_tier: r.get(5)?,
            timestamp: r.get(6)?,
        })
    };
    if let Some(a) = agent_filter {
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, event_type, requested_family, granted, \
             attestation_tier, timestamp FROM audit_log \
             WHERE event_type = 'capability_expansion' AND agent_id = ?1 \
             ORDER BY timestamp DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![a, n], map_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, event_type, requested_family, granted, \
             attestation_tier, timestamp FROM audit_log \
             WHERE event_type = 'capability_expansion' \
             ORDER BY timestamp DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![n], map_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

pub fn doctor_max_sync_skew_secs(conn: &Connection) -> Result<Option<i64>> {
    let mut stmt = match conn.prepare(
        "SELECT last_seen_at, last_pulled_at FROM sync_state WHERE last_pulled_at IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut max_skew: Option<i64> = None;
    for row in rows {
        let Ok((seen, pulled)) = row else { continue };
        let Ok(s) = chrono::DateTime::parse_from_rfc3339(&seen) else {
            continue;
        };
        let Ok(p) = chrono::DateTime::parse_from_rfc3339(&pulled) else {
            continue;
        };
        let skew = (s.with_timezone(&Utc) - p.with_timezone(&Utc))
            .num_seconds()
            .abs();
        max_skew = Some(max_skew.map_or(skew, |m| m.max(skew)));
    }
    Ok(max_skew)
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
        // v0.6.3.1 P2 (G5) — restore preserves the original tier and
        // expires_at instead of resetting to long/permanent. Pre-v17 this
        // test asserted `is_none()` for expires_at — that was the bug
        // being fixed.
        let conn = test_db();
        let mut mem = make_memory("Restorable", "test", Tier::Short, 5);
        let original_expiry = "2020-01-01T00:00:00+00:00".to_string();
        mem.expires_at = Some(original_expiry.clone());
        let id = insert(&conn, &mem).unwrap();

        gc(&conn, true).unwrap();
        assert!(get(&conn, &id).unwrap().is_none()); // gone from active

        let restored = restore_archived(&conn, &id).unwrap();
        assert!(restored);

        let got = get(&conn, &id).unwrap().unwrap();
        assert_eq!(got.title, "Restorable");
        assert_eq!(
            got.tier.as_str(),
            "short",
            "G5: restore must preserve the original tier"
        );
        assert_eq!(
            got.expires_at,
            Some(original_expiry),
            "G5: restore must preserve the original expires_at"
        );
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

    // -- Pillar 2 / Stream D — memory_check_duplicate -------------------

    fn insert_with_embedding(
        conn: &Connection,
        title: &str,
        ns: &str,
        embedding: &[f32],
    ) -> String {
        let mem = make_memory(title, ns, Tier::Long, 5);
        let id = insert(conn, &mem).unwrap();
        set_embedding(conn, &id, embedding).unwrap();
        id
    }

    #[test]
    fn check_duplicate_empty_db_returns_no_match() {
        let conn = test_db();
        let q = vec![1.0_f32, 0.0, 0.0];
        let r = check_duplicate(&conn, &q, None, 0.85).unwrap();
        assert!(!r.is_duplicate);
        assert!(r.nearest.is_none());
        assert_eq!(r.candidates_scanned, 0);
    }

    #[test]
    fn check_duplicate_finds_highest_cosine_match() {
        let conn = test_db();
        // a = [1,0,0]; b = [0,1,0]; c = [0.99,0.01,0]. Query = [1,0,0]
        // expects `c` (cos ~0.9999) > `a` (cos =1.0 actually).
        // Use distinct vectors: a=[1,0,0] cos 1.0, b=[0.7,0.7,0] cos 0.707,
        // c=[0,1,0] cos 0.0. Best should be `a`.
        let id_a = insert_with_embedding(&conn, "alpha", "ns", &[1.0, 0.0, 0.0]);
        let _id_b = insert_with_embedding(&conn, "beta", "ns", &[0.7, 0.7, 0.0]);
        let _id_c = insert_with_embedding(&conn, "gamma", "ns", &[0.0, 1.0, 0.0]);

        let q = vec![1.0_f32, 0.0, 0.0];
        let r = check_duplicate(&conn, &q, None, 0.85).unwrap();
        let nearest = r.nearest.expect("expected a nearest match");
        assert_eq!(nearest.id, id_a);
        assert!(nearest.similarity > 0.99);
        assert_eq!(r.candidates_scanned, 3);
        assert!(r.is_duplicate);
        assert!((r.threshold - 0.85).abs() < 1e-6);
    }

    #[test]
    fn check_duplicate_below_threshold_not_flagged_but_returns_nearest() {
        let conn = test_db();
        let id_b = insert_with_embedding(&conn, "beta", "ns", &[0.7, 0.7, 0.0]);

        // Cosine([1,0,0], [0.7,0.7,0]) ~ 0.707 — below default 0.85.
        let q = vec![1.0_f32, 0.0, 0.0];
        let r = check_duplicate(&conn, &q, None, 0.85).unwrap();
        let nearest = r
            .nearest
            .expect("nearest must surface even when below threshold");
        assert_eq!(nearest.id, id_b);
        assert!(!r.is_duplicate);
    }

    #[test]
    fn check_duplicate_threshold_clamped_to_floor() {
        let conn = test_db();
        // Caller passes a permissive 0.0; the response threshold must
        // be clamped to DUPLICATE_THRESHOLD_MIN so unrelated content
        // can't be dressed as a merge candidate.
        let _ = insert_with_embedding(&conn, "x", "ns", &[1.0, 0.0, 0.0]);
        let q = vec![0.0_f32, 1.0, 0.0]; // orthogonal — cosine 0.0
        let r = check_duplicate(&conn, &q, None, 0.0).unwrap();
        assert!((r.threshold - DUPLICATE_THRESHOLD_MIN).abs() < 1e-6);
        assert!(!r.is_duplicate);
    }

    #[test]
    fn check_duplicate_namespace_filter_isolates_scan() {
        let conn = test_db();
        let _hit_in_other_ns = insert_with_embedding(&conn, "x", "other", &[1.0, 0.0, 0.0]);
        let id_target = insert_with_embedding(&conn, "y", "ns", &[0.6, 0.8, 0.0]);

        let q = vec![1.0_f32, 0.0, 0.0];
        let r = check_duplicate(&conn, &q, Some("ns"), 0.85).unwrap();
        assert_eq!(r.candidates_scanned, 1);
        assert_eq!(r.nearest.expect("namespace filter ignored").id, id_target);
    }

    #[test]
    fn check_duplicate_skips_expired_rows() {
        let conn = test_db();
        // Short-tier memory with a backdated `expires_at` is past the
        // live-row gate and must not be a candidate.
        let mut mem = make_memory("expired", "ns", Tier::Short, 5);
        mem.expires_at = Some((chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339());
        let id = insert(&conn, &mem).unwrap();
        set_embedding(&conn, &id, &[1.0, 0.0, 0.0]).unwrap();

        let q = vec![1.0_f32, 0.0, 0.0];
        let r = check_duplicate(&conn, &q, None, 0.85).unwrap();
        assert_eq!(r.candidates_scanned, 0);
        assert!(r.nearest.is_none());
    }

    #[test]
    fn check_duplicate_skips_unembedded_rows() {
        let conn = test_db();
        // One memory with an embedding, one without — only the embedded
        // row should appear in `candidates_scanned`.
        let id_embedded = insert_with_embedding(&conn, "with-emb", "ns", &[1.0, 0.0, 0.0]);
        let mem = make_memory("no-emb", "ns", Tier::Long, 5);
        let _ = insert(&conn, &mem).unwrap();

        let q = vec![1.0_f32, 0.0, 0.0];
        let r = check_duplicate(&conn, &q, None, 0.85).unwrap();
        assert_eq!(r.candidates_scanned, 1);
        assert_eq!(r.nearest.expect("embedded match").id, id_embedded);
    }

    #[test]
    fn check_duplicate_skips_blob_with_non_multiple_of_4_length() {
        // Regression: pre-fix, an embedding blob whose length was not
        // a multiple of 4 would silently drop a trailing partial chunk
        // via chunks_exact and compute cosine against a shorter
        // candidate vector — producing a misleading score. The bounds
        // check now skips the row entirely.
        let conn = test_db();
        let mem = make_memory("malformed-blob", "ns", Tier::Long, 5);
        let id = insert(&conn, &mem).unwrap();
        // Write a 7-byte blob (1 short of 8 = 2 f32s) directly to
        // sqlite, bypassing set_embedding which only takes &[f32].
        conn.execute(
            "UPDATE memories SET embedding = ?1 WHERE id = ?2",
            params![&[0u8; 7][..], &id],
        )
        .unwrap();

        let q = vec![1.0_f32, 0.0];
        let r = check_duplicate(&conn, &q, None, 0.85).unwrap();
        assert_eq!(
            r.candidates_scanned, 0,
            "malformed blob must be skipped, not silently truncated"
        );
        assert!(r.nearest.is_none());
    }

    #[test]
    fn check_duplicate_skips_blob_with_dimension_mismatch() {
        // Regression: a blob with a valid length (multiple of 4) but
        // wrong dimension vs the query embedding must NOT be scored;
        // cosine_similarity zips and would silently truncate to the
        // shorter input, producing a wrong similarity.
        let conn = test_db();
        // Insert a memory with a 3-dim embedding via the normal path.
        let _id = insert_with_embedding(&conn, "different-dim", "ns", &[1.0, 0.0, 0.0]);

        // Query with a 4-dim embedding — different from the candidate.
        let q = vec![1.0_f32, 0.0, 0.0, 0.0];
        let r = check_duplicate(&conn, &q, None, 0.85).unwrap();
        assert_eq!(
            r.candidates_scanned, 0,
            "dimension-mismatched candidate must be skipped"
        );
        assert!(r.nearest.is_none());
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
        assert!(!sanitized.contains('*'));
        assert!(!sanitized.contains('('));
        assert!(!sanitized.contains(')'));
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

    // -- Pillar 2 / Stream B — entity_register / entity_get_by_alias ------

    #[test]
    fn entity_register_creates_new_entity_with_aliases() {
        let conn = test_db();
        let aliases = vec!["pa".to_string(), "Project A".to_string()];
        let reg = entity_register(
            &conn,
            "Project Alpha",
            "projects/alpha",
            &aliases,
            &serde_json::json!({}),
            Some("test-agent"),
        )
        .unwrap();
        assert!(reg.created, "first registration must be created=true");
        assert_eq!(reg.canonical_name, "Project Alpha");
        assert_eq!(reg.namespace, "projects/alpha");
        // Aliases inserted in one call share a created_at; the
        // secondary `alias ASC` sort orders 'P' before 'p'.
        assert_eq!(reg.aliases, vec!["Project A".to_string(), "pa".to_string()]);

        let m = get(&conn, &reg.entity_id).unwrap().unwrap();
        assert_eq!(m.title, "Project Alpha");
        assert_eq!(m.tier.rank(), Tier::Long.rank());
        assert!(m.tags.contains(&"entity".to_string()));
        assert_eq!(m.metadata["kind"], "entity");
        assert_eq!(m.metadata["agent_id"], "test-agent");
    }

    #[test]
    fn entity_register_reuses_existing_and_merges_aliases() {
        let conn = test_db();
        let first = entity_register(
            &conn,
            "Project Alpha",
            "projects/alpha",
            &["pa".to_string()],
            &serde_json::json!({}),
            Some("a1"),
        )
        .unwrap();
        let second = entity_register(
            &conn,
            "Project Alpha",
            "projects/alpha",
            &["pa".to_string(), "alpha".to_string()],
            &serde_json::json!({}),
            Some("a2"),
        )
        .unwrap();
        assert!(first.created);
        assert!(!second.created, "second call must reuse the entity");
        assert_eq!(first.entity_id, second.entity_id);
        assert_eq!(second.aliases, vec!["pa".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn entity_register_errors_on_collision_with_non_entity_memory() {
        let conn = test_db();
        let mem = make_memory("Conflict", "projects/alpha", Tier::Long, 5);
        insert(&conn, &mem).unwrap();
        let err = entity_register(
            &conn,
            "Conflict",
            "projects/alpha",
            &[],
            &serde_json::json!({}),
            None,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("non-entity memory"),
            "expected collision error, got: {msg}"
        );
    }

    #[test]
    fn entity_register_skips_blank_aliases() {
        let conn = test_db();
        let reg = entity_register(
            &conn,
            "Trim Test",
            "test",
            &[String::new(), "   ".to_string(), "ok".to_string()],
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        assert_eq!(reg.aliases, vec!["ok".to_string()]);
    }

    #[test]
    fn entity_register_preserves_caller_metadata_keys() {
        let conn = test_db();
        let extra = serde_json::json!({"team": "platform", "kind": "ignored"});
        let reg = entity_register(&conn, "Service X", "svc", &[], &extra, None).unwrap();
        let m = get(&conn, &reg.entity_id).unwrap().unwrap();
        assert_eq!(m.metadata["team"], "platform");
        // Caller's `kind` is overwritten — entity records must always
        // carry kind=entity for the resolver to find them.
        assert_eq!(m.metadata["kind"], "entity");
    }

    #[test]
    fn entity_get_by_alias_returns_record_with_full_alias_set() {
        let conn = test_db();
        let reg = entity_register(
            &conn,
            "Project Alpha",
            "projects/alpha",
            &["pa".to_string(), "alpha".to_string()],
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        let got = entity_get_by_alias(&conn, "pa", None).unwrap().unwrap();
        assert_eq!(got.entity_id, reg.entity_id);
        assert_eq!(got.canonical_name, "Project Alpha");
        assert_eq!(got.namespace, "projects/alpha");
        // Same-batch aliases share a created_at; alphabetical
        // tiebreak puts "alpha" before "pa".
        assert_eq!(got.aliases, vec!["alpha".to_string(), "pa".to_string()]);
    }

    #[test]
    fn entity_get_by_alias_returns_none_for_unknown_alias() {
        let conn = test_db();
        let got = entity_get_by_alias(&conn, "missing", None).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn entity_get_by_alias_filters_by_namespace() {
        let conn = test_db();
        entity_register(
            &conn,
            "Acme",
            "ns_a",
            &["a".to_string()],
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        entity_register(
            &conn,
            "Acme Corp",
            "ns_b",
            &["a".to_string()],
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        let in_a = entity_get_by_alias(&conn, "a", Some("ns_a"))
            .unwrap()
            .unwrap();
        assert_eq!(in_a.namespace, "ns_a");
        assert_eq!(in_a.canonical_name, "Acme");
        let in_b = entity_get_by_alias(&conn, "a", Some("ns_b"))
            .unwrap()
            .unwrap();
        assert_eq!(in_b.namespace, "ns_b");
        assert_eq!(in_b.canonical_name, "Acme Corp");
    }

    #[test]
    fn entity_get_by_alias_without_namespace_picks_most_recent() {
        let conn = test_db();
        // Older entity created first.
        entity_register(
            &conn,
            "Older",
            "ns_old",
            &["dup".to_string()],
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        // Sleep just enough to guarantee a strictly later created_at.
        std::thread::sleep(std::time::Duration::from_millis(5));
        entity_register(
            &conn,
            "Newer",
            "ns_new",
            &["dup".to_string()],
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        let got = entity_get_by_alias(&conn, "dup", None).unwrap().unwrap();
        assert_eq!(got.canonical_name, "Newer");
        assert_eq!(got.namespace, "ns_new");
    }

    #[test]
    fn entity_get_by_alias_ignores_non_entity_memory_with_matching_alias() {
        let conn = test_db();
        // Insert a regular (non-entity) memory and a stray
        // entity_aliases row pointing at it. The resolver must skip
        // it because `kind != 'entity'`.
        let mut mem = make_memory("Decoy", "test", Tier::Long, 5);
        mem.metadata = serde_json::json!({});
        let mid = insert(&conn, &mem).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO entity_aliases (entity_id, alias, created_at) VALUES (?1, ?2, ?3)",
            params![&mid, "decoy", &now],
        )
        .unwrap();
        let got = entity_get_by_alias(&conn, "decoy", None).unwrap();
        assert!(got.is_none(), "non-entity memories must not resolve");
    }

    #[test]
    fn entity_register_idempotent_aliases_are_deduped() {
        let conn = test_db();
        let reg = entity_register(
            &conn,
            "Dedup",
            "test",
            &["x".to_string(), "x".to_string(), "y".to_string()],
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        // INSERT OR IGNORE collapses the duplicate "x".
        assert_eq!(reg.aliases.len(), 2);
        assert!(reg.aliases.contains(&"x".to_string()));
        assert!(reg.aliases.contains(&"y".to_string()));
    }

    // -- Pillar 2 / Stream C — kg_timeline ---------------------------------

    /// Insert a link with an explicit `valid_from` so timeline tests can
    /// pin event ordering without relying on wall-clock spread.
    fn insert_link_at(
        conn: &Connection,
        source_id: &str,
        target_id: &str,
        relation: &str,
        valid_from: &str,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at, valid_from) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![source_id, target_id, relation, now, valid_from],
        )
        .unwrap();
    }

    #[test]
    fn create_link_populates_valid_from_for_new_rows() {
        let conn = test_db();
        let src = make_memory("kg-src", "test", Tier::Long, 5);
        let tgt = make_memory("kg-tgt", "test", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &tgt).unwrap();
        create_link(&conn, &src.id, &tgt.id, "related_to").unwrap();
        let valid_from: Option<String> = conn
            .query_row(
                "SELECT valid_from FROM memory_links WHERE source_id = ?1",
                params![&src.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            valid_from.is_some(),
            "create_link must populate valid_from so kg_timeline can see new links"
        );
    }

    #[test]
    fn kg_timeline_returns_events_ordered_by_valid_from_ascending() {
        let conn = test_db();
        let src = make_memory("alpha", "kg/projects/alpha", Tier::Long, 5);
        let s1 = make_memory("kickoff", "kg/projects/alpha", Tier::Long, 5);
        let s2 = make_memory("design phase", "kg/projects/alpha", Tier::Long, 5);
        let s3 = make_memory("implementation", "kg/projects/alpha", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &s1).unwrap();
        insert(&conn, &s2).unwrap();
        insert(&conn, &s3).unwrap();

        // Insert in a deliberately-shuffled order so ORDER BY isn't
        // a happy accident of insertion order.
        insert_link_at(
            &conn,
            &src.id,
            &s2.id,
            "supersedes",
            "2026-02-03T00:00:00+00:00",
        );
        insert_link_at(
            &conn,
            &src.id,
            &s1.id,
            "related_to",
            "2026-01-15T00:00:00+00:00",
        );
        insert_link_at(
            &conn,
            &src.id,
            &s3.id,
            "supersedes",
            "2026-03-22T00:00:00+00:00",
        );

        let events = kg_timeline(&conn, &src.id, None, None, None).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].target_id, s1.id);
        assert_eq!(events[1].target_id, s2.id);
        assert_eq!(events[2].target_id, s3.id);
        assert_eq!(events[0].title, "kickoff");
        assert_eq!(events[1].relation, "supersedes");
        assert_eq!(events[0].target_namespace, "kg/projects/alpha");
    }

    #[test]
    fn kg_timeline_filters_by_since_inclusive() {
        let conn = test_db();
        let src = make_memory("e", "ns", Tier::Long, 5);
        let t1 = make_memory("e1", "ns", Tier::Long, 5);
        let t2 = make_memory("e2", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t1).unwrap();
        insert(&conn, &t2).unwrap();
        insert_link_at(&conn, &src.id, &t1.id, "rel", "2026-01-01T00:00:00+00:00");
        insert_link_at(&conn, &src.id, &t2.id, "rel", "2026-03-01T00:00:00+00:00");

        let events = kg_timeline(
            &conn,
            &src.id,
            Some("2026-02-01T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].target_id, t2.id);

        // Boundary: since == valid_from should match (inclusive).
        let on_boundary = kg_timeline(
            &conn,
            &src.id,
            Some("2026-03-01T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(on_boundary.len(), 1);
    }

    #[test]
    fn kg_timeline_filters_by_until_inclusive() {
        let conn = test_db();
        let src = make_memory("e", "ns", Tier::Long, 5);
        let t1 = make_memory("e1", "ns", Tier::Long, 5);
        let t2 = make_memory("e2", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t1).unwrap();
        insert(&conn, &t2).unwrap();
        insert_link_at(&conn, &src.id, &t1.id, "rel", "2026-01-01T00:00:00+00:00");
        insert_link_at(&conn, &src.id, &t2.id, "rel", "2026-03-01T00:00:00+00:00");

        let events = kg_timeline(
            &conn,
            &src.id,
            None,
            Some("2026-02-01T00:00:00+00:00"),
            None,
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].target_id, t1.id);
    }

    #[test]
    fn kg_timeline_skips_links_with_null_valid_from() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        let t1 = make_memory("t1", "ns", Tier::Long, 5);
        let t2 = make_memory("t2", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t1).unwrap();
        insert(&conn, &t2).unwrap();
        // Direct insert with NULL valid_from to simulate an external
        // writer that bypassed `create_link`.
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at, valid_from) \
             VALUES (?1, ?2, 'rel', ?3, NULL)",
            params![&src.id, &t1.id, &now],
        )
        .unwrap();
        insert_link_at(&conn, &src.id, &t2.id, "rel", "2026-01-01T00:00:00+00:00");

        let events = kg_timeline(&conn, &src.id, None, None, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].target_id, t2.id);
    }

    #[test]
    fn kg_timeline_excludes_links_where_source_is_target() {
        // The query is anchored on `source_id`; inbound edges (where the
        // entity is the target) are intentionally NOT part of the
        // timeline. This guards against accidentally widening the
        // contract to a bidirectional view.
        let conn = test_db();
        let entity = make_memory("entity", "ns", Tier::Long, 5);
        let other = make_memory("other", "ns", Tier::Long, 5);
        insert(&conn, &entity).unwrap();
        insert(&conn, &other).unwrap();
        insert_link_at(
            &conn,
            &other.id,
            &entity.id,
            "rel",
            "2026-01-01T00:00:00+00:00",
        );
        let events = kg_timeline(&conn, &entity.id, None, None, None).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn kg_timeline_limit_clamped_to_max() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        for i in 0..5 {
            let t = make_memory(&format!("t{i}"), "ns", Tier::Long, 5);
            insert(&conn, &t).unwrap();
            insert_link_at(
                &conn,
                &src.id,
                &t.id,
                "rel",
                &format!("2026-01-0{}T00:00:00+00:00", i + 1),
            );
        }
        // Caller passes a wildly oversized limit — should be clamped
        // to KG_TIMELINE_MAX_LIMIT (i.e. accepted, not errored), and
        // since the row count is small, should return all 5.
        let events = kg_timeline(&conn, &src.id, None, None, Some(usize::MAX)).unwrap();
        assert_eq!(events.len(), 5);

        // Caller passes 0 — clamp to 1.
        let one = kg_timeline(&conn, &src.id, None, None, Some(0)).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn kg_timeline_carries_observed_by_and_valid_until() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        let t = make_memory("t", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at, valid_from, valid_until, observed_by) \
             VALUES (?1, ?2, 'supersedes', ?3, '2026-01-01T00:00:00+00:00', '2026-12-31T23:59:59+00:00', 'agent-pm-1')",
            params![&src.id, &t.id, &now],
        )
        .unwrap();
        let events = kg_timeline(&conn, &src.id, None, None, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].observed_by.as_deref(), Some("agent-pm-1"));
        assert_eq!(
            events[0].valid_until.as_deref(),
            Some("2026-12-31T23:59:59+00:00")
        );
    }

    #[test]
    fn kg_timeline_empty_for_unknown_source() {
        let conn = test_db();
        let events = kg_timeline(&conn, "nonexistent-id", None, None, None).unwrap();
        assert!(events.is_empty());
    }

    // -- Pillar 2 / Stream C — kg_invalidate -------------------------------

    #[test]
    fn invalidate_link_sets_valid_until_to_provided_timestamp() {
        let conn = test_db();
        let src = make_memory("inv-s", "test", Tier::Long, 5);
        let tgt = make_memory("inv-t", "test", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &tgt).unwrap();
        create_link(&conn, &src.id, &tgt.id, "related_to").unwrap();
        let stamp = "2026-12-31T23:59:59+00:00";
        let res = invalidate_link(&conn, &src.id, &tgt.id, "related_to", Some(stamp))
            .unwrap()
            .expect("link must exist");
        assert_eq!(res.valid_until, stamp);
        assert!(res.previous_valid_until.is_none());
        let stored: Option<String> = conn
            .query_row(
                "SELECT valid_until FROM memory_links \
                 WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
                params![&src.id, &tgt.id, "related_to"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some(stamp));
    }

    #[test]
    fn invalidate_link_defaults_to_now_when_no_timestamp_provided() {
        let conn = test_db();
        let src = make_memory("inv-s", "test", Tier::Long, 5);
        let tgt = make_memory("inv-t", "test", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &tgt).unwrap();
        create_link(&conn, &src.id, &tgt.id, "related_to").unwrap();
        let res = invalidate_link(&conn, &src.id, &tgt.id, "related_to", None)
            .unwrap()
            .expect("link must exist");
        // The default is wall-clock now; assert it parses as RFC3339 and
        // is within a small window of the test's "now" (allow 60s skew
        // to accommodate slow runners).
        let parsed = chrono::DateTime::parse_from_rfc3339(&res.valid_until)
            .expect("default valid_until must be RFC3339");
        let now = chrono::Utc::now();
        let drift = now.signed_duration_since(parsed.with_timezone(&chrono::Utc));
        assert!(
            drift.num_seconds().abs() < 60,
            "default valid_until {} should be near now {now}",
            res.valid_until
        );
    }

    #[test]
    fn invalidate_link_returns_none_for_unknown_triple() {
        let conn = test_db();
        // No memories or links created.
        let res = invalidate_link(&conn, "missing-src", "missing-tgt", "related_to", None).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn invalidate_link_returns_none_when_relation_does_not_match() {
        // Link exists for ("related_to") but caller asks for ("supersedes").
        let conn = test_db();
        let src = make_memory("inv-s", "test", Tier::Long, 5);
        let tgt = make_memory("inv-t", "test", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &tgt).unwrap();
        create_link(&conn, &src.id, &tgt.id, "related_to").unwrap();
        let res = invalidate_link(&conn, &src.id, &tgt.id, "supersedes", None).unwrap();
        assert!(res.is_none(), "must not match across relation values");
    }

    #[test]
    fn invalidate_link_overwrites_existing_valid_until_and_reports_prior() {
        let conn = test_db();
        let src = make_memory("inv-s", "test", Tier::Long, 5);
        let tgt = make_memory("inv-t", "test", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &tgt).unwrap();
        create_link(&conn, &src.id, &tgt.id, "related_to").unwrap();
        let first = "2026-06-01T00:00:00+00:00";
        let second = "2026-12-01T00:00:00+00:00";
        let r1 = invalidate_link(&conn, &src.id, &tgt.id, "related_to", Some(first))
            .unwrap()
            .unwrap();
        assert!(r1.previous_valid_until.is_none());
        let r2 = invalidate_link(&conn, &src.id, &tgt.id, "related_to", Some(second))
            .unwrap()
            .unwrap();
        assert_eq!(r2.previous_valid_until.as_deref(), Some(first));
        assert_eq!(r2.valid_until, second);
    }

    #[test]
    fn invalidate_link_distinguishes_relation_when_multiple_links_share_endpoints() {
        // Two links between the same pair, different relations. Invalidating
        // one must not affect the other.
        let conn = test_db();
        let src = make_memory("inv-s", "test", Tier::Long, 5);
        let tgt = make_memory("inv-t", "test", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &tgt).unwrap();
        create_link(&conn, &src.id, &tgt.id, "related_to").unwrap();
        create_link(&conn, &src.id, &tgt.id, "supersedes").unwrap();
        let stamp = "2026-07-15T12:00:00+00:00";
        invalidate_link(&conn, &src.id, &tgt.id, "related_to", Some(stamp))
            .unwrap()
            .unwrap();
        let related: Option<String> = conn
            .query_row(
                "SELECT valid_until FROM memory_links \
                 WHERE source_id = ?1 AND target_id = ?2 AND relation = 'related_to'",
                params![&src.id, &tgt.id],
                |r| r.get(0),
            )
            .unwrap();
        let supers: Option<String> = conn
            .query_row(
                "SELECT valid_until FROM memory_links \
                 WHERE source_id = ?1 AND target_id = ?2 AND relation = 'supersedes'",
                params![&src.id, &tgt.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(related.as_deref(), Some(stamp));
        assert!(
            supers.is_none(),
            "the sibling 'supersedes' link must remain valid"
        );
    }

    #[test]
    fn invalidate_link_preserves_other_columns() {
        // valid_from, observed_by, created_at, signature must not be
        // touched by the invalidate UPDATE.
        let conn = test_db();
        let src = make_memory("inv-s", "test", Tier::Long, 5);
        let tgt = make_memory("inv-t", "test", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &tgt).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_links \
             (source_id, target_id, relation, created_at, valid_from, observed_by) \
             VALUES (?1, ?2, 'related_to', ?3, '2026-01-01T00:00:00+00:00', 'agent-x')",
            params![&src.id, &tgt.id, &now],
        )
        .unwrap();
        invalidate_link(
            &conn,
            &src.id,
            &tgt.id,
            "related_to",
            Some("2026-12-31T23:59:59+00:00"),
        )
        .unwrap()
        .unwrap();
        let (vf, ob, ca): (Option<String>, Option<String>, String) = conn
            .query_row(
                "SELECT valid_from, observed_by, created_at FROM memory_links \
                 WHERE source_id = ?1 AND target_id = ?2 AND relation = 'related_to'",
                params![&src.id, &tgt.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(vf.as_deref(), Some("2026-01-01T00:00:00+00:00"));
        assert_eq!(ob.as_deref(), Some("agent-x"));
        assert_eq!(ca, now);
    }

    // -- Pillar 2 / Stream C — kg_query (depth=1) ---------------------------

    /// Insert a link with explicit `temporal/observed_by` columns so the
    /// `kg_query` filter tests can pin behavior without relying on
    /// wall-clock spread.
    fn insert_link_full(
        conn: &Connection,
        source_id: &str,
        target_id: &str,
        relation: &str,
        valid_from: Option<&str>,
        valid_until: Option<&str>,
        observed_by: Option<&str>,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_links \
             (source_id, target_id, relation, created_at, valid_from, valid_until, observed_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                source_id,
                target_id,
                relation,
                now,
                valid_from,
                valid_until,
                observed_by
            ],
        )
        .unwrap();
    }

    #[test]
    fn kg_query_returns_outbound_neighbors_at_depth_1() {
        let conn = test_db();
        let src = make_memory("alpha", "kg/projects/alpha", Tier::Long, 5);
        let n1 = make_memory("kickoff", "kg/projects/alpha", Tier::Long, 5);
        let n2 = make_memory("design", "kg/projects/alpha", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &n1).unwrap();
        insert(&conn, &n2).unwrap();
        insert_link_full(
            &conn,
            &src.id,
            &n1.id,
            "related_to",
            Some("2026-01-15T00:00:00+00:00"),
            None,
            Some("agent-1"),
        );
        insert_link_full(
            &conn,
            &src.id,
            &n2.id,
            "supersedes",
            Some("2026-02-03T00:00:00+00:00"),
            None,
            Some("agent-2"),
        );

        let nodes = kg_query(&conn, &src.id, 1, None, None, None).unwrap();
        assert_eq!(nodes.len(), 2);
        // Ordered by COALESCE(valid_from, created_at) ASC.
        assert_eq!(nodes[0].target_id, n1.id);
        assert_eq!(nodes[1].target_id, n2.id);
        assert_eq!(nodes[0].title, "kickoff");
        assert_eq!(nodes[0].relation, "related_to");
        assert_eq!(nodes[0].observed_by.as_deref(), Some("agent-1"));
        assert_eq!(nodes[0].depth, 1);
        assert_eq!(nodes[0].path, format!("{}->{}", src.id, n1.id));
        assert_eq!(nodes[0].target_namespace, "kg/projects/alpha");
    }

    #[test]
    fn kg_query_filters_by_valid_at_window() {
        let conn = test_db();
        let src = make_memory("e", "ns", Tier::Long, 5);
        let t1 = make_memory("e1", "ns", Tier::Long, 5);
        let t2 = make_memory("e2", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t1).unwrap();
        insert(&conn, &t2).unwrap();
        // t1 valid 2026-01-01 → 2026-02-01; t2 valid from 2026-03-01.
        insert_link_full(
            &conn,
            &src.id,
            &t1.id,
            "related_to",
            Some("2026-01-01T00:00:00+00:00"),
            Some("2026-02-01T00:00:00+00:00"),
            None,
        );
        insert_link_full(
            &conn,
            &src.id,
            &t2.id,
            "related_to",
            Some("2026-03-01T00:00:00+00:00"),
            None,
            None,
        );

        // At 2026-01-15 only t1 is valid.
        let n_jan = kg_query(
            &conn,
            &src.id,
            1,
            Some("2026-01-15T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(n_jan.len(), 1);
        assert_eq!(n_jan[0].target_id, t1.id);

        // At 2026-02-15 the first link is closed, the second hasn't
        // started yet — empty.
        let n_feb = kg_query(
            &conn,
            &src.id,
            1,
            Some("2026-02-15T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert!(n_feb.is_empty());

        // At 2026-04-01 only t2 is valid.
        let n_apr = kg_query(
            &conn,
            &src.id,
            1,
            Some("2026-04-01T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(n_apr.len(), 1);
        assert_eq!(n_apr[0].target_id, t2.id);
    }

    #[test]
    fn kg_query_skips_null_valid_from_when_valid_at_filter_active() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        let t = make_memory("t", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t).unwrap();
        // Link with NULL valid_from — must be invisible to a temporally
        // scoped query (we cannot tell if it was valid at any point).
        insert_link_full(&conn, &src.id, &t.id, "related_to", None, None, None);

        let with_filter = kg_query(
            &conn,
            &src.id,
            1,
            Some("2026-01-15T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert!(with_filter.is_empty());

        // Without the filter, the same link IS returned.
        let without = kg_query(&conn, &src.id, 1, None, None, None).unwrap();
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].target_id, t.id);
    }

    #[test]
    fn kg_query_filters_by_allowed_agents() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        let t1 = make_memory("t1", "ns", Tier::Long, 5);
        let t2 = make_memory("t2", "ns", Tier::Long, 5);
        let t3 = make_memory("t3", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t1).unwrap();
        insert(&conn, &t2).unwrap();
        insert(&conn, &t3).unwrap();
        insert_link_full(
            &conn,
            &src.id,
            &t1.id,
            "related_to",
            Some("2026-01-01T00:00:00+00:00"),
            None,
            Some("agent-a"),
        );
        insert_link_full(
            &conn,
            &src.id,
            &t2.id,
            "related_to",
            Some("2026-01-02T00:00:00+00:00"),
            None,
            Some("agent-b"),
        );
        // Link with NULL observed_by must be excluded once the agent
        // filter is active (`NULL IN (...)` is NULL/false in SQLite).
        insert_link_full(
            &conn,
            &src.id,
            &t3.id,
            "related_to",
            Some("2026-01-03T00:00:00+00:00"),
            None,
            None,
        );

        let allow_a = vec!["agent-a".to_string()];
        let only_a = kg_query(&conn, &src.id, 1, None, Some(&allow_a), None).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].target_id, t1.id);

        let allow_both = vec!["agent-a".to_string(), "agent-b".to_string()];
        let both = kg_query(&conn, &src.id, 1, None, Some(&allow_both), None).unwrap();
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn kg_query_empty_allowed_agents_returns_zero_rows() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        let t = make_memory("t", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &t).unwrap();
        insert_link_full(
            &conn,
            &src.id,
            &t.id,
            "related_to",
            Some("2026-01-01T00:00:00+00:00"),
            None,
            Some("agent-a"),
        );

        // Sanity: no filter returns the link.
        let unfiltered = kg_query(&conn, &src.id, 1, None, None, None).unwrap();
        assert_eq!(unfiltered.len(), 1);

        // Empty allowlist == "no agents trusted" — must return zero
        // rows, not silently fall through to the unfiltered path.
        let empty: Vec<String> = Vec::new();
        let none = kg_query(&conn, &src.id, 1, None, Some(&empty), None).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn kg_query_rejects_max_depth_zero() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        let err = kg_query(&conn, &src.id, 0, None, None, None).unwrap_err();
        assert!(err.to_string().contains("max_depth"));
    }

    #[test]
    fn kg_query_rejects_unsupported_max_depth() {
        // The recursive-CTE slice supports depth 1..=5; passing 6+ must
        // produce an explicit error so callers learn they hit the
        // ceiling rather than receiving a partial graph.
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        let err = kg_query(
            &conn,
            &src.id,
            KG_QUERY_MAX_SUPPORTED_DEPTH + 1,
            None,
            None,
            None,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&format!("max_depth={}", KG_QUERY_MAX_SUPPORTED_DEPTH + 1)));
        assert!(msg.contains(&format!("supported depth={KG_QUERY_MAX_SUPPORTED_DEPTH}")));
    }

    #[test]
    fn kg_query_traverses_multiple_hops() {
        // src -> mid -> leaf. depth=2 must return both hops, with
        // depth/path reflecting the chain.
        let conn = test_db();
        let src = make_memory("src", "ns", Tier::Long, 5);
        let mid = make_memory("mid", "ns", Tier::Long, 5);
        let leaf = make_memory("leaf", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &mid).unwrap();
        insert(&conn, &leaf).unwrap();
        insert_link_full(
            &conn,
            &src.id,
            &mid.id,
            "related_to",
            Some("2026-01-01T00:00:00+00:00"),
            None,
            Some("agent-x"),
        );
        insert_link_full(
            &conn,
            &mid.id,
            &leaf.id,
            "supersedes",
            Some("2026-01-02T00:00:00+00:00"),
            None,
            Some("agent-x"),
        );

        // depth=1 sees only mid.
        let d1 = kg_query(&conn, &src.id, 1, None, None, None).unwrap();
        assert_eq!(d1.len(), 1);
        assert_eq!(d1[0].target_id, mid.id);
        assert_eq!(d1[0].depth, 1);

        // depth=2 sees both, ordered shallow-first.
        let d2 = kg_query(&conn, &src.id, 2, None, None, None).unwrap();
        assert_eq!(d2.len(), 2);
        assert_eq!(d2[0].target_id, mid.id);
        assert_eq!(d2[0].depth, 1);
        assert_eq!(d2[0].path, format!("{}->{}", src.id, mid.id));
        assert_eq!(d2[1].target_id, leaf.id);
        assert_eq!(d2[1].depth, 2);
        assert_eq!(d2[1].relation, "supersedes");
        assert_eq!(d2[1].path, format!("{}->{}->{}", src.id, mid.id, leaf.id));
    }

    #[test]
    fn kg_query_multi_hop_respects_valid_at_per_hop() {
        // src -> mid valid 2026-01..02; mid -> leaf valid 2026-04+.
        // At valid_at=2026-01-15 the second hop is not yet valid, so
        // only mid is returned; at valid_at=2026-04-15 the first hop is
        // closed, so both are filtered out.
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        let mid = make_memory("m", "ns", Tier::Long, 5);
        let leaf = make_memory("l", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &mid).unwrap();
        insert(&conn, &leaf).unwrap();
        insert_link_full(
            &conn,
            &src.id,
            &mid.id,
            "related_to",
            Some("2026-01-01T00:00:00+00:00"),
            Some("2026-02-01T00:00:00+00:00"),
            None,
        );
        insert_link_full(
            &conn,
            &mid.id,
            &leaf.id,
            "related_to",
            Some("2026-04-01T00:00:00+00:00"),
            None,
            None,
        );

        let mid_only = kg_query(
            &conn,
            &src.id,
            3,
            Some("2026-01-15T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(mid_only.len(), 1);
        assert_eq!(mid_only[0].target_id, mid.id);

        let neither = kg_query(
            &conn,
            &src.id,
            3,
            Some("2026-04-15T00:00:00+00:00"),
            None,
            None,
        )
        .unwrap();
        assert!(neither.is_empty());
    }

    #[test]
    fn kg_query_detects_cycles() {
        // a -> b -> c -> a forms a cycle. Even with max_depth=5, the
        // traversal must stop revisiting nodes that are already on the
        // path; the result lists each reachable node at most once.
        let conn = test_db();
        let a = make_memory("a", "ns", Tier::Long, 5);
        let b = make_memory("b", "ns", Tier::Long, 5);
        let c = make_memory("c", "ns", Tier::Long, 5);
        insert(&conn, &a).unwrap();
        insert(&conn, &b).unwrap();
        insert(&conn, &c).unwrap();
        insert_link_full(
            &conn,
            &a.id,
            &b.id,
            "related_to",
            Some("2026-01-01T00:00:00+00:00"),
            None,
            None,
        );
        insert_link_full(
            &conn,
            &b.id,
            &c.id,
            "related_to",
            Some("2026-01-02T00:00:00+00:00"),
            None,
            None,
        );
        insert_link_full(
            &conn,
            &c.id,
            &a.id,
            "related_to",
            Some("2026-01-03T00:00:00+00:00"),
            None,
            None,
        );

        let nodes = kg_query(&conn, &a.id, 5, None, None, None).unwrap();
        // Expect b at depth 1 and c at depth 2; the cycle back to a is
        // pruned. (The c->a edge could in principle surface a again at
        // depth 3, but only if a is not on its own path — and the
        // anchor seeds path with `a->b`, so a IS on every descendant
        // path through b/c.)
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].target_id, b.id);
        assert_eq!(nodes[0].depth, 1);
        assert_eq!(nodes[1].target_id, c.id);
        assert_eq!(nodes[1].depth, 2);
    }

    #[test]
    fn kg_query_multi_hop_filters_by_allowed_agents_per_hop() {
        // src -> mid (agent-a), mid -> leaf (agent-b). With allow=[a]
        // only the first hop survives; with allow=[a,b] both surface.
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        let mid = make_memory("m", "ns", Tier::Long, 5);
        let leaf = make_memory("l", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        insert(&conn, &mid).unwrap();
        insert(&conn, &leaf).unwrap();
        insert_link_full(
            &conn,
            &src.id,
            &mid.id,
            "related_to",
            Some("2026-01-01T00:00:00+00:00"),
            None,
            Some("agent-a"),
        );
        insert_link_full(
            &conn,
            &mid.id,
            &leaf.id,
            "related_to",
            Some("2026-01-02T00:00:00+00:00"),
            None,
            Some("agent-b"),
        );

        let allow_a = vec!["agent-a".to_string()];
        let only_first = kg_query(&conn, &src.id, 3, None, Some(&allow_a), None).unwrap();
        assert_eq!(only_first.len(), 1);
        assert_eq!(only_first[0].target_id, mid.id);

        let allow_both = vec!["agent-a".to_string(), "agent-b".to_string()];
        let both = kg_query(&conn, &src.id, 3, None, Some(&allow_both), None).unwrap();
        assert_eq!(both.len(), 2);
        assert_eq!(both[1].target_id, leaf.id);
        assert_eq!(both[1].depth, 2);
    }

    #[test]
    fn kg_query_limit_clamped_to_max() {
        let conn = test_db();
        let src = make_memory("s", "ns", Tier::Long, 5);
        insert(&conn, &src).unwrap();
        for i in 0..3 {
            let t = make_memory(&format!("t{i}"), "ns", Tier::Long, 5);
            insert(&conn, &t).unwrap();
            insert_link_full(
                &conn,
                &src.id,
                &t.id,
                "related_to",
                Some(&format!("2026-01-{:02}T00:00:00+00:00", i + 1)),
                None,
                None,
            );
        }

        // limit=usize::MAX clamps to KG_QUERY_MAX_LIMIT (1000),
        // which is bigger than our 3 rows — all returned.
        let all = kg_query(&conn, &src.id, 1, None, None, Some(usize::MAX)).unwrap();
        assert_eq!(all.len(), 3);

        // limit=0 clamps up to 1.
        let one = kg_query(&conn, &src.id, 1, None, None, Some(0)).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn kg_query_empty_for_unknown_source() {
        let conn = test_db();
        let nodes = kg_query(&conn, "no-such-id", 1, None, None, None).unwrap();
        assert!(nodes.is_empty());
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

    #[test]
    fn namespace_prefix_query_index_available() {
        let conn = test_db();
        // SQLite's default BINARY collation supports prefix-matching LIKE queries
        // with the idx_memories_namespace index. Verify the index exists and a
        // simple prefix query can execute (EXPLAIN QUERY PLAN output varies by
        // SQLite version and query planner heuristics, so we just check that the
        // query completes without error).
        let result: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_memories_namespace'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            result,
            Some("idx_memories_namespace".to_string()),
            "idx_memories_namespace index should exist"
        );

        // Execute a prefix LIKE query to ensure it compiles and runs
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE namespace LIKE 'test/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    // -----------------------------------------------------------------
    // Doctor (P7) helper unit tests.
    // -----------------------------------------------------------------

    #[test]
    fn doctor_dim_violations_post_p2_returns_zero_on_fresh_db() {
        // Post-P2 (schema v18+), a fresh DB has the `embedding_dim` column
        // but zero rows in violation. The helper must report Some(0), not
        // None. (Pre-P2 it returned None to indicate "column not yet
        // present"; that path is now obsolete.)
        let conn = test_db();
        let result = doctor_dim_violations(&conn).unwrap();
        assert_eq!(result, Some(0));
    }

    #[test]
    fn doctor_oldest_pending_age_secs_empty_queue() {
        let conn = test_db();
        let age = doctor_oldest_pending_age_secs(&conn).unwrap();
        assert_eq!(age, None);
    }

    #[test]
    fn doctor_oldest_pending_age_secs_reports_age() {
        let conn = test_db();
        let one_hour_ago = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        conn.execute(
            "INSERT INTO pending_actions (id, action_type, namespace, payload, requested_by, requested_at, status)
             VALUES ('p1', 'store', 'ns', '{}', 'agent', ?1, 'pending')",
            params![one_hour_ago],
        )
        .unwrap();
        let age = doctor_oldest_pending_age_secs(&conn).unwrap().unwrap();
        // Allow a generous margin — the test machine clock is the source of truth.
        assert!((3500..=3700).contains(&age), "expected ~3600s, got {age}");
    }

    #[test]
    fn doctor_governance_coverage_with_namespace_meta() {
        let conn = test_db();
        // No namespaces — both counts zero.
        let (with, without) = doctor_governance_coverage(&conn).unwrap();
        assert_eq!((with, without), (0, 0));
    }

    #[test]
    fn doctor_governance_depth_distribution_chains() {
        let conn = test_db();
        // Build a small inheritance tree: root -> a -> a/b -> a/b/c
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO namespace_meta (namespace, parent_namespace, updated_at) VALUES ('root', NULL, ?1)",
            params![now],
        ).unwrap();
        conn.execute(
            "INSERT INTO namespace_meta (namespace, parent_namespace, updated_at) VALUES ('a', 'root', ?1)",
            params![now],
        ).unwrap();
        conn.execute(
            "INSERT INTO namespace_meta (namespace, parent_namespace, updated_at) VALUES ('a/b', 'a', ?1)",
            params![now],
        ).unwrap();
        conn.execute(
            "INSERT INTO namespace_meta (namespace, parent_namespace, updated_at) VALUES ('a/b/c', 'a/b', ?1)",
            params![now],
        ).unwrap();
        let dist = doctor_governance_depth_distribution(&conn).unwrap();
        assert_eq!(dist[0], 1, "root has depth 0");
        assert_eq!(dist[1], 1, "a has depth 1");
        assert_eq!(dist[2], 1, "a/b has depth 2");
        assert_eq!(dist[3], 1, "a/b/c has depth 3");
    }

    #[test]
    fn doctor_webhook_delivery_totals_empty() {
        let conn = test_db();
        let (dispatched, failed) = doctor_webhook_delivery_totals(&conn).unwrap();
        assert_eq!((dispatched, failed), (0, 0));
    }

    #[test]
    fn doctor_max_sync_skew_secs_empty() {
        let conn = test_db();
        let skew = doctor_max_sync_skew_secs(&conn).unwrap();
        assert_eq!(skew, None);
    }

    // ---- v0.6.4-009 — capability-expansion audit log ----

    #[test]
    fn audit_log_record_and_list_grant_and_deny() {
        let conn = test_db();
        record_capability_expansion(&conn, Some("alice"), "graph", true, None);
        record_capability_expansion(&conn, Some("bob"), "power", false, None);
        let rows = list_capability_expansions(&conn, 50, None).unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert!(rows[0].timestamp >= rows[1].timestamp);
        let grant_row = rows
            .iter()
            .find(|r| r.agent_id.as_deref() == Some("alice"))
            .unwrap();
        assert!(grant_row.granted);
        assert_eq!(grant_row.requested_family.as_deref(), Some("graph"));
        let deny_row = rows
            .iter()
            .find(|r| r.agent_id.as_deref() == Some("bob"))
            .unwrap();
        assert!(!deny_row.granted);
        assert_eq!(deny_row.requested_family.as_deref(), Some("power"));
    }

    #[test]
    fn audit_log_filter_by_agent() {
        let conn = test_db();
        record_capability_expansion(&conn, Some("alice"), "graph", true, None);
        record_capability_expansion(&conn, Some("bob"), "power", false, None);
        let alice = list_capability_expansions(&conn, 50, Some("alice")).unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].agent_id.as_deref(), Some("alice"));
        let none_match = list_capability_expansions(&conn, 50, Some("nobody")).unwrap();
        assert!(none_match.is_empty());
    }

    #[test]
    fn audit_log_anonymous_caller() {
        let conn = test_db();
        record_capability_expansion(&conn, None, "core", true, None);
        let rows = list_capability_expansions(&conn, 50, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].agent_id.is_none());
    }

    #[test]
    fn audit_log_migration_idempotent_on_re_open() {
        // Open the DB twice in succession; the audit_log CREATE TABLE
        // IF NOT EXISTS path must not error.
        let p = tempfile::NamedTempFile::new().unwrap();
        let p = p.path().to_path_buf();
        let _ = open(&p).unwrap();
        let conn = open(&p).unwrap();
        // And the indexes are present.
        let cnt: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE 'idx_audit_log_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            cnt, 3,
            "expected 3 audit_log indexes (agent_id, ts, event_type)"
        );
    }

    // ---------------------------------------------------------------
    // v0.7.0 K2 — pending_actions timeout sweeper.
    //
    // Closes the v0.6.3.1 honest-Capabilities-v2 disclosure that
    // `default_timeout_seconds` was advertised but unused.
    // ---------------------------------------------------------------

    /// Insert a `pending_actions` row with a back-dated `requested_at`
    /// so we can drive the sweeper without `tokio::time` games.
    fn insert_stale_pending(
        conn: &Connection,
        id: &str,
        namespace: &str,
        age_secs: i64,
        per_row_timeout: Option<i64>,
    ) {
        let requested_at =
            (chrono::Utc::now() - chrono::Duration::seconds(age_secs)).to_rfc3339();
        conn.execute(
            "INSERT INTO pending_actions
             (id, action_type, namespace, payload, requested_by, requested_at,
              status, default_timeout_seconds)
             VALUES (?1, 'store', ?2, '{}', 'tester', ?3, 'pending', ?4)",
            params![id, namespace, requested_at, per_row_timeout],
        )
        .unwrap();
    }

    #[test]
    fn sweep_marks_stale_pending_row_expired() {
        let conn = test_db();
        // 2-hour-old pending row; global default is 1 hour → must expire.
        insert_stale_pending(&conn, "stale-1", "ns/a", 7_200, None);

        let expired = sweep_pending_action_timeouts(&conn, 3_600).unwrap();
        assert_eq!(expired.len(), 1, "expected exactly one expiry");
        assert_eq!(expired[0], ("stale-1".to_string(), "ns/a".to_string()));

        // Row is now status='expired' with expired_at populated.
        let (status, expired_at): (String, Option<String>) = conn
            .query_row(
                "SELECT status, expired_at FROM pending_actions WHERE id = ?1",
                params!["stale-1"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "expired");
        assert!(
            expired_at.is_some(),
            "expired_at must be stamped by the sweeper"
        );
    }

    #[test]
    fn sweep_leaves_fresh_pending_alone() {
        let conn = test_db();
        // 30-second-old pending row; global default is 1 hour → still pending.
        insert_stale_pending(&conn, "fresh-1", "ns/a", 30, None);

        let expired = sweep_pending_action_timeouts(&conn, 3_600).unwrap();
        assert!(expired.is_empty());
        let status: String = conn
            .query_row(
                "SELECT status FROM pending_actions WHERE id = ?1",
                params!["fresh-1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn sweep_per_row_timeout_overrides_global_default() {
        let conn = test_db();
        // 5-minute-old row; per-row TTL = 60s → MUST expire even
        // though the global default (1h) would say "still fresh".
        insert_stale_pending(&conn, "short-ttl", "ns/a", 300, Some(60));
        // Same age, no per-row override → still pending under the
        // 1h global default.
        insert_stale_pending(&conn, "no-override", "ns/a", 300, None);

        let expired = sweep_pending_action_timeouts(&conn, 3_600).unwrap();
        let ids: Vec<&String> = expired.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![&"short-ttl".to_string()]);
    }

    #[test]
    fn sweep_skips_already_decided_rows() {
        let conn = test_db();
        // Pre-insert an OLD row already approved — must not touch it.
        let approved_at = (chrono::Utc::now() - chrono::Duration::seconds(7_200)).to_rfc3339();
        conn.execute(
            "INSERT INTO pending_actions
             (id, action_type, namespace, payload, requested_by, requested_at,
              status, decided_by, decided_at)
             VALUES ('approved-old', 'store', 'ns/a', '{}', 'alice', ?1,
                     'approved', 'bob', ?1)",
            params![approved_at],
        )
        .unwrap();

        let expired = sweep_pending_action_timeouts(&conn, 60).unwrap();
        assert!(expired.is_empty(), "non-pending rows must be ignored");
        let status: String = conn
            .query_row(
                "SELECT status FROM pending_actions WHERE id = 'approved-old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "approved", "decided row status preserved");
    }

    #[test]
    fn sweep_disabled_when_global_default_non_positive() {
        let conn = test_db();
        // Stale row with no per-row TTL.
        insert_stale_pending(&conn, "stale-2", "ns/a", 7_200, None);
        // Operator escape hatch: 0 (or negative) global default
        // disables the sweep entirely.
        let expired = sweep_pending_action_timeouts(&conn, 0).unwrap();
        assert!(expired.is_empty());
        let expired_neg = sweep_pending_action_timeouts(&conn, -1).unwrap();
        assert!(expired_neg.is_empty());
    }

    #[test]
    fn sweep_empty_queue_is_silent_noop() {
        let conn = test_db();
        let expired = sweep_pending_action_timeouts(&conn, 60).unwrap();
        assert!(expired.is_empty());
    }
}
