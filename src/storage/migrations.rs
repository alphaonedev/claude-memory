// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! SQLite schema definition + migration ladder. v0.7.0 L0.5-3
//! extracted the `SCHEMA` constant, the `MIGRATION_V*_SQLITE`
//! include-bytes constants, the `CURRENT_SCHEMA_VERSION` parallel
//! constant, and the `migrate` function out of `src/db.rs` into
//! this sub-module. Pure refactor — semantics unchanged. The
//! `MAX_SUPPORTED_SCHEMA` constant in `cli::boot` must still bump
//! in lockstep with [`CURRENT_SCHEMA_VERSION`] (current value: 29).

use anyhow::Result;
use rusqlite::{Connection, params};

pub(super) const SCHEMA: &str = r"
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
    metadata         TEXT NOT NULL DEFAULT '{}',
    -- v0.7.0 Task 1/8 (recursive learning, schema v29) — depth in the
    -- substrate-native reflection recursion tree. `0` for caller-minted
    -- memories (and any pre-v0.7.0 row); positive for synthesised
    -- reflections. Mirrors `models::Memory::reflection_depth`.
    reflection_depth INTEGER NOT NULL DEFAULT 0
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
// v22 = v0.7.0 I1 (attested-cortex epic) `memory_transcripts` BLOB
//       store with zstd-3 content blobs. Substrate for I2 (join
//       table), I3 (archive→prune lifecycle), I4 (memory_replay),
//       I5/R5 (pre_store extraction hook).
// v23 = v0.7.0 H2 (attested-cortex epic, outbound link signing)
//       `memory_links.attest_level` TEXT column ("unsigned" |
//       "self_signed" | "peer_attested"). The companion `signature`
//       BLOB column shipped dead in v15 and is now live. H3+H4 will
//       layer inbound verification + the `memory_verify` MCP tool on
//       top of this column.
// v24 = v0.7.0 I2 (attested-cortex epic) `memory_transcript_links`
//       join table establishing the m:n relationship between
//       `memories` and the `memory_transcripts` substrate from I1
//       (v22). Optional (span_start, span_end) byte offsets address a
//       sub-region of the decompressed transcript. ON DELETE CASCADE
//       on both foreign keys keeps the table free of dangling rows
//       when memories are deleted or I3's archive->prune lifecycle
//       removes transcripts. Substrate for I4 (memory_replay) and
//       I5/R5 (pre_store extraction hook).
// v25 = v0.7.0 I3 (attested-cortex epic) per-namespace transcript TTL
//       with archive->prune lifecycle. Adds the `archived_at TEXT`
//       column on `memory_transcripts` (NULL = live, RFC3339 = the
//       moment the sweeper marked the row archived) plus a partial
//       index on archived rows so the prune-phase scan is bounded.
//       The lifecycle sweeper itself lives in `transcripts.rs` and
//       runs on a 10-minute cadence from `daemon_runtime`. Per-
//       namespace TTL overrides arrive via the `[transcripts]`
//       config section (`config.rs`) and are resolved against the
//       transcript's namespace at sweep time.
// v29 = v0.7.0 Task 1/8 (recursive learning) — `memories.reflection_depth`
//       INTEGER NOT NULL DEFAULT 0 column. Depth in the substrate-native
//       reflection recursion tree; 0 for caller-minted (or pre-v0.7.0)
//       rows. ALTER TABLE emitted from Rust (SQLite has no `ADD COLUMN
//       IF NOT EXISTS`); fresh-schema installs pick it up inline from
//       the `SCHEMA` constant above.
const CURRENT_SCHEMA_VERSION: i64 = 29;

const MIGRATION_V15_SQLITE: &str =
    include_str!("../../migrations/sqlite/0010_v063_hierarchy_kg.sql");
// v0.6.3.1 (P4, audit G1): backfill `metadata.governance.inherit = true`
// on existing policies so downstream readers and SQL-side dashboards
// see a consistent shape after upgrade. Idempotent.
const MIGRATION_V17_SQLITE: &str =
    include_str!("../../migrations/sqlite/0012_governance_inherit.sql");
// v0.6.3.1 (P2, audit G4/G5/G13): data-integrity hardening. ALTER TABLEs
// emitted from Rust because SQLite has no `ADD COLUMN IF NOT EXISTS`;
// the SQL file holds idempotent backfills + indexes.
const MIGRATION_V18_SQLITE: &str =
    include_str!("../../migrations/sqlite/0011_v0631_data_integrity.sql");
// v0.6.3.1 (P5, audit G9): webhook event-types column + per-subscriber
// filter index. ADD COLUMN done inline (SQLite has no `ADD COLUMN IF NOT
// EXISTS`); SQL file holds the idempotent index batch.
const MIGRATION_V19_SQLITE: &str =
    include_str!("../../migrations/sqlite/0013_webhook_event_types.sql");
// v0.6.4-009: capability-expansion audit log table. CREATE TABLE IF NOT
// EXISTS + indexes — fully idempotent.
const MIGRATION_V20_SQLITE: &str = include_str!("../../migrations/sqlite/0014_v064_audit_log.sql");
// v0.7.0 K2: pending_actions timeout sweeper. ALTER TABLEs are emitted
// from Rust (see v21 below) because SQLite has no `ADD COLUMN IF NOT
// EXISTS`; this file just holds the idempotent index batch.
const MIGRATION_V21_SQLITE: &str =
    include_str!("../../migrations/sqlite/0015_v07_pending_action_timeouts.sql");
// v0.7.0 I1 — `memory_transcripts` table backing the attested-cortex
// epic. CREATE TABLE IF NOT EXISTS + index — fully idempotent. Substrate
// for I2 (join table), I3 (archive→prune lifecycle), I4 (memory_replay),
// and I5/R5 (pre_store extraction hook).
const MIGRATION_V22_SQLITE: &str = include_str!("../../migrations/sqlite/0016_v07_transcripts.sql");
// v0.7.0 H2 — outbound link signing. ALTER TABLE adding the
// `attest_level` column is emitted from Rust (SQLite has no
// `ADD COLUMN IF NOT EXISTS`); this file holds the idempotent
// backfill ("unsigned" for legacy rows) plus the supporting index.
const MIGRATION_V23_SQLITE: &str =
    include_str!("../../migrations/sqlite/0017_v07_link_attest_level.sql");
// v0.7.0 I2 — `memory_transcript_links` join table connecting
// `memories` to the `memory_transcripts` substrate from I1 (v22).
// CREATE TABLE IF NOT EXISTS + indexes — fully idempotent. Substrate
// only; I4 (memory_replay) reads from this table and I5/R5
// (pre_store extraction hook) writes to it.
const MIGRATION_V24_SQLITE: &str =
    include_str!("../../migrations/sqlite/0018_v07_transcript_links.sql");
// v0.7.0 I3 — per-namespace transcript TTL with archive->prune
// lifecycle. ALTER TABLE adding `memory_transcripts.archived_at` is
// emitted from Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`); the
// SQL file holds the supporting partial index on archived rows so
// the prune-phase scan stays O(archived) rather than O(total).
const MIGRATION_V25_SQLITE: &str =
    include_str!("../../migrations/sqlite/0019_v07_transcript_lifecycle.sql");
// v0.7.0 H5 — append-only `signed_events` audit table backing the
// immutable attestation chain. CREATE TABLE IF NOT EXISTS + indexes —
// fully idempotent. The H5 substrate; H6 read-side tooling layers on
// top.
const MIGRATION_V26_SQLITE: &str =
    include_str!("../../migrations/sqlite/0020_v07_signed_events.sql");
// v0.7.0 K6 — A2A correlation IDs + ACK / retry / DLQ for the
// subscription dispatch path. Adds `subscription_events.correlation_id`
// (UUIDv7 string) for replay-from-cursor lookups, the
// `subscription_events` audit table itself (created here because no
// prior K-track migration introduced it), and the `subscription_dlq`
// table holding deliveries that exhausted the three-attempt retry
// ladder. The ALTER TABLE on a pre-existing `subscription_events`
// row (deployments that hand-rolled it) is emitted from Rust because
// SQLite has no `ADD COLUMN IF NOT EXISTS`; the SQL file holds the
// idempotent CREATE TABLE / CREATE INDEX statements.
const MIGRATION_V27_SQLITE: &str =
    include_str!("../../migrations/sqlite/0021_v07_a2a_correlation.sql");
// v0.7.0 K8 — per-agent quotas (memories/day, storage bytes, links/day).
// CREATE TABLE IF NOT EXISTS + index — fully idempotent. Daily counters
// reset at UTC midnight via the K8 sweep loop wired into
// `daemon_runtime::bootstrap_serve`. The store_memory + memory_link
// write paths consult the row before committing; on exceeded limit the
// call returns a `QUOTA_EXCEEDED` diagnostic naming the limit hit.
const MIGRATION_V28_SQLITE: &str =
    include_str!("../../migrations/sqlite/0022_v07_agent_quotas.sql");

#[allow(clippy::too_many_lines)]
pub(super) fn migrate(conn: &Connection) -> Result<()> {
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
                conn.execute("ALTER TABLE pending_actions ADD COLUMN expired_at TEXT", [])?;
            }
            conn.execute_batch(MIGRATION_V21_SQLITE)?;
        }
        if version < 22 {
            // v0.7.0 I1 — `memory_transcripts` substrate for the
            // attested-cortex epic. CREATE TABLE IF NOT EXISTS + index
            // — fully idempotent. Subsequent I-track tasks (I2 join
            // table, I3 archive→prune, I4 memory_replay, I5/R5 pre_store
            // hook) layer on top of this substrate.
            conn.execute_batch(MIGRATION_V22_SQLITE)?;
        }
        if version < 23 {
            // v0.7.0 H2 — outbound link signing. Adds the `attest_level`
            // TEXT column to `memory_links` ("unsigned" | "self_signed"
            // | "peer_attested"); the companion `signature` BLOB column
            // shipped dead in v15 (Stream B) and is now live. ALTER
            // TABLE done inline (SQLite has no `ADD COLUMN IF NOT
            // EXISTS`); the SQL file holds the idempotent backfill +
            // index. H3 will populate `peer_attested` on the inbound
            // verification path; H4 layers `memory_verify` on top of
            // this column.
            let has_attest_level = conn
                .prepare("SELECT attest_level FROM memory_links LIMIT 0")
                .is_ok();
            if !has_attest_level {
                conn.execute("ALTER TABLE memory_links ADD COLUMN attest_level TEXT", [])?;
            }
            conn.execute_batch(MIGRATION_V23_SQLITE)?;
        }
        if version < 24 {
            // v0.7.0 I2 — `memory_transcript_links` join table tying
            // memories to the `memory_transcripts` substrate from I1.
            // CREATE TABLE IF NOT EXISTS + indexes — fully idempotent.
            // Substrate only; I4 layers `memory_replay` on top, I5/R5
            // wires the pre_store extraction hook that populates it.
            conn.execute_batch(MIGRATION_V24_SQLITE)?;
        }
        if version < 25 {
            // v0.7.0 I3 — per-namespace transcript TTL with archive→
            // prune lifecycle. Adds `memory_transcripts.archived_at`
            // (NULL = live, RFC3339 = archived). The lifecycle
            // sweeper in `transcripts.rs` consults this column; the
            // partial index from the SQL file keeps the prune-phase
            // scan bounded. Substrate for the 10-minute background
            // task wired into `daemon_runtime::bootstrap_serve`.
            let has_archived_at = conn
                .prepare("SELECT archived_at FROM memory_transcripts LIMIT 0")
                .is_ok();
            if !has_archived_at {
                conn.execute(
                    "ALTER TABLE memory_transcripts ADD COLUMN archived_at TEXT",
                    [],
                )?;
            }
            conn.execute_batch(MIGRATION_V25_SQLITE)?;
        }
        if version < 26 {
            // v0.7.0 H5 — append-only `signed_events` audit table.
            // CREATE TABLE IF NOT EXISTS + indexes — fully idempotent;
            // see MIGRATION_V26_SQLITE for the substrate documentation.
            conn.execute_batch(MIGRATION_V26_SQLITE)?;
        }
        if version < 27 {
            // v0.7.0 K6 — A2A correlation IDs + DLQ. Brings up the
            // `subscription_events` audit table (if not already
            // present) and the `subscription_dlq` table. If a prior
            // operator hand-rolled `subscription_events`, the
            // CREATE TABLE IF NOT EXISTS is a no-op but they may be
            // missing the new `correlation_id` column — we ALTER it
            // in here from Rust because SQLite has no `ADD COLUMN IF
            // NOT EXISTS`.
            conn.execute_batch(MIGRATION_V27_SQLITE)?;
            let has_correlation = conn
                .prepare("SELECT correlation_id FROM subscription_events LIMIT 0")
                .is_ok();
            if !has_correlation {
                conn.execute(
                    "ALTER TABLE subscription_events ADD COLUMN correlation_id TEXT NOT NULL DEFAULT ''",
                    [],
                )?;
            }
        }
        if version < 28 {
            // v0.7.0 K8 — per-agent quotas (memories/day, storage
            // bytes, links/day). CREATE TABLE IF NOT EXISTS + index —
            // fully idempotent; see MIGRATION_V28_SQLITE for the
            // substrate documentation.
            conn.execute_batch(MIGRATION_V28_SQLITE)?;
        }
        if version < 29 {
            // v0.7.0 Task 1/8 (recursive learning) — add
            // `memories.reflection_depth INTEGER NOT NULL DEFAULT 0`.
            // ALTER TABLE done inline (SQLite has no `ADD COLUMN IF NOT
            // EXISTS`); the column-existence probe makes the step
            // idempotent against a partially-stamped database.
            let has_reflection_depth = conn
                .prepare("SELECT reflection_depth FROM memories LIMIT 0")
                .is_ok();
            if !has_reflection_depth {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN reflection_depth INTEGER NOT NULL DEFAULT 0",
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
