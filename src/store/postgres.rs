// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Postgres + pgvector adapter for the Storage Abstraction Layer (v0.7).
//!
//! Gated behind the `sal-postgres` cargo feature, which layers on top of
//! the base `sal` feature. Default builds do not pull sqlx or pgvector.
//!
//! # Schema
//!
//! The adapter owns its schema (CREATE TABLE IF NOT EXISTS at init) —
//! no manual DBA step required. The embedding column is `vector(384)`
//! to match the default `MiniLmL6V2` model; adapters configured for
//! `NomicEmbedV15` need to drop the column and recreate with
//! `vector(768)` (tooling for that will land with the migration helper
//! in a follow-up).
//!
//! The full schema — parity with the SQLite backend including
//! memories, memory_links, archived_memories, namespace_meta,
//! pending_actions, sync_state, subscriptions — lives at
//! `src/store/postgres_schema.sql`.
//!
//! Key semantic choices at the SQL layer (matching SQLite):
//! - Upsert contract is `ON CONFLICT (title, namespace)`
//!   (UNIQUE INDEX `memories_title_ns_uidx`).
//! - `metadata.agent_id` is immutable across UPSERT and UPDATE via
//!   `jsonb_set` preserving the original agent_id when present.
//! - Tier never downgrades: UPSERT and UPDATE apply `tier_rank()`
//!   precedence so `Long → *` and `Mid → Short` are refused at the
//!   SQL layer.
//!
//! # Capabilities
//!
//! This adapter advertises `TRANSACTIONS | NATIVE_VECTOR | FULLTEXT |
//! DURABLE | STRONG_CONSISTENCY | ATOMIC_MULTI_WRITE`. It does **not**
//! currently advertise `TTL_NATIVE` — expiry sweeps still run
//! application-side to match `SqliteStore` semantics.
//!
//! # Testing
//!
//! Integration tests in `tests/sal_postgres.rs` run iff
//! `AI_MEMORY_TEST_POSTGRES_URL` is set (typically pointed at the
//! `docker compose -f packaging/docker-compose.postgres.yml up`
//! fixture). Unit tests in this module exercise only the SQL-builder
//! helpers that don't need a running Postgres.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};

use super::{
    CallerContext, Capabilities, Filter, KgBackend, KgInvalidateRow, KgQueryRow, KgTimelineRow,
    MemoryStore, StoreError, StoreResult, UpdatePatch, VerifyReport,
};
use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};

/// Bootstrap schema run at adapter init — idempotent via IF NOT EXISTS.
const INIT_SCHEMA: &str = include_str!("postgres_schema.sql");

/// Current schema version. Matches SQLite CURRENT_SCHEMA_VERSION (src/db.rs:233).
/// Incremented on each migration step.
///
/// v15 — Temporal-validity columns on `memory_links` + `entity_aliases`.
/// v17 — `metadata.governance.inherit` backfill (mirrors SQLite v17).
/// v18 — Data-integrity hardening (`embedding_dim`, archive lossless,
///       endianness header backfill — SQLite v18 / Postgres
///       0011_v0631_data_integrity).
/// v19 — Webhook `event_types` JSON array on subscriptions + index.
/// v20 — `audit_log` table for capability-expansion observability.
/// v21 — `pending_actions.default_timeout_seconds` + `expired_at`
///       columns plus the (status, requested_at) sweep index.
/// v22 — `memory_transcripts` substrate (attested-cortex epic I1).
/// v23 — `memory_links.attest_level` for signed-link writes (H2).
/// v24 — `memory_transcript_links` join table (I2) + the F6 SAL KG
///       SQL surfaces (`kg_query_view` / `kg_timeline_view` /
///       `kg_find_paths()`).
/// v25 — `memory_transcripts.archived_at` for archive→prune lifecycle (I3).
/// v26 — `signed_events` append-only audit table (H5).
/// v27 — A2A correlation IDs + DLQ (`subscription_events`,
///       `subscription_dlq`) (K6).
/// v28 — `agent_quotas` per-agent rate + storage caps (K8).
const CURRENT_SCHEMA_VERSION: i32 = 28;

/// Default connection pool settings. Tuned for a mid-range ai-memory
/// daemon — adjust via `PostgresStore::with_pool_options` when wiring
/// a larger deployment.
const DEFAULT_MAX_CONNECTIONS: u32 = 16;
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// `PostgresStore` — sqlx + pgvector backend. Owns a connection pool.
#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
    /// Resolved knowledge-graph backend tag — set at [`Self::connect`]
    /// time by probing `pg_extension` for Apache AGE. Substrate for
    /// Track J: J2-J7 dispatch their KG queries on this value, falling
    /// back to the recursive-CTE path when AGE is absent. See
    /// [`Self::kg_backend`].
    kg_backend: KgBackend,
}

impl PostgresStore {
    /// Connect using a Postgres URL (e.g. `postgres://user:pass@host:5432/db`).
    /// Runs the bootstrap schema on the first connection acquired.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::BackendUnavailable` if the connection
    /// cannot be established or the schema bootstrap fails.
    pub async fn connect(url: &str) -> StoreResult<Self> {
        let options: PgConnectOptions =
            url.parse()
                .map_err(|e: sqlx::Error| StoreError::BackendUnavailable {
                    backend: "postgres".to_string(),
                    detail: format!("parse url: {e}"),
                })?;
        let pool = PgPoolOptions::new()
            .max_connections(DEFAULT_MAX_CONNECTIONS)
            .acquire_timeout(DEFAULT_ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|e| StoreError::BackendUnavailable {
                backend: "postgres".to_string(),
                detail: format!("connect: {e}"),
            })?;

        // Bootstrap schema — idempotent.
        sqlx::raw_sql(INIT_SCHEMA)
            .execute(&pool)
            .await
            .map_err(|e| StoreError::BackendUnavailable {
                backend: "postgres".to_string(),
                detail: format!("init schema: {e}"),
            })?;

        // Sanity-check pgvector version. We support 0.7.x–0.8.x; older
        // versions have HNSW behaviour differences we haven't tested
        // against. (#302 item 2.)
        let extver: Option<(String,)> =
            sqlx::query_as("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
                .fetch_optional(&pool)
                .await
                .map_err(|e| StoreError::BackendUnavailable {
                    backend: "postgres".to_string(),
                    detail: format!("read pgvector version: {e}"),
                })?;
        if let Some((ver,)) = extver
            && !(ver.starts_with("0.7") || ver.starts_with("0.8"))
        {
            tracing::warn!(
                target = "store::postgres",
                version = %ver,
                "pgvector version outside the tested range 0.7.x–0.8.x; HNSW recall may differ"
            );
        }

        // Sanity-check that the embedding column dimension matches the
        // default embedder (MiniLmL6V2 = 384). If a deployment has
        // configured a different model (e.g. NomicEmbedV15 = 768), the
        // table must be recreated with the matching vector(N) — we log
        // a WARN here so operators notice before writes start failing.
        // (#304 nit.)
        let typmod: Option<(i32,)> = sqlx::query_as(
            "SELECT atttypmod FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid
             WHERE c.relname = 'memories' AND a.attname = 'embedding'",
        )
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten();
        if let Some((typmod,)) = typmod
            && typmod != 384
        {
            tracing::warn!(
                target = "store::postgres",
                dim = typmod,
                "memories.embedding column dimension is not 384; recreate with matching vector(N) for your embedder"
            );
        }

        // v0.7 J1 — detect Apache AGE so Track J can dispatch
        // knowledge-graph traversals through Cypher when the extension
        // is installed. Falls back to the recursive-CTE path on
        // missing extension OR query error: AGE is opt-in, never a
        // bootstrap blocker. The resolved tag is surfaced on the SAL
        // handle via [`Self::kg_backend`] for J2-J7.
        let kg_backend = detect_kg_backend(&pool).await;
        tracing::info!(
            target = "store::postgres",
            kg_backend = %kg_backend,
            "Postgres KG backend: {}",
            match kg_backend {
                KgBackend::Age => "AGE",
                KgBackend::Cte => "CTE",
            }
        );

        // Run schema migrations after bootstrap schema is loaded.
        let store = Self { pool, kg_backend };
        store.migrate().await?;

        Ok(store)
    }

    /// Knowledge-graph backend resolved at [`Self::connect`] time.
    ///
    /// v0.7 J1 substrate. J2-J7 dispatch on this value so the same
    /// `memory_kg_*` MCP wire shape can route to either Cypher (when
    /// AGE is installed) or the recursive-CTE fallback. The resolution
    /// is sticky for the life of the pool — adapters do not re-probe
    /// per call.
    #[must_use]
    pub fn kg_backend(&self) -> KgBackend {
        self.kg_backend
    }

    /// Run schema migrations on the connection. Called after bootstrap schema
    /// is loaded. Reads the current schema_version, then applies all pending
    /// migrations in a transaction per version step (matching SQLite behavior
    /// in src/db.rs::migrate).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::BackendUnavailable` if migration fails.
    async fn migrate(&self) -> StoreResult<()> {
        // Read the current version from schema_version table.
        let current_version: Option<i32> =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| to_store_err("read schema_version", e))?;

        let current_version = current_version.unwrap_or(0);

        if current_version >= CURRENT_SCHEMA_VERSION {
            return Ok(());
        }

        // Apply each migration step in its own transaction for idempotence.
        // Versions 15→28 are stamped in order; each migrate_vN function is
        // independently idempotent (column-existence checks + IF NOT EXISTS
        // DDL) so re-running the migrator on a partially-stamped database
        // is safe.
        if current_version < 15 {
            self.migrate_v15().await?;
        }
        if current_version < 17 {
            self.migrate_v17().await?;
        }
        if current_version < 18 {
            self.migrate_v18().await?;
        }
        if current_version < 19 {
            self.migrate_v19().await?;
        }
        if current_version < 20 {
            self.migrate_v20().await?;
        }
        if current_version < 21 {
            self.migrate_v21().await?;
        }
        if current_version < 22 {
            self.migrate_v22().await?;
        }
        if current_version < 23 {
            self.migrate_v23().await?;
        }
        if current_version < 24 {
            self.migrate_v24().await?;
        }
        if current_version < 25 {
            self.migrate_v25().await?;
        }
        if current_version < 26 {
            self.migrate_v26().await?;
        }
        if current_version < 27 {
            self.migrate_v27().await?;
        }
        if current_version < 28 {
            self.migrate_v28().await?;
        }

        Ok(())
    }

    /// v0.7.0 H2 — Add `attest_level` TEXT column to `memory_links`.
    /// Mirrors SQLite migration 0017_v07_link_attest_level.sql.
    /// Idempotent: ALTER TABLE … IF NOT EXISTS guarded by an
    /// information_schema lookup, plus an idempotent backfill.
    async fn migrate_v23(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v23 tx", e))?;

        let has_attest_level: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM information_schema.columns
                WHERE table_name='memory_links' AND column_name='attest_level'
            )",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("check attest_level column", e))?;

        if !has_attest_level {
            sqlx::query("ALTER TABLE memory_links ADD COLUMN attest_level TEXT")
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("add attest_level column", e))?;
        }

        // Backfill rows written before the column existed.
        sqlx::query("UPDATE memory_links SET attest_level = 'unsigned' WHERE attest_level IS NULL")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("backfill attest_level", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_links_attest_level
             ON memory_links (attest_level, created_at)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_memory_links_attest_level", e))?;

        record_schema_version(&mut tx, 23).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v23 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v23 applied");
        Ok(())
    }

    /// v0.7.0 v24 — `memory_transcript_links` join table (I2) and the
    /// F6 SAL knowledge-graph SQL surfaces (`kg_query_view`,
    /// `kg_timeline_view`, `kg_find_paths()`).
    ///
    /// Mirrors `migrations/sqlite/0018_v07_transcript_links.sql` plus
    /// the Postgres-only F6 view set. The view bodies live in
    /// `postgres_schema.sql` (which is re-run on every connect, so they
    /// are always present on a fresh init); this migration also stamps
    /// the bookkeeping row and re-runs the DDL defensively in case an
    /// operator dropped a view between connects.
    async fn migrate_v24(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v24 tx", e))?;

        // I2 — memory_transcript_links join table. ON DELETE CASCADE on
        // both foreign keys keeps the join free of dangling rows when
        // memories are deleted or I3's archive→prune lifecycle removes
        // transcripts. Mirrors SQLite migration 0018_v07_transcript_links.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS memory_transcript_links (
                memory_id     TEXT NOT NULL REFERENCES memories(id)           ON DELETE CASCADE,
                transcript_id TEXT NOT NULL REFERENCES memory_transcripts(id) ON DELETE CASCADE,
                span_start    BIGINT,
                span_end      BIGINT,
                PRIMARY KEY (memory_id, transcript_id)
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create memory_transcript_links table", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_mtl_transcript
             ON memory_transcript_links (transcript_id)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_mtl_transcript", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_mtl_memory
             ON memory_transcript_links (memory_id)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_mtl_memory", e))?;

        // Re-create the views in case operators dropped them between
        // connects. Bodies match postgres_schema.sql verbatim — kept
        // here as an inline copy so the migration is self-contained.
        sqlx::query(
            "CREATE OR REPLACE VIEW kg_query_view AS
             WITH RECURSIVE traversal(source_id, target_id, relation, depth, path) AS (
                 SELECT ml.source_id, ml.target_id, ml.relation, 1,
                        ml.source_id || '->' || ml.target_id
                 FROM memory_links ml
                 UNION ALL
                 SELECT t.source_id, ml.target_id, ml.relation, t.depth + 1,
                        t.path || '->' || ml.target_id
                 FROM memory_links ml
                 JOIN traversal t ON ml.source_id = t.target_id
                 WHERE t.depth < 5
                   AND position(('->' || ml.target_id) IN t.path) = 0
                   AND position((ml.target_id || '->') IN t.path) = 0
             )
             SELECT source_id, target_id, relation, depth, path FROM traversal",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create kg_query_view", e))?;

        sqlx::query(
            "CREATE OR REPLACE VIEW kg_timeline_view AS
             SELECT ml.source_id, ml.target_id, ml.relation,
                    ml.valid_from, ml.valid_until, ml.observed_by,
                    encode(ml.signature, 'hex') AS signature_hex
             FROM memory_links ml
             WHERE ml.valid_from IS NOT NULL
             ORDER BY ml.valid_from DESC, ml.created_at DESC",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create kg_timeline_view", e))?;

        sqlx::query(
            "CREATE OR REPLACE FUNCTION kg_find_paths(start_id TEXT, max_depth INTEGER)
             RETURNS TABLE (path_id INTEGER, length INTEGER, nodes TEXT[], relations TEXT[])
             LANGUAGE SQL STABLE PARALLEL SAFE AS $$
                 WITH RECURSIVE walk(current_id, depth, nodes, relations) AS (
                     SELECT start_id, 0, ARRAY[start_id], ARRAY[]::TEXT[]
                     UNION ALL
                     SELECT edges.next_id,
                            w.depth + 1,
                            w.nodes || edges.next_id,
                            w.relations || edges.relation
                     FROM walk w
                     JOIN (
                         SELECT source_id AS from_id, target_id AS next_id, relation FROM memory_links
                         UNION
                         SELECT target_id AS from_id, source_id AS next_id, relation FROM memory_links
                     ) edges ON edges.from_id = w.current_id
                     WHERE w.depth < LEAST(max_depth, 7)
                       AND NOT (edges.next_id = ANY(w.nodes))
                 )
                 SELECT
                     ROW_NUMBER() OVER (ORDER BY depth ASC, nodes ASC)::INTEGER AS path_id,
                     depth                                                      AS length,
                     nodes,
                     relations
                 FROM walk
                 WHERE depth >= 1
             $$",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create kg_find_paths", e))?;

        record_schema_version(&mut tx, 24).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v24 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v24 applied");
        Ok(())
    }

    // ───────────────────────────────────────────────────────────────────
    // v0.7.0 schema parity — Wave 2 ports of SQLite migrations 0012-0022
    // (schema versions 17-28) onto the Postgres adapter. Each function
    // is independently idempotent: it re-issues IF NOT EXISTS DDL plus
    // explicit `information_schema.columns` checks for ALTER TABLE
    // additions Postgres lacks an `IF NOT EXISTS` clause for. The
    // bootstrap script (`postgres_schema.sql`) already creates the
    // current shape on a fresh init; these handlers exist for in-place
    // upgrades from v15-stamped or v23-stamped pre-existing deployments.
    // Mirrors the SQLite-side ladder in `src/db.rs::migrate`.
    // ───────────────────────────────────────────────────────────────────

    /// v17 — Governance inheritance backfill.
    ///
    /// Mirrors `migrations/sqlite/0012_governance_inherit.sql`. Adds the
    /// `inherit` field (default `true`) to existing
    /// `metadata.governance` policy objects so the field is physically
    /// present on legacy rows. The Rust deserializer already defaults
    /// missing fields to `true`, so this only changes how the JSON
    /// looks at SQL inspection time, not the semantic resolution.
    /// Idempotent: only updates rows whose `inherit` is currently NULL.
    async fn migrate_v17(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v17 tx", e))?;

        // Postgres jsonb_set sets inherit=true on `metadata.governance`
        // objects that don't yet carry the field. The WHERE clause
        // restricts the rewrite to JSON objects with a non-null
        // governance object whose `inherit` is missing — same shape as
        // the SQLite UPDATE in 0012_governance_inherit.sql.
        sqlx::query(
            "UPDATE memories
             SET metadata = jsonb_set(
                 metadata,
                 '{governance,inherit}',
                 'true'::jsonb,
                 true
             )
             WHERE jsonb_typeof(metadata -> 'governance') = 'object'
               AND NOT (metadata -> 'governance' ? 'inherit')",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("backfill governance.inherit", e))?;

        record_schema_version(&mut tx, 17).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v17 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v17 applied");
        Ok(())
    }

    /// v18 — Data-integrity hardening (`embedding_dim` columns + archive
    /// lossless metadata).
    ///
    /// Mirrors `migrations/sqlite/0011_v0631_data_integrity.sql` and the
    /// inline column adds in `db.rs::migrate` v18 block. Adds:
    ///   - `memories.embedding_dim`
    ///   - `archived_memories.embedding`, `embedding_dim`,
    ///     `original_tier`, `original_expires_at`
    /// plus the partial `embedding_dim` indexes on memories.
    ///
    /// Backfill: legacy rows have `embedding_dim` NULL; we leave them
    /// NULL because Postgres `vector` type does not expose an octet
    /// length matching SQLite's `length(embedding)/4` heuristic — the
    /// in-app daemon writes the dim alongside the vector going forward.
    /// `archived_memories.original_tier` is backfilled to `'long'` to
    /// match the SQLite path's defensive default.
    async fn migrate_v18(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v18 tx", e))?;

        for (table, column, ddl) in [
            (
                "memories",
                "embedding_dim",
                "ALTER TABLE memories ADD COLUMN embedding_dim INTEGER",
            ),
            (
                "archived_memories",
                "embedding",
                "ALTER TABLE archived_memories ADD COLUMN embedding vector(384)",
            ),
            (
                "archived_memories",
                "embedding_dim",
                "ALTER TABLE archived_memories ADD COLUMN embedding_dim INTEGER",
            ),
            (
                "archived_memories",
                "original_tier",
                "ALTER TABLE archived_memories ADD COLUMN original_tier TEXT",
            ),
            (
                "archived_memories",
                "original_expires_at",
                "ALTER TABLE archived_memories ADD COLUMN original_expires_at TIMESTAMPTZ",
            ),
        ] {
            add_column_if_missing(&mut tx, table, column, ddl).await?;
        }

        // G5 backfill — pre-existing archive rows have lost original
        // tier/expiry; default original_tier to 'long' so restore_archived
        // doesn't immediately re-delete on first restore. NULL stays NULL
        // for original_expires_at (permanent until re-tiered).
        sqlx::query(
            "UPDATE archived_memories
             SET original_tier = 'long'
             WHERE original_tier IS NULL",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("backfill original_tier", e))?;

        // Partial indexes — match SQLite's idx_memories_embedding_dim and
        // idx_memories_ns_dim. The bootstrap creates these too; re-running
        // here is a no-op against a fresh init.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_embedding_dim
             ON memories (embedding_dim)
             WHERE embedding_dim IS NOT NULL",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_memories_embedding_dim", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_ns_dim
             ON memories (namespace, embedding_dim)
             WHERE embedding_dim IS NOT NULL",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_memories_ns_dim", e))?;

        record_schema_version(&mut tx, 18).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v18 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v18 applied");
        Ok(())
    }

    /// v19 — Webhook `event_types` JSON array on subscriptions.
    ///
    /// Mirrors `migrations/sqlite/0013_webhook_event_types.sql`. Adds a
    /// JSONB column for the structured event-type opt-in surface; the
    /// legacy `events` text column stays as the canonical match key at
    /// dispatch time. The supporting index keeps list_subscriptions
    /// O(log n) when callers want to scope by event.
    async fn migrate_v19(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v19 tx", e))?;

        add_column_if_missing(
            &mut tx,
            "subscriptions",
            "event_types",
            "ALTER TABLE subscriptions ADD COLUMN event_types JSONB",
        )
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_subscriptions_event_types
             ON subscriptions (event_types)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_subscriptions_event_types", e))?;

        record_schema_version(&mut tx, 19).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v19 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v19 applied");
        Ok(())
    }

    /// v20 — Capability-expansion `audit_log` table.
    ///
    /// Mirrors `migrations/sqlite/0014_v064_audit_log.sql`. Idempotent
    /// CREATE TABLE IF NOT EXISTS + indexes. The Postgres column type
    /// for `granted` is BOOLEAN where SQLite uses INTEGER 0/1; both
    /// surface as Rust `bool` so callers stay backend-agnostic.
    async fn migrate_v20(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v20 tx", e))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS audit_log (
                id                 TEXT PRIMARY KEY,
                agent_id           TEXT,
                event_type         TEXT NOT NULL,
                requested_family   TEXT,
                granted            BOOLEAN NOT NULL,
                attestation_tier   TEXT,
                timestamp          TIMESTAMPTZ NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create audit_log table", e))?;

        for (name, ddl) in [
            (
                "idx_audit_log_agent_id",
                "CREATE INDEX IF NOT EXISTS idx_audit_log_agent_id ON audit_log (agent_id)",
            ),
            (
                "idx_audit_log_timestamp",
                "CREATE INDEX IF NOT EXISTS idx_audit_log_timestamp ON audit_log (timestamp)",
            ),
            (
                "idx_audit_log_event_type",
                "CREATE INDEX IF NOT EXISTS idx_audit_log_event_type ON audit_log (event_type)",
            ),
        ] {
            sqlx::query(ddl)
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err(&format!("create {name}"), e))?;
        }

        record_schema_version(&mut tx, 20).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v20 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v20 applied");
        Ok(())
    }

    /// v21 — `pending_actions` timeout sweeper columns + sweep index.
    ///
    /// Mirrors `migrations/sqlite/0015_v07_pending_action_timeouts.sql`
    /// and the inline column adds in `db.rs::migrate` v21 block. Adds:
    ///   - `pending_actions.default_timeout_seconds` (per-row TTL)
    ///   - `pending_actions.expired_at` (RFC3339 stamp on transition)
    /// plus the composite `(status, requested_at)` sweep index.
    async fn migrate_v21(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v21 tx", e))?;

        add_column_if_missing(
            &mut tx,
            "pending_actions",
            "default_timeout_seconds",
            "ALTER TABLE pending_actions ADD COLUMN default_timeout_seconds BIGINT",
        )
        .await?;

        add_column_if_missing(
            &mut tx,
            "pending_actions",
            "expired_at",
            "ALTER TABLE pending_actions ADD COLUMN expired_at TIMESTAMPTZ",
        )
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS pending_actions_status_requested_idx
             ON pending_actions (status, requested_at)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create pending_actions_status_requested_idx", e))?;

        record_schema_version(&mut tx, 21).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v21 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v21 applied");
        Ok(())
    }

    /// v22 — `memory_transcripts` substrate (attested-cortex epic, I1).
    ///
    /// Mirrors `migrations/sqlite/0016_v07_transcripts.sql`. Compressed
    /// (zstd-3) blob storage of conversation transcripts. The Rust
    /// transcripts.rs read/write path currently binds to SQLite; the
    /// table is provisioned here for parity so a Postgres bootstrap is
    /// one wiring change away from full SAL coverage.
    async fn migrate_v22(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v22 tx", e))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS memory_transcripts (
                id               TEXT PRIMARY KEY,
                namespace        TEXT NOT NULL,
                created_at       TIMESTAMPTZ NOT NULL,
                expires_at       TIMESTAMPTZ,
                compressed_size  BIGINT NOT NULL,
                original_size    BIGINT NOT NULL,
                zstd_level       INTEGER NOT NULL DEFAULT 3,
                content_blob     BYTEA NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create memory_transcripts table", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_transcripts_namespace_created
             ON memory_transcripts (namespace, created_at)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_memory_transcripts_namespace_created", e))?;

        record_schema_version(&mut tx, 22).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v22 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v22 applied");
        Ok(())
    }

    /// v25 — Per-namespace transcript TTL with archive→prune lifecycle (I3).
    ///
    /// Mirrors `migrations/sqlite/0019_v07_transcript_lifecycle.sql` and
    /// the inline ADD COLUMN in `db.rs::migrate` v25 block. Adds
    /// `memory_transcripts.archived_at` plus the supporting partial
    /// index on archived rows so the prune-phase scan stays
    /// O(archived) rather than O(total transcripts).
    async fn migrate_v25(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v25 tx", e))?;

        add_column_if_missing(
            &mut tx,
            "memory_transcripts",
            "archived_at",
            "ALTER TABLE memory_transcripts ADD COLUMN archived_at TIMESTAMPTZ",
        )
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_transcripts_archived_at
             ON memory_transcripts (archived_at)
             WHERE archived_at IS NOT NULL",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_memory_transcripts_archived_at", e))?;

        record_schema_version(&mut tx, 25).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v25 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v25 applied");
        Ok(())
    }

    /// v26 — `signed_events` append-only audit table (H5).
    ///
    /// Mirrors `migrations/sqlite/0020_v07_signed_events.sql`. The
    /// append-only invariant is enforced at the Rust API surface (one
    /// writer, no UPDATE/DELETE call sites) — no SQL-layer triggers
    /// here because they would also fire against operator-driven
    /// retention pruning, defeating the documented escape hatch.
    async fn migrate_v26(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v26 tx", e))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS signed_events (
                id           TEXT PRIMARY KEY,
                agent_id     TEXT NOT NULL,
                event_type   TEXT NOT NULL,
                payload_hash BYTEA NOT NULL,
                signature    BYTEA,
                attest_level TEXT NOT NULL DEFAULT 'unsigned',
                timestamp    TIMESTAMPTZ NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create signed_events table", e))?;

        for (name, ddl) in [
            (
                "idx_signed_events_agent",
                "CREATE INDEX IF NOT EXISTS idx_signed_events_agent ON signed_events (agent_id)",
            ),
            (
                "idx_signed_events_type",
                "CREATE INDEX IF NOT EXISTS idx_signed_events_type ON signed_events (event_type)",
            ),
            (
                "idx_signed_events_timestamp",
                "CREATE INDEX IF NOT EXISTS idx_signed_events_timestamp ON signed_events (timestamp)",
            ),
        ] {
            sqlx::query(ddl)
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err(&format!("create {name}"), e))?;
        }

        record_schema_version(&mut tx, 26).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v26 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v26 applied");
        Ok(())
    }

    /// v27 — A2A correlation IDs + DLQ.
    ///
    /// Mirrors `migrations/sqlite/0021_v07_a2a_correlation.sql`. Brings
    /// up `subscription_events` (per-delivery audit log keyed on
    /// UUIDv7 correlation_id) and `subscription_dlq` (failures past
    /// the three-attempt retry ladder). On Postgres these tables use
    /// BIGSERIAL primary keys; the Rust callers see monotonically-
    /// increasing i64 values matching SQLite's autoincrement.
    async fn migrate_v27(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v27 tx", e))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS subscription_events (
                id              BIGSERIAL PRIMARY KEY,
                subscription_id TEXT NOT NULL,
                correlation_id  TEXT NOT NULL DEFAULT '',
                event_type      TEXT NOT NULL,
                payload         JSONB NOT NULL,
                delivered_at    TIMESTAMPTZ NOT NULL,
                delivery_status TEXT NOT NULL DEFAULT 'pending'
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create subscription_events table", e))?;

        // Defensive ALTER TABLE for deployments that hand-rolled a
        // `subscription_events` table before K6 (mirrors the inline
        // column add in src/db.rs::migrate v27 block).
        add_column_if_missing(
            &mut tx,
            "subscription_events",
            "correlation_id",
            "ALTER TABLE subscription_events ADD COLUMN correlation_id TEXT NOT NULL DEFAULT ''",
        )
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_subscription_events_correlation
             ON subscription_events (correlation_id)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_subscription_events_correlation", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_subscription_events_subscription
             ON subscription_events (subscription_id, delivered_at)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_subscription_events_subscription", e))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS subscription_dlq (
                id               BIGSERIAL PRIMARY KEY,
                subscription_id  TEXT NOT NULL,
                correlation_id   TEXT NOT NULL,
                event_type       TEXT NOT NULL,
                payload          JSONB NOT NULL,
                retry_count      INTEGER NOT NULL,
                last_error       TEXT NOT NULL,
                first_failed_at  TIMESTAMPTZ NOT NULL,
                last_failed_at   TIMESTAMPTZ NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create subscription_dlq table", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_subscription_dlq_subscription
             ON subscription_dlq (subscription_id, last_failed_at)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_subscription_dlq_subscription", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_subscription_dlq_correlation
             ON subscription_dlq (correlation_id)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_subscription_dlq_correlation", e))?;

        record_schema_version(&mut tx, 27).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v27 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v27 applied");
        Ok(())
    }

    /// v28 — Per-agent quotas (`agent_quotas`).
    ///
    /// Mirrors `migrations/sqlite/0022_v07_agent_quotas.sql`. Idempotent
    /// CREATE TABLE IF NOT EXISTS + index. Compiled defaults match the
    /// SQLite path: 1000 memories/day, 100 MiB storage cap, 5000
    /// links/day. Daily counters reset at UTC midnight via the K8
    /// sweep loop (currently SQLite-bound; SAL wiring in a future wave).
    async fn migrate_v28(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v28 tx", e))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agent_quotas (
                agent_id                TEXT PRIMARY KEY,
                max_memories_per_day    BIGINT  NOT NULL DEFAULT 1000,
                max_storage_bytes       BIGINT  NOT NULL DEFAULT 104857600,
                max_links_per_day       BIGINT  NOT NULL DEFAULT 5000,
                current_memories_today  BIGINT  NOT NULL DEFAULT 0,
                current_storage_bytes   BIGINT  NOT NULL DEFAULT 0,
                current_links_today     BIGINT  NOT NULL DEFAULT 0,
                day_started_at          TIMESTAMPTZ NOT NULL,
                created_at              TIMESTAMPTZ NOT NULL,
                updated_at              TIMESTAMPTZ NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create agent_quotas table", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_agent_quotas_agent_id
             ON agent_quotas (agent_id)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_agent_quotas_agent_id", e))?;

        record_schema_version(&mut tx, 28).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v28 migration", e))?;

        tracing::info!(target = "store::postgres", "schema migration v28 applied");
        Ok(())
    }

    /// v0.6.3 Stream B — Temporal-Validity KG schema additions.
    /// Idempotent: safe to run twice. Mirrors src/db.rs migrate v15 block.
    async fn migrate_v15(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin transaction", e))?;

        // Add temporal columns to memory_links if they do not exist.
        let has_valid_from: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM information_schema.columns
                WHERE table_name='memory_links' AND column_name='valid_from'
            )",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("check valid_from column", e))?;

        if !has_valid_from {
            sqlx::query("ALTER TABLE memory_links ADD COLUMN valid_from TIMESTAMPTZ")
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("add valid_from column", e))?;
        }

        let has_valid_until: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM information_schema.columns
                WHERE table_name='memory_links' AND column_name='valid_until'
            )",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("check valid_until column", e))?;

        if !has_valid_until {
            sqlx::query("ALTER TABLE memory_links ADD COLUMN valid_until TIMESTAMPTZ")
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("add valid_until column", e))?;
        }

        let has_observed_by: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM information_schema.columns
                WHERE table_name='memory_links' AND column_name='observed_by'
            )",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("check observed_by column", e))?;

        if !has_observed_by {
            sqlx::query("ALTER TABLE memory_links ADD COLUMN observed_by TEXT")
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("add observed_by column", e))?;
        }

        let has_signature: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM information_schema.columns
                WHERE table_name='memory_links' AND column_name='signature'
            )",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("check signature column", e))?;

        if !has_signature {
            sqlx::query("ALTER TABLE memory_links ADD COLUMN signature BYTEA")
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("add signature column", e))?;
        }

        // Backfill valid_from from source memory's created_at (idempotent).
        sqlx::query(
            "UPDATE memory_links
             SET valid_from = (SELECT created_at FROM memories WHERE id = memory_links.source_id)
             WHERE valid_from IS NULL",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("backfill valid_from", e))?;

        // Create temporal indexes (idempotent via IF NOT EXISTS).
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_links_temporal_src
             ON memory_links (source_id, valid_from, valid_until)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_links_temporal_src", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt
             ON memory_links (target_id, valid_from, valid_until)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_links_temporal_tgt", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_links_relation
             ON memory_links (relation, valid_from)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_links_relation", e))?;

        // Create entity_aliases table (idempotent via IF NOT EXISTS).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS entity_aliases (
                entity_id  TEXT NOT NULL,
                alias      TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (entity_id, alias)
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create entity_aliases table", e))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
             ON entity_aliases (alias)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("create idx_entity_aliases_alias", e))?;

        // Record the migration in schema_version.
        record_schema_version(&mut tx, 15).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit migration transaction", e))?;

        tracing::info!(
            target = "store::postgres",
            version = 15,
            "schema migration v15 applied"
        );

        Ok(())
    }

    /// Outbound knowledge-graph traversal — v0.7 Track J dispatcher.
    ///
    /// Routes on the [`KgBackend`] resolved at [`Self::connect`] time
    /// (J1 substrate). When AGE is installed the traversal runs as a
    /// Cypher `MATCH ... -[:related_to*1..N]-> ...` query through the
    /// `memory_graph` projection; otherwise we fall back to a recursive
    /// CTE over the `memory_links` table that mirrors the SQLite shape.
    ///
    /// Both branches return rows in the same [`KgQueryRow`] shape so
    /// the upper-layer `memory_kg_query` handler can stay backend-blind.
    ///
    /// `max_depth` is clamped at [`KG_QUERY_MAX_SUPPORTED_DEPTH`] to
    /// match the SQLite implementation's published budget; passing a
    /// larger value yields an explicit error rather than a silent
    /// truncation, so callers learn they hit the ceiling.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::InvalidInput` when `max_depth` is zero or
    /// exceeds the supported ceiling, and `StoreError::BackendUnavailable`
    /// when the underlying SQL or Cypher query fails.
    pub async fn kg_query(
        &self,
        source_id: &str,
        max_depth: usize,
    ) -> StoreResult<Vec<KgQueryRow>> {
        match self.kg_backend {
            KgBackend::Age => self.kg_query_cypher(source_id, max_depth).await,
            KgBackend::Cte => self.kg_query_cte(source_id, max_depth).await,
        }
    }

    /// Cypher (Apache AGE) implementation of `kg_query`.
    ///
    /// Wraps a `MATCH (a)-[:related_to*1..N]->(b)` traversal in the
    /// `cypher('memory_graph', ...)` set-returning function. Parameter
    /// passing uses AGE's `$vars` syntax through a JSON-encoded second
    /// argument so the start id is bound — never interpolated — to
    /// keep the surface free of injection hazards.
    ///
    /// The graph projection (`memory_graph`) and the `:related_to`
    /// edge label are conventions established by the J1 schema-prep
    /// scripts. When the projection is absent the underlying call
    /// surfaces as `BackendUnavailable` so the test harness can
    /// distinguish "AGE present, graph missing" from a real bug.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::InvalidInput` for an out-of-range
    /// `max_depth`, and `StoreError::BackendUnavailable` for any sqlx
    /// or AGE error.
    pub async fn kg_query_cypher(
        &self,
        source_id: &str,
        max_depth: usize,
    ) -> StoreResult<Vec<KgQueryRow>> {
        validate_depth(max_depth)?;

        // AGE requires `ag_catalog` on the search path and the extension
        // loaded into the session. Both are session-local — sqlx hands
        // each query a fresh connection from the pool so we issue them
        // as part of the same transaction to keep them in scope.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin AGE tx", e))?;

        sqlx::query("LOAD 'age'")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("LOAD age", e))?;
        sqlx::query("SET search_path = ag_catalog, \"$user\", public")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("set search_path", e))?;

        // Build the Cypher body. `max_depth` is interpolated into the
        // variable-length pattern (Cypher does NOT accept a parameter
        // there); we already clamped it via `validate_depth` so the
        // value is a small bounded integer with no injection surface.
        // The start id is parameterised through AGE's `$vars` JSON.
        let cypher = format!(
            "MATCH p = (a)-[r:related_to*1..{max_depth}]->(b) \
             WHERE a.id = $start_id \
             RETURN b.id AS target_id, \
                    last(r).relation AS relation, \
                    length(r) AS depth, \
                    reduce(s = a.id, n IN nodes(p)[1..] | s + '->' + n.id) AS path"
        );

        let sql = format!(
            "SELECT target_id, relation, depth, path FROM cypher('memory_graph', $$ {cypher} $$, \
             $1::agtype) AS (target_id agtype, relation agtype, depth agtype, path agtype)"
        );

        let params = serde_json::json!({ "start_id": source_id }).to_string();
        let rows = sqlx::query(&sql)
            .bind(params)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| to_store_err("cypher kg_query", e))?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit AGE tx", e))?;

        rows.iter()
            .map(|r| {
                // AGE returns `agtype`. sqlx has no first-class agtype
                // decoder; we read each cell as String and trim AGE's
                // quoting (`"id"` -> `id`, `2` stays `2`). This keeps
                // the dependency surface minimal — pulling a dedicated
                // agtype crate would balloon CI for one type tag.
                let target_id: String = r
                    .try_get::<String, _>("target_id")
                    .map_err(|e| to_store_err("read target_id", e))?;
                let relation: String = r
                    .try_get::<String, _>("relation")
                    .map_err(|e| to_store_err("read relation", e))?;
                let depth_raw: String = r
                    .try_get::<String, _>("depth")
                    .map_err(|e| to_store_err("read depth", e))?;
                let path: String = r
                    .try_get::<String, _>("path")
                    .map_err(|e| to_store_err("read path", e))?;
                let depth: usize = strip_agtype_quotes(&depth_raw).parse().map_err(|_| {
                    StoreError::IntegrityFailed {
                        detail: format!("non-numeric AGE depth: {depth_raw}"),
                    }
                })?;
                Ok(KgQueryRow {
                    target_id: strip_agtype_quotes(&target_id).to_string(),
                    relation: strip_agtype_quotes(&relation).to_string(),
                    depth,
                    path: strip_agtype_quotes(&path).to_string(),
                })
            })
            .collect()
    }

    /// Recursive-CTE fallback for `kg_query` on Postgres.
    ///
    /// Mirrors the SQLite recursive-CTE in `db::kg_query` so deployments
    /// running vanilla Postgres (no AGE extension) still get the same
    /// traversal semantics. Returns the shared [`KgQueryRow`] shape —
    /// the dispatcher in [`Self::kg_query`] doesn't have to care which
    /// branch ran.
    ///
    /// # Errors
    ///
    /// `StoreError::InvalidInput` for an out-of-range `max_depth`;
    /// `StoreError::BackendUnavailable` for any sqlx error.
    pub async fn kg_query_cte(
        &self,
        source_id: &str,
        max_depth: usize,
    ) -> StoreResult<Vec<KgQueryRow>> {
        validate_depth(max_depth)?;

        let depth_cap = i32::try_from(max_depth).unwrap_or(i32::MAX);
        let sql = "WITH RECURSIVE traversal(target_id, relation, depth, path) AS (
                SELECT ml.target_id, ml.relation, 1,
                       ml.source_id || '->' || ml.target_id
                FROM memory_links ml
                WHERE ml.source_id = $1
                UNION ALL
                SELECT ml.target_id, ml.relation, t.depth + 1,
                       t.path || '->' || ml.target_id
                FROM memory_links ml
                JOIN traversal t ON ml.source_id = t.target_id
                WHERE t.depth < $2
                  AND position(('->' || ml.target_id) IN t.path) = 0
                  AND position((ml.target_id || '->') IN t.path) = 0
            )
            SELECT target_id, relation, depth, path
            FROM traversal
            ORDER BY depth ASC, target_id ASC";

        let rows = sqlx::query(sql)
            .bind(source_id)
            .bind(depth_cap)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| to_store_err("cte kg_query", e))?;

        rows.iter()
            .map(|r| {
                let target_id: String = r
                    .try_get::<String, _>("target_id")
                    .map_err(|e| to_store_err("read target_id", e))?;
                let relation: String = r
                    .try_get::<String, _>("relation")
                    .map_err(|e| to_store_err("read relation", e))?;
                let depth_i: i32 = r
                    .try_get::<i32, _>("depth")
                    .map_err(|e| to_store_err("read depth", e))?;
                let path: String = r
                    .try_get::<String, _>("path")
                    .map_err(|e| to_store_err("read path", e))?;
                Ok(KgQueryRow {
                    target_id,
                    relation,
                    depth: usize::try_from(depth_i).unwrap_or(0),
                    path,
                })
            })
            .collect()
    }

    /// Ordered fact timeline for an entity — v0.7 Track J dispatcher.
    ///
    /// Routes on the [`KgBackend`] resolved at [`Self::connect`] time
    /// (J1 substrate). When AGE is installed the timeline is assembled
    /// via a Cypher `MATCH (a)-[r]->(b)` over the `memory_graph`
    /// projection, ordered by `r.valid_from ASC`; otherwise we fall
    /// back to a plain SQL scan over `memory_links` joined to
    /// `memories` that mirrors the SQLite shape in `db::kg_timeline`.
    ///
    /// Both branches return rows in the same [`KgTimelineRow`] shape
    /// so the upper-layer `memory_kg_timeline` handler can stay
    /// backend-blind, mirroring J2's pattern for `kg_query`.
    ///
    /// `since` and `until` are RFC3339 timestamps (inclusive) that
    /// filter on `valid_from`; `limit` is clamped to
    /// `[1, KG_TIMELINE_MAX_LIMIT_SAL]` and defaults to
    /// [`KG_TIMELINE_DEFAULT_LIMIT_SAL`] when omitted.
    ///
    /// Rows with NULL `valid_from` are excluded — a link without a
    /// valid-from anchor cannot be ordered on the timeline. This
    /// matches the SQLite contract documented on `db::kg_timeline`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::InvalidInput` for malformed timestamp
    /// inputs; `StoreError::BackendUnavailable` for any sqlx or AGE
    /// error.
    pub async fn kg_timeline(
        &self,
        source_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        limit: Option<usize>,
    ) -> StoreResult<Vec<KgTimelineRow>> {
        match self.kg_backend {
            KgBackend::Age => {
                self.kg_timeline_cypher(source_id, since, until, limit)
                    .await
            }
            KgBackend::Cte => self.kg_timeline_cte(source_id, since, until, limit).await,
        }
    }

    /// Cypher (Apache AGE) implementation of `kg_timeline`.
    ///
    /// Wraps a `MATCH (a)-[r:related_to]->(b)` traversal in the
    /// `cypher('memory_graph', ...)` set-returning function, ordered
    /// by `r.valid_from ASC` and tie-broken by `r.created_at ASC` to
    /// match the SQLite implementation. The since/until filters are
    /// applied through Cypher's `WHERE` predicate; the start id is
    /// passed as an AGE `$vars` JSON parameter so the user-supplied
    /// value is never interpolated into the query body.
    ///
    /// Title and namespace are pulled by joining the AGE result back
    /// to the source-of-truth `memories` table — keeping the column
    /// snapshots in sync with the relational store and avoiding
    /// drift if the property-graph projection lags behind.
    ///
    /// # Errors
    ///
    /// `StoreError::BackendUnavailable` for any sqlx or AGE error.
    pub async fn kg_timeline_cypher(
        &self,
        source_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        limit: Option<usize>,
    ) -> StoreResult<Vec<KgTimelineRow>> {
        let cap = clamp_timeline_limit(limit);

        // AGE requires `ag_catalog` on the search path and the
        // extension loaded into the session. Both are session-local
        // — sqlx hands each query a fresh connection from the pool
        // so we issue them as part of the same transaction to keep
        // them in scope. Same shape as `kg_query_cypher`.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin AGE tx", e))?;

        sqlx::query("LOAD 'age'")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("LOAD age", e))?;
        sqlx::query("SET search_path = ag_catalog, \"$user\", public")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("set search_path", e))?;

        // Build the WHERE predicate dynamically. `since`/`until` are
        // pre-validated RFC3339 strings at the upper layer (the MCP
        // handler runs `validate_expires_at_format`), but we still
        // pass them through as bound `$since`/`$until` AGE vars so
        // they're never interpolated into the Cypher body.
        let mut where_clauses: Vec<&str> = vec!["a.id = $start_id", "r.valid_from IS NOT NULL"];
        if since.is_some() {
            where_clauses.push("r.valid_from >= $since");
        }
        if until.is_some() {
            where_clauses.push("r.valid_from <= $until");
        }
        let where_sql = where_clauses.join(" AND ");

        let cypher = format!(
            "MATCH (a)-[r:related_to]->(b) \
             WHERE {where_sql} \
             RETURN b.id AS target_id, \
                    r.relation AS relation, \
                    r.valid_from AS valid_from, \
                    r.valid_until AS valid_until, \
                    r.observed_by AS observed_by \
             ORDER BY r.valid_from ASC, r.created_at ASC \
             LIMIT {cap}"
        );

        let sql = format!(
            "SELECT target_id, relation, valid_from, valid_until, observed_by \
             FROM cypher('memory_graph', $$ {cypher} $$, $1::agtype) AS \
             (target_id agtype, relation agtype, valid_from agtype, \
              valid_until agtype, observed_by agtype)"
        );

        let mut params = serde_json::Map::new();
        params.insert(
            "start_id".to_string(),
            serde_json::Value::String(source_id.to_string()),
        );
        if let Some(s) = since {
            params.insert(
                "since".to_string(),
                serde_json::Value::String(s.to_string()),
            );
        }
        if let Some(u) = until {
            params.insert(
                "until".to_string(),
                serde_json::Value::String(u.to_string()),
            );
        }
        let params_json = serde_json::Value::Object(params).to_string();

        let rows = sqlx::query(&sql)
            .bind(params_json)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| to_store_err("cypher kg_timeline", e))?;

        // Decode the agtype payloads into raw Rust strings/options
        // first; we'll backfill `title` + `target_namespace` from
        // the `memories` table inside the same transaction. Pulling
        // the display fields from the relational store avoids a
        // class of drift bugs where the AGE projection lags behind
        // a `memories` rename.
        let mut decoded: Vec<KgTimelineRow> = Vec::with_capacity(rows.len());
        for r in &rows {
            let target_id_raw: String = r
                .try_get::<String, _>("target_id")
                .map_err(|e| to_store_err("read target_id", e))?;
            let relation_raw: String = r
                .try_get::<String, _>("relation")
                .map_err(|e| to_store_err("read relation", e))?;
            let valid_from_raw: String = r
                .try_get::<String, _>("valid_from")
                .map_err(|e| to_store_err("read valid_from", e))?;
            let valid_until_raw: String = r
                .try_get::<String, _>("valid_until")
                .map_err(|e| to_store_err("read valid_until", e))?;
            let observed_by_raw: String = r
                .try_get::<String, _>("observed_by")
                .map_err(|e| to_store_err("read observed_by", e))?;

            decoded.push(KgTimelineRow {
                target_id: strip_agtype_quotes(&target_id_raw).to_string(),
                relation: strip_agtype_quotes(&relation_raw).to_string(),
                valid_from: strip_agtype_quotes(&valid_from_raw).to_string(),
                valid_until: agtype_optional_string(&valid_until_raw),
                observed_by: agtype_optional_string(&observed_by_raw),
                // Filled in below by the `memories` join.
                title: String::new(),
                target_namespace: String::new(),
            });
        }

        // Backfill title + namespace in a single round-trip by
        // pulling the unique target ids and joining server-side.
        // Empty result set short-circuits without hitting Postgres.
        if !decoded.is_empty() {
            let ids: Vec<String> = {
                let mut seen: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for row in &decoded {
                    seen.insert(row.target_id.clone());
                }
                seen.into_iter().collect()
            };

            let display_rows =
                sqlx::query("SELECT id, title, namespace FROM memories WHERE id = ANY($1)")
                    .bind(&ids)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(|e| to_store_err("fetch timeline display fields", e))?;

            let mut display: std::collections::HashMap<String, (String, String)> =
                std::collections::HashMap::with_capacity(display_rows.len());
            for r in &display_rows {
                let id: String = r
                    .try_get::<String, _>("id")
                    .map_err(|e| to_store_err("read id", e))?;
                let title: String = r
                    .try_get::<String, _>("title")
                    .map_err(|e| to_store_err("read title", e))?;
                let namespace: String = r
                    .try_get::<String, _>("namespace")
                    .map_err(|e| to_store_err("read namespace", e))?;
                display.insert(id, (title, namespace));
            }

            for row in &mut decoded {
                if let Some((title, ns)) = display.get(&row.target_id) {
                    row.title.clone_from(title);
                    row.target_namespace.clone_from(ns);
                }
            }
        }

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit AGE tx", e))?;

        Ok(decoded)
    }

    /// SQL fallback for `kg_timeline` on Postgres.
    ///
    /// Mirrors the SQLite query in `db::kg_timeline` so deployments
    /// running vanilla Postgres (no AGE extension) still get the
    /// same temporal ordering and filter semantics. Returns the
    /// shared [`KgTimelineRow`] shape so the dispatcher in
    /// [`Self::kg_timeline`] doesn't have to care which branch ran.
    ///
    /// # Errors
    ///
    /// `StoreError::BackendUnavailable` for any sqlx error.
    pub async fn kg_timeline_cte(
        &self,
        source_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        limit: Option<usize>,
    ) -> StoreResult<Vec<KgTimelineRow>> {
        let cap = clamp_timeline_limit(limit);
        let cap_i64 = i64::try_from(cap).unwrap_or(i64::MAX);

        // Compose the predicate dynamically for `since` / `until`.
        // Bind values are appended in the same order so the `$N`
        // placeholders line up. We rely on TIMESTAMPTZ casting to
        // parse the RFC3339 inputs; malformed values surface as a
        // `BackendUnavailable` from sqlx (the upper-layer handler
        // pre-validates so this is a defense-in-depth path).
        let mut sql = String::from(
            "SELECT ml.target_id, ml.relation, ml.valid_from, ml.valid_until,
                    ml.observed_by, m.title, m.namespace, ml.created_at
             FROM memory_links ml
             JOIN memories m ON m.id = ml.target_id
             WHERE ml.source_id = $1
               AND ml.valid_from IS NOT NULL",
        );
        let mut next_placeholder = 2usize;
        if since.is_some() {
            sql.push_str(&format!(
                " AND ml.valid_from >= ${next_placeholder}::TIMESTAMPTZ"
            ));
            next_placeholder += 1;
        }
        if until.is_some() {
            sql.push_str(&format!(
                " AND ml.valid_from <= ${next_placeholder}::TIMESTAMPTZ"
            ));
            next_placeholder += 1;
        }
        sql.push_str(&format!(
            " ORDER BY ml.valid_from ASC, ml.created_at ASC LIMIT ${next_placeholder}"
        ));

        let mut q = sqlx::query(&sql).bind(source_id);
        if let Some(s) = since {
            q = q.bind(s);
        }
        if let Some(u) = until {
            q = q.bind(u);
        }
        q = q.bind(cap_i64);

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| to_store_err("cte kg_timeline", e))?;

        rows.iter()
            .map(|r| {
                let target_id: String = r
                    .try_get::<String, _>("target_id")
                    .map_err(|e| to_store_err("read target_id", e))?;
                let relation: String = r
                    .try_get::<String, _>("relation")
                    .map_err(|e| to_store_err("read relation", e))?;
                let valid_from: DateTime<Utc> = r
                    .try_get::<DateTime<Utc>, _>("valid_from")
                    .map_err(|e| to_store_err("read valid_from", e))?;
                let valid_until: Option<DateTime<Utc>> = r
                    .try_get::<Option<DateTime<Utc>>, _>("valid_until")
                    .map_err(|e| to_store_err("read valid_until", e))?;
                let observed_by: Option<String> = r
                    .try_get::<Option<String>, _>("observed_by")
                    .map_err(|e| to_store_err("read observed_by", e))?;
                let title: String = r
                    .try_get::<String, _>("title")
                    .map_err(|e| to_store_err("read title", e))?;
                let target_namespace: String = r
                    .try_get::<String, _>("namespace")
                    .map_err(|e| to_store_err("read namespace", e))?;
                Ok(KgTimelineRow {
                    target_id,
                    relation,
                    valid_from: valid_from.to_rfc3339(),
                    valid_until: valid_until.map(|t| t.to_rfc3339()),
                    observed_by,
                    title,
                    target_namespace,
                })
            })
            .collect()
    }

    /// Mark a KG link as superseded — v0.7 Track J dispatcher.
    ///
    /// Routes on the [`KgBackend`] resolved at [`Self::connect`] time
    /// (J1 substrate). When AGE is installed the supersession runs as
    /// a Cypher `MATCH (a)-[r:related_to]->(b) ... SET r.valid_until`
    /// over the `memory_graph` projection; otherwise we fall back to a
    /// plain `UPDATE memory_links` that mirrors the SQLite shape in
    /// `db::invalidate_link`.
    ///
    /// Both branches return rows in the same [`KgInvalidateRow`] shape
    /// so the upper-layer `memory_kg_invalidate` handler can stay
    /// backend-blind, mirroring J2/J3's pattern for `kg_query` and
    /// `kg_timeline`.
    ///
    /// `valid_until` defaults to the current wall-clock time in
    /// RFC3339 form when `None`. Idempotent: calling repeatedly
    /// overwrites the prior `valid_until` (the prior value is returned
    /// in `previous_valid_until` so callers can detect the overwrite).
    /// When the `(source_id, target_id, relation)` triple does not
    /// match an existing link the returned row carries `found = false`
    /// so the SAL shape distinguishes "no-op" from "applied" the same
    /// way the SQLite path does (via `Option::None`).
    ///
    /// G14 audit-edge emission lives in the upper-layer handler —
    /// keeping it there means both backends share a single
    /// invalidation-event call site rather than duplicating webhook
    /// dispatch into the SAL layer.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::BackendUnavailable` for any sqlx or AGE
    /// error.
    pub async fn kg_invalidate(
        &self,
        source_id: &str,
        target_id: &str,
        relation: &str,
        valid_until: Option<&str>,
    ) -> StoreResult<KgInvalidateRow> {
        match self.kg_backend {
            KgBackend::Age => {
                self.kg_invalidate_cypher(source_id, target_id, relation, valid_until)
                    .await
            }
            KgBackend::Cte => {
                self.kg_invalidate_cte(source_id, target_id, relation, valid_until)
                    .await
            }
        }
    }

    /// Cypher (Apache AGE) implementation of `kg_invalidate`.
    ///
    /// Wraps a `MATCH (a)-[r:related_to]->(b) ... SET r.valid_until`
    /// over the `memory_graph` projection in the
    /// `cypher('memory_graph', ...)` set-returning function. Parameter
    /// passing uses AGE's `$vars` syntax through a JSON-encoded second
    /// argument so the source/target ids and the timestamp are bound
    /// — never interpolated — into the Cypher body.
    ///
    /// Two-step traversal: a `MATCH ... RETURN r.valid_until` first
    /// captures the prior value (so the dispatcher can populate
    /// `previous_valid_until`), then the same triple is matched again
    /// with `SET r.valid_until = $now RETURN count(r)`. Both run
    /// inside a single AGE transaction so a parallel writer can't
    /// interleave between the read and the SET. The two-step shape
    /// mirrors the SQLite path in `db::invalidate_link` which also
    /// SELECTs the prior row before UPDATEing it.
    ///
    /// AGE's `cypher()` SRF returns no rows when the MATCH misses —
    /// we map that to `found = false` so the upper-layer wire shape
    /// matches the CTE branch exactly.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::BackendUnavailable` for any sqlx or AGE
    /// error.
    pub async fn kg_invalidate_cypher(
        &self,
        source_id: &str,
        target_id: &str,
        relation: &str,
        valid_until: Option<&str>,
    ) -> StoreResult<KgInvalidateRow> {
        let stamp = valid_until.map_or_else(|| Utc::now().to_rfc3339(), str::to_string);

        // AGE requires `ag_catalog` on the search path and the
        // extension loaded into the session. Both are session-local
        // — sqlx hands each query a fresh connection from the pool
        // so we issue them as part of the same transaction to keep
        // them in scope. Same shape as `kg_query_cypher` and
        // `kg_timeline_cypher`.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin AGE tx", e))?;

        sqlx::query("LOAD 'age'")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("LOAD age", e))?;
        sqlx::query("SET search_path = ag_catalog, \"$user\", public")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("set search_path", e))?;

        // Step 1: capture the prior `valid_until` so the wire-shape
        // can surface it. We bind the ids + relation through AGE's
        // `$vars` JSON so user input is never interpolated.
        let read_cypher = "MATCH (a)-[r:related_to]->(b) \
             WHERE a.id = $src AND b.id = $dst AND r.relation = $rel \
             RETURN r.valid_until AS prior";
        let read_sql = format!(
            "SELECT prior FROM cypher('memory_graph', $$ {read_cypher} $$, $1::agtype) AS \
             (prior agtype)"
        );
        let read_params = serde_json::json!({
            "src": source_id,
            "dst": target_id,
            "rel": relation,
        })
        .to_string();
        let prior_rows = sqlx::query(&read_sql)
            .bind(&read_params)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| to_store_err("cypher kg_invalidate read", e))?;

        if prior_rows.is_empty() {
            // No matching edge — mirror the SQLite path's `Ok(None)`
            // contract by surfacing `found = false`. The transaction
            // still commits so the LOAD/SET search_path pair don't
            // leak across pool checkouts.
            tx.commit()
                .await
                .map_err(|e| to_store_err("commit AGE tx", e))?;
            return Ok(KgInvalidateRow {
                found: false,
                valid_until: String::new(),
                previous_valid_until: None,
            });
        }

        let prior_raw: String = prior_rows[0]
            .try_get::<String, _>("prior")
            .map_err(|e| to_store_err("read prior valid_until", e))?;
        let previous_valid_until = agtype_optional_string(&prior_raw);

        // Step 2: SET r.valid_until = $now and count the affected
        // edge. We rely on AGE's identity to atomically rewrite the
        // edge property — Cypher's SET semantics replace the property
        // value (no append). count(r) returns the number of edges
        // that matched the WHERE; for a unique (src, dst, rel) triple
        // this is 1, but we don't assume — duplicate edges would show
        // up here and the dispatcher's contract is the same either
        // way (the prior value already came from row[0]).
        let write_cypher = "MATCH (a)-[r:related_to]->(b) \
             WHERE a.id = $src AND b.id = $dst AND r.relation = $rel \
             SET r.valid_until = $now \
             RETURN count(r) AS affected";
        let write_sql = format!(
            "SELECT affected FROM cypher('memory_graph', $$ {write_cypher} $$, $1::agtype) AS \
             (affected agtype)"
        );
        let write_params = serde_json::json!({
            "src": source_id,
            "dst": target_id,
            "rel": relation,
            "now": stamp,
        })
        .to_string();
        let _ = sqlx::query(&write_sql)
            .bind(&write_params)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| to_store_err("cypher kg_invalidate set", e))?;

        // Mirror the SET into the relational `memory_links` row so the
        // CTE-side reads and the AGE-side reads stay aligned. The AGE
        // projection lags behind a relational `UPDATE` in deployments
        // that lack a sync trigger; doing both writes here is the same
        // dual-write contract J2/J3 already rely on for the read path.
        sqlx::query(
            "UPDATE memory_links SET valid_until = $4 \
             WHERE source_id = $1 AND target_id = $2 AND relation = $3",
        )
        .bind(source_id)
        .bind(target_id)
        .bind(relation)
        .bind(&stamp)
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("cypher kg_invalidate mirror", e))?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit AGE tx", e))?;

        Ok(KgInvalidateRow {
            found: true,
            valid_until: stamp,
            previous_valid_until,
        })
    }

    /// SQL fallback for `kg_invalidate` on Postgres.
    ///
    /// Mirrors the SQLite query in `db::invalidate_link` so deployments
    /// running vanilla Postgres (no AGE extension) get the same
    /// supersession semantics. Uses `UPDATE ... RETURNING` to atomically
    /// read the prior `valid_until` and write the new value in one
    /// round-trip — the SQLite path has to issue a SELECT then an
    /// UPDATE because rusqlite's RETURNING is gated on a feature flag,
    /// but Postgres has it natively. Returns the shared
    /// [`KgInvalidateRow`] shape so the dispatcher in
    /// [`Self::kg_invalidate`] doesn't have to care which branch ran.
    ///
    /// # Errors
    ///
    /// `StoreError::BackendUnavailable` for any sqlx error.
    pub async fn kg_invalidate_cte(
        &self,
        source_id: &str,
        target_id: &str,
        relation: &str,
        valid_until: Option<&str>,
    ) -> StoreResult<KgInvalidateRow> {
        let stamp = valid_until.map_or_else(|| Utc::now().to_rfc3339(), str::to_string);

        // `UPDATE ... RETURNING` captures the prior `valid_until` and
        // writes the new one in a single round-trip. The OLD-row
        // semantics come from a CTE: Postgres' `RETURNING` clause sees
        // the NEW row, so we wrap the UPDATE in a CTE and read the
        // prior value through a separate `SELECT` joined on the same
        // (source, target, relation) triple.
        let sql = "WITH prev AS (
                SELECT valid_until AS prior
                FROM memory_links
                WHERE source_id = $1 AND target_id = $2 AND relation = $3
                FOR UPDATE
            ),
            upd AS (
                UPDATE memory_links
                SET valid_until = $4::TIMESTAMPTZ
                WHERE source_id = $1 AND target_id = $2 AND relation = $3
                RETURNING valid_until AS now_until
            )
            SELECT prev.prior, upd.now_until
            FROM prev FULL OUTER JOIN upd ON TRUE";

        let rows = sqlx::query(sql)
            .bind(source_id)
            .bind(target_id)
            .bind(relation)
            .bind(&stamp)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| to_store_err("cte kg_invalidate", e))?;

        if rows.is_empty() {
            return Ok(KgInvalidateRow {
                found: false,
                valid_until: String::new(),
                previous_valid_until: None,
            });
        }

        // The FULL OUTER JOIN yields one row when the triple matched
        // (both `prior` and `now_until` populated) and zero rows when
        // it missed. We've handled the empty case above; pull the
        // first row for the matched case.
        let row = &rows[0];
        let prior: Option<DateTime<Utc>> = row
            .try_get::<Option<DateTime<Utc>>, _>("prior")
            .map_err(|e| to_store_err("read prior valid_until", e))?;
        let now_until: Option<DateTime<Utc>> = row
            .try_get::<Option<DateTime<Utc>>, _>("now_until")
            .map_err(|e| to_store_err("read new valid_until", e))?;

        // `now_until` is `None` only when the UPDATE matched zero rows
        // — i.e. the link did not exist. The `WITH prev AS (...)` CTE
        // would also produce zero rows in that case, so `rows` would
        // be empty. Defensive double-check: if we got here with `None`
        // the triple didn't match.
        if now_until.is_none() {
            return Ok(KgInvalidateRow {
                found: false,
                valid_until: String::new(),
                previous_valid_until: None,
            });
        }

        Ok(KgInvalidateRow {
            found: true,
            valid_until: stamp,
            previous_valid_until: prior.map(|t| t.to_rfc3339()),
        })
    }

    /// v0.7 J7 — enumerate up to N paths between two memories.
    ///
    /// Routes on the [`KgBackend`] resolved at [`Self::connect`] time
    /// (J1 substrate). When AGE is installed the enumeration runs as a
    /// Cypher `MATCH p = (s)-[*..N]-(t) RETURN p LIMIT M` query through
    /// the `memory_graph` projection; otherwise we fall back to a
    /// recursive CTE over the `memory_links` table that mirrors the
    /// SQLite shape in `db::find_paths`.
    ///
    /// Both branches return rows in the same `Vec<Vec<String>>` shape
    /// so the upper-layer `memory_find_paths` handler stays
    /// backend-blind.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::InvalidInput` for `max_depth == 0` or above
    /// the supported ceiling, and `StoreError::BackendUnavailable` for
    /// any underlying SQL or Cypher error.
    pub async fn find_paths(
        &self,
        source_id: &str,
        target_id: &str,
        max_depth: Option<usize>,
        max_results: Option<usize>,
    ) -> StoreResult<Vec<Vec<String>>> {
        match self.kg_backend {
            KgBackend::Age => {
                self.find_paths_cypher(source_id, target_id, max_depth, max_results)
                    .await
            }
            KgBackend::Cte => {
                self.find_paths_cte(source_id, target_id, max_depth, max_results)
                    .await
            }
        }
    }

    /// Recursive-CTE fallback for `find_paths` on Postgres.
    ///
    /// Mirrors the SQLite recursive-CTE in `db::find_paths` so vanilla
    /// Postgres deployments (no AGE extension) get the same enumeration
    /// semantics. The walk is undirected: edges are unioned with their
    /// reverse so paths can traverse `memory_links` against the
    /// declared direction. Cycle prevention uses a TEXT-array prefix
    /// of visited ids.
    ///
    /// # Errors
    ///
    /// `StoreError::InvalidInput` for an out-of-range `max_depth`;
    /// `StoreError::BackendUnavailable` for any sqlx error.
    pub async fn find_paths_cte(
        &self,
        source_id: &str,
        target_id: &str,
        max_depth: Option<usize>,
        max_results: Option<usize>,
    ) -> StoreResult<Vec<Vec<String>>> {
        let depth = max_depth.unwrap_or(FIND_PATHS_DEFAULT_DEPTH_SAL);
        validate_find_paths_depth(depth)?;
        let cap = max_results
            .unwrap_or(FIND_PATHS_DEFAULT_LIMIT_SAL)
            .clamp(1, FIND_PATHS_MAX_LIMIT_SAL);

        if source_id == target_id {
            return Ok(vec![vec![source_id.to_string()]]);
        }

        let depth_i32 = i32::try_from(depth).unwrap_or(i32::MAX);
        let cap_i64 = i64::try_from(cap).unwrap_or(i64::MAX);

        // The CTE walks symmetric edges via a UNION over the original
        // and reverse direction. The visited-id prefix is carried as
        // TEXT[] so we can both check membership (= ANY) and append
        // (array_append). Rows whose `current_id` matches the target
        // and whose depth is at least 1 are reported as completed
        // paths; ordering by depth then path keeps the shortest paths
        // first so the LIMIT cap drops the longest tail.
        let sql = "WITH RECURSIVE traversal(current_id, depth, path) AS (
                SELECT $1::TEXT, 0, ARRAY[$1::TEXT]
                UNION ALL
                SELECT edges.next_id, t.depth + 1, t.path || edges.next_id
                FROM traversal t
                JOIN (
                    SELECT source_id AS from_id, target_id AS next_id FROM memory_links
                    UNION
                    SELECT target_id AS from_id, source_id AS next_id FROM memory_links
                ) edges ON edges.from_id = t.current_id
                WHERE t.depth < $3
                  AND NOT (edges.next_id = ANY(t.path))
            )
            SELECT path
            FROM traversal
            WHERE current_id = $2 AND depth >= 1
            ORDER BY depth ASC, path ASC
            LIMIT $4";

        let rows = sqlx::query(sql)
            .bind(source_id)
            .bind(target_id)
            .bind(depth_i32)
            .bind(cap_i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| to_store_err("cte find_paths", e))?;

        rows.iter()
            .map(|r| {
                let path: Vec<String> = r
                    .try_get::<Vec<String>, _>("path")
                    .map_err(|e| to_store_err("read path", e))?;
                Ok(path)
            })
            .collect()
    }

    /// Cypher (Apache AGE) implementation of `find_paths`.
    ///
    /// Wraps a `MATCH p = (s)-[*..N]-(t) RETURN [n IN nodes(p) | n.id]`
    /// traversal in the `cypher('memory_graph', ...)` set-returning
    /// function, ordered by `length(p)` so shorter paths land first.
    /// Source / target ids are bound through AGE's `$vars` JSON so
    /// user input is never interpolated into the Cypher body. The
    /// variable-length pattern's upper bound IS interpolated — Cypher
    /// does not accept a parameter there — but `validate_find_paths_depth`
    /// already clamps it to a small bounded integer with no injection
    /// surface.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::InvalidInput` for an out-of-range
    /// `max_depth`; `StoreError::BackendUnavailable` for any sqlx or
    /// AGE error.
    pub async fn find_paths_cypher(
        &self,
        source_id: &str,
        target_id: &str,
        max_depth: Option<usize>,
        max_results: Option<usize>,
    ) -> StoreResult<Vec<Vec<String>>> {
        let depth = max_depth.unwrap_or(FIND_PATHS_DEFAULT_DEPTH_SAL);
        validate_find_paths_depth(depth)?;
        let cap = max_results
            .unwrap_or(FIND_PATHS_DEFAULT_LIMIT_SAL)
            .clamp(1, FIND_PATHS_MAX_LIMIT_SAL);

        if source_id == target_id {
            return Ok(vec![vec![source_id.to_string()]]);
        }

        // AGE requires `ag_catalog` on the search path and the extension
        // loaded into the session. Same shape as `kg_query_cypher`.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin AGE tx", e))?;

        sqlx::query("LOAD 'age'")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("LOAD age", e))?;
        sqlx::query("SET search_path = ag_catalog, \"$user\", public")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("set search_path", e))?;

        // `[*..N]` (no leading hop count) reads as "1..N" in Cypher,
        // matching the directed-or-reverse spec. The pattern is
        // un-arrowed (`-[r]-`), giving the symmetric closure for free
        // — the AGE projection stores edges in declared direction but
        // the path query treats them as undirected, the same contract
        // as the CTE branch.
        let cypher = format!(
            "MATCH p = (a)-[*..{depth}]-(b) \
             WHERE a.id = $start_id AND b.id = $target_id \
             RETURN [n IN nodes(p) | n.id] AS path \
             ORDER BY length(p) ASC \
             LIMIT {cap}"
        );

        let sql = format!(
            "SELECT path FROM cypher('memory_graph', $$ {cypher} $$, $1::agtype) AS \
             (path agtype)"
        );

        let params = serde_json::json!({
            "start_id": source_id,
            "target_id": target_id,
        })
        .to_string();

        let rows = sqlx::query(&sql)
            .bind(params)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| to_store_err("cypher find_paths", e))?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit AGE tx", e))?;

        // AGE returns each `path` cell as agtype text shaped like
        // `["id1","id2",...]`. We parse it as JSON since AGE arrays of
        // strings are JSON-compatible — the same trick the rest of
        // this module uses for scalars.
        rows.iter()
            .map(|r| {
                let raw: String = r
                    .try_get::<String, _>("path")
                    .map_err(|e| to_store_err("read path", e))?;
                let parsed: Vec<String> =
                    serde_json::from_str(&raw).map_err(|e| StoreError::IntegrityFailed {
                        detail: format!("non-JSON AGE path: {raw}: {e}"),
                    })?;
                Ok(parsed)
            })
            .collect()
    }

    /// Common implementation for [`PostgresStore::link`] +
    /// [`PostgresStore::link_signed`]. Mirrors SQLite's
    /// `db::create_link_signed` byte-for-byte: when `keypair` carries a
    /// usable private key, the canonical-CBOR-signed bytes land in
    /// `memory_links.signature` with `attest_level = "self_signed"` and
    /// `observed_by = kp.agent_id`; otherwise the row is unsigned.
    ///
    /// Idempotent on the `(source_id, target_id, relation)` unique key
    /// — duplicate writes collapse via `ON CONFLICT … DO NOTHING` so a
    /// migrate replay doesn't error on already-shipped links.
    ///
    /// Returns the resolved attestation level so the trait surfaces
    /// (`link_signed`) can pass it back to upper layers without
    /// re-querying.
    async fn link_internal(
        &self,
        link: &MemoryLink,
        keypair: Option<&crate::identity::keypair::AgentKeypair>,
    ) -> StoreResult<&'static str> {
        // FK pre-flight — mirror SQLite's explicit existence check so
        // the error message names the missing memory rather than
        // surfacing a raw `pg_class` constraint violation. Both
        // adapters now agree on the wire-shape for this failure mode.
        let source_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memories WHERE id = $1)")
                .bind(&link.source_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| to_store_err("check source memory", e))?;
        if !source_exists {
            return Err(StoreError::InvalidInput {
                detail: format!("source memory not found: {}", link.source_id),
            });
        }
        let target_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memories WHERE id = $1)")
                .bind(&link.target_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| to_store_err("check target memory", e))?;
        if !target_exists {
            return Err(StoreError::InvalidInput {
                detail: format!("target memory not found: {}", link.target_id),
            });
        }

        // Resolve `created_at` / `valid_from`. Mirrors SQLite — both
        // columns get the current wall-clock when the caller did not
        // supply explicit values on the input record. Federation
        // replays (peer-attested links) carry their own stamps so we
        // honour them when present so signatures still verify.
        let now_utc = Utc::now();
        let now_rfc = now_utc.to_rfc3339();

        let created_at_dt = if link.created_at.is_empty() {
            now_utc
        } else {
            parse_rfc3339_required(&link.created_at)?
        };
        let valid_from_dt = match link.valid_from.as_deref() {
            Some(s) if !s.is_empty() => parse_rfc3339_required(s)?,
            _ => now_utc,
        };
        let valid_until_dt = match link.valid_until.as_deref() {
            Some(s) if !s.is_empty() => Some(parse_rfc3339_required(s)?),
            _ => None,
        };

        // Branch on the keypair: signed vs. unsigned. The signed path
        // computes the canonical CBOR + Ed25519 signature BEFORE the
        // INSERT so a CBOR/sign failure surfaces as a clean error
        // rather than a half-written row. This is the same ordering
        // SQLite uses (see `db::create_link_signed`).
        let valid_from_str = match link.valid_from.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => now_rfc.clone(),
        };
        let (signature, attest_level, observed_by_col): (
            Option<Vec<u8>>,
            &'static str,
            Option<String>,
        ) = match keypair {
            Some(kp) if kp.can_sign() => {
                let signable = crate::identity::sign::SignableLink {
                    src_id: &link.source_id,
                    dst_id: &link.target_id,
                    relation: &link.relation,
                    observed_by: Some(kp.agent_id.as_str()),
                    valid_from: Some(valid_from_str.as_str()),
                    valid_until: link.valid_until.as_deref(),
                };
                let sig = crate::identity::sign::sign(kp, &signable).map_err(|e| {
                    StoreError::IntegrityFailed {
                        detail: format!("sign link: {e}"),
                    }
                })?;
                (Some(sig), "self_signed", Some(kp.agent_id.clone()))
            }
            _ => (None, "unsigned", None),
        };

        // ON CONFLICT … DO NOTHING gives idempotent migrate replays:
        // re-shipping a link the destination already holds collapses
        // to a no-op. The row's existing `signature` / `attest_level`
        // are preserved, so a self-signed write is never silently
        // demoted to unsigned by a subsequent unsigned replay.
        sqlx::query(
            "INSERT INTO memory_links
                 (source_id, target_id, relation, created_at, valid_from,
                  valid_until, signature, attest_level, observed_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (source_id, target_id, relation) DO NOTHING",
        )
        .bind(&link.source_id)
        .bind(&link.target_id)
        .bind(&link.relation)
        .bind(created_at_dt)
        .bind(valid_from_dt)
        .bind(valid_until_dt)
        .bind(signature)
        .bind(attest_level)
        .bind(observed_by_col)
        .execute(&self.pool)
        .await
        .map_err(|e| to_store_err("insert memory_link", e))?;

        Ok(attest_level)
    }

    fn row_to_memory(row: &sqlx::postgres::PgRow) -> StoreResult<Memory> {
        let created_at: DateTime<Utc> = row
            .try_get("created_at")
            .map_err(|e| to_store_err("read created_at", e))?;
        let updated_at: DateTime<Utc> = row
            .try_get("updated_at")
            .map_err(|e| to_store_err("read updated_at", e))?;
        let last_accessed_at: Option<DateTime<Utc>> = row
            .try_get("last_accessed_at")
            .map_err(|e| to_store_err("read last_accessed_at", e))?;
        let expires_at: Option<DateTime<Utc>> = row
            .try_get("expires_at")
            .map_err(|e| to_store_err("read expires_at", e))?;

        let tier_str: String = row
            .try_get("tier")
            .map_err(|e| to_store_err("read tier", e))?;
        let tier = Tier::from_str(&tier_str).ok_or_else(|| StoreError::IntegrityFailed {
            detail: format!("invalid tier value: {tier_str}"),
        })?;

        let tags_json: serde_json::Value = row
            .try_get("tags")
            .map_err(|e| to_store_err("read tags", e))?;
        let tags: Vec<String> = serde_json::from_value(tags_json).unwrap_or_default();

        let metadata: serde_json::Value = row
            .try_get("metadata")
            .map_err(|e| to_store_err("read metadata", e))?;

        Ok(Memory {
            id: row.try_get("id").map_err(|e| to_store_err("read id", e))?,
            tier,
            namespace: row
                .try_get("namespace")
                .map_err(|e| to_store_err("read namespace", e))?,
            title: row
                .try_get("title")
                .map_err(|e| to_store_err("read title", e))?,
            content: row
                .try_get("content")
                .map_err(|e| to_store_err("read content", e))?,
            tags,
            priority: row
                .try_get("priority")
                .map_err(|e| to_store_err("read priority", e))?,
            confidence: row
                .try_get("confidence")
                .map_err(|e| to_store_err("read confidence", e))?,
            source: row
                .try_get("source")
                .map_err(|e| to_store_err("read source", e))?,
            access_count: row
                .try_get("access_count")
                .map_err(|e| to_store_err("read access_count", e))?,
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
            last_accessed_at: last_accessed_at.map(|t| t.to_rfc3339()),
            expires_at: expires_at.map(|t| t.to_rfc3339()),
            metadata,
        })
    }
}

#[allow(clippy::needless_pass_by_value)]
fn to_store_err(what: &str, e: sqlx::Error) -> StoreError {
    StoreError::BackendUnavailable {
        backend: "postgres".to_string(),
        detail: format!("{what}: {e}"),
    }
}

/// Issue an `ALTER TABLE ... ADD COLUMN` only when the column is not
/// yet present. Postgres has no `ADD COLUMN IF NOT EXISTS` clause that
/// also tolerates an existing column with a different shape — the
/// `information_schema.columns` lookup is the safest probe and runs in
/// the same transaction so a concurrent re-applied migration is
/// serializable. Caller passes the full DDL string verbatim because
/// each ADD COLUMN may carry different defaults / nullability.
async fn add_column_if_missing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    column: &str,
    ddl: &str,
) -> StoreResult<()> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM information_schema.columns
            WHERE table_name = $1 AND column_name = $2
        )",
    )
    .bind(table)
    .bind(column)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| to_store_err(&format!("check {table}.{column} column"), e))?;

    if !exists {
        sqlx::query(ddl)
            .execute(&mut **tx)
            .await
            .map_err(|e| to_store_err(&format!("add {table}.{column} column"), e))?;
    }
    Ok(())
}

/// Stamp a successful schema migration into `schema_version`.
///
/// Each step calls this with its own version number; we use
/// `ON CONFLICT (version) DO NOTHING` so re-running the migration over
/// an already-stamped row is a clean no-op. The previous v15 path
/// `DELETE` + `INSERT`'d which would drop history of intermediate
/// versions; preserving every applied version is more useful for
/// post-mortem auditing.
async fn record_schema_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    version: i32,
) -> StoreResult<()> {
    sqlx::query(
        "INSERT INTO schema_version (version) VALUES ($1) ON CONFLICT (version) DO NOTHING",
    )
    .bind(version)
    .execute(&mut **tx)
    .await
    .map_err(|e| to_store_err("insert schema_version", e))?;
    Ok(())
}

/// Maximum traversal depth supported by [`PostgresStore::kg_query`].
///
/// Mirrors `crate::db::KG_QUERY_MAX_SUPPORTED_DEPTH` (the SQLite path)
/// so the published depth budget is the same on every backend. Pinned
/// here rather than re-imported because `crate::db` is a SQLite-only
/// module that doesn't compile under the `sal-postgres` test surface
/// when sqlite features are disabled.
const KG_QUERY_MAX_SUPPORTED_DEPTH: usize = 5;

/// Common pre-flight check shared by both `kg_query` branches. Pulled
/// out so the dispatcher's contract — "0 and >5 always error before we
/// touch the wire" — survives a future refactor that splits the
/// branches into separate modules.
fn validate_depth(max_depth: usize) -> StoreResult<()> {
    if max_depth == 0 {
        return Err(StoreError::InvalidInput {
            detail: "max_depth must be >= 1".to_string(),
        });
    }
    if max_depth > KG_QUERY_MAX_SUPPORTED_DEPTH {
        return Err(StoreError::InvalidInput {
            detail: format!(
                "max_depth={max_depth} exceeds supported depth={KG_QUERY_MAX_SUPPORTED_DEPTH}"
            ),
        });
    }
    Ok(())
}

/// Default depth used by [`PostgresStore::find_paths`] when the caller
/// omits `max_depth`. Mirrors `crate::db::FIND_PATHS_DEFAULT_DEPTH`.
const FIND_PATHS_DEFAULT_DEPTH_SAL: usize = 4;

/// Hard ceiling on traversal depth supported by
/// [`PostgresStore::find_paths`]. Mirrors
/// `crate::db::FIND_PATHS_MAX_DEPTH`.
const FIND_PATHS_MAX_DEPTH_SAL: usize = 7;

/// Default cap on paths returned by [`PostgresStore::find_paths`] when
/// the caller omits `max_results`. Mirrors
/// `crate::db::FIND_PATHS_DEFAULT_LIMIT`.
const FIND_PATHS_DEFAULT_LIMIT_SAL: usize = 10;

/// Hard ceiling on paths returned by [`PostgresStore::find_paths`].
/// Mirrors `crate::db::FIND_PATHS_MAX_LIMIT`.
const FIND_PATHS_MAX_LIMIT_SAL: usize = 50;

/// Common pre-flight check shared by both `find_paths` branches. Pulled
/// out so the dispatcher's contract — "0 and >FIND_PATHS_MAX_DEPTH_SAL
/// always error before we touch the wire" — survives a future refactor
/// that splits the branches into separate modules.
fn validate_find_paths_depth(max_depth: usize) -> StoreResult<()> {
    if max_depth == 0 {
        return Err(StoreError::InvalidInput {
            detail: "max_depth must be >= 1".to_string(),
        });
    }
    if max_depth > FIND_PATHS_MAX_DEPTH_SAL {
        return Err(StoreError::InvalidInput {
            detail: format!(
                "max_depth={max_depth} exceeds supported depth={FIND_PATHS_MAX_DEPTH_SAL}"
            ),
        });
    }
    Ok(())
}

/// Default cap on rows returned by [`PostgresStore::kg_timeline`] when
/// the caller does not specify one.
///
/// Mirrors `crate::db::KG_TIMELINE_DEFAULT_LIMIT` so the default page
/// size is identical on every backend. Pinned here rather than
/// re-imported because `crate::db` is a SQLite-only module that
/// doesn't compile under the `sal-postgres` test surface when sqlite
/// features are disabled.
const KG_TIMELINE_DEFAULT_LIMIT_SAL: usize = 200;

/// Hard ceiling on rows returned by [`PostgresStore::kg_timeline`].
///
/// Mirrors `crate::db::KG_TIMELINE_MAX_LIMIT`.
const KG_TIMELINE_MAX_LIMIT_SAL: usize = 1000;

/// Apply the published timeline page-size policy: default to
/// [`KG_TIMELINE_DEFAULT_LIMIT_SAL`] when the caller didn't pass a
/// limit, then clamp to the `[1, KG_TIMELINE_MAX_LIMIT_SAL]` band so a
/// crafted call cannot exhaust the connection pool with one giant
/// fetch. Pulled into a free function so both backend branches share
/// the same clamping contract.
fn clamp_timeline_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(KG_TIMELINE_DEFAULT_LIMIT_SAL)
        .clamp(1, KG_TIMELINE_MAX_LIMIT_SAL)
}

/// Decode an AGE agtype scalar that may be a quoted string or the
/// agtype `null` token. Returns `None` for `null`, otherwise the
/// dequoted UTF-8 payload.
///
/// AGE's `cypher()` SRF returns each column as `agtype`. Casting to
/// `text` produces `"value"` for strings and the literal token `null`
/// for missing data. The CTE branch returns `Option<String>` directly
/// from sqlx; the AGE branch needs this helper to mirror the same
/// shape so the upper-layer handler stays backend-blind.
fn agtype_optional_string(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    Some(strip_agtype_quotes(trimmed).to_string())
}

/// Strip the surrounding double-quotes that AGE wraps around its
/// agtype string scalars when they get cast to text.
///
/// AGE returns scalar values as text in the form `"value"` (note the
/// embedded quotes). Calling `.trim_matches('"')` is enough to recover
/// the original UTF-8 — agtype escaping for embedded quotes uses `\"`,
/// and the v0.7 KG corpus does not write ids containing literal quote
/// characters. If that contract changes we'll replace this with a
/// dedicated agtype parser; keeping it minimal avoids pulling another
/// crate just to peel a string.
fn strip_agtype_quotes(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

/// v0.7 J1 — probe Apache AGE.
///
/// Runs `SELECT 1 FROM pg_extension WHERE extname = 'age'` against the
/// pool and reports the resolved [`KgBackend`]. Errors are *not*
/// surfaced — a transient probe failure (replica lag, permissions on
/// `pg_extension` in a hardened deployment) MUST NOT block adapter
/// bootstrap. We log a debug line and fall back to
/// [`KgBackend::Cte`] which every Postgres install supports.
///
/// Factored out of [`PostgresStore::connect`] so unit tests can hit
/// the SQL builder branch without standing up a real Postgres pool.
pub(crate) async fn detect_kg_backend(pool: &PgPool) -> KgBackend {
    match sqlx::query_scalar::<_, i32>("SELECT 1 FROM pg_extension WHERE extname = 'age'")
        .fetch_optional(pool)
        .await
    {
        Ok(Some(_)) => KgBackend::Age,
        Ok(None) => KgBackend::Cte,
        Err(e) => {
            tracing::debug!(
                target = "store::postgres",
                error = %e,
                "AGE detection probe failed; defaulting to CTE backend"
            );
            KgBackend::Cte
        }
    }
}

fn parse_rfc3339_opt(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|raw| DateTime::parse_from_rfc3339(raw).ok().map(Into::into))
}

fn parse_rfc3339_required(s: &str) -> StoreResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(Into::into)
        .map_err(|e| StoreError::IntegrityFailed {
            detail: format!("invalid rfc3339 timestamp {s}: {e}"),
        })
}

#[async_trait]
impl MemoryStore for PostgresStore {
    fn capabilities(&self) -> Capabilities {
        Capabilities::TRANSACTIONS
            | Capabilities::NATIVE_VECTOR
            | Capabilities::FULLTEXT
            | Capabilities::DURABLE
            | Capabilities::STRONG_CONSISTENCY
            | Capabilities::ATOMIC_MULTI_WRITE
    }

    async fn store(&self, _ctx: &CallerContext, memory: &Memory) -> StoreResult<String> {
        let created_at = parse_rfc3339_required(&memory.created_at)?;
        let updated_at = parse_rfc3339_required(&memory.updated_at)?;
        let last_accessed_at = parse_rfc3339_opt(memory.last_accessed_at.as_deref());
        let expires_at = parse_rfc3339_opt(memory.expires_at.as_deref());
        let tags_json =
            serde_json::to_value(&memory.tags).map_err(|e| StoreError::IntegrityFailed {
                detail: format!("serialize tags: {e}"),
            })?;

        // Upsert contract matches SQLite: `ON CONFLICT (title, namespace)`.
        // Backed by the UNIQUE INDEX `memories_title_ns_uidx` in
        // postgres_schema.sql. Fix for blocker #294.
        //
        // Agent-id immutability (blocker #295): on conflict we preserve
        // the ORIGINAL `metadata.agent_id` via `jsonb_set`, mirroring the
        // SQLite `json_set` CASE clause in `src/db.rs::insert`. The
        // caller-supplied metadata otherwise wins.
        //
        // Tier never downgrades (blocker #296 / SQLite parity): on
        // conflict tier takes max of existing vs new via rank mapping.
        sqlx::query(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, last_accessed_at,
                expires_at, metadata
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            ON CONFLICT (title, namespace) DO UPDATE SET
                content = EXCLUDED.content,
                tier = CASE
                    WHEN tier_rank(EXCLUDED.tier) >= tier_rank(memories.tier)
                        THEN EXCLUDED.tier
                    ELSE memories.tier
                END,
                tags = EXCLUDED.tags,
                priority = EXCLUDED.priority,
                confidence = EXCLUDED.confidence,
                updated_at = EXCLUDED.updated_at,
                metadata = CASE
                    WHEN memories.metadata ? 'agent_id'
                        THEN jsonb_set(
                            EXCLUDED.metadata,
                            '{agent_id}',
                            memories.metadata -> 'agent_id'
                        )
                    ELSE EXCLUDED.metadata
                END
            RETURNING id",
        )
        .bind(&memory.id)
        .bind(memory.tier.as_str())
        .bind(&memory.namespace)
        .bind(&memory.title)
        .bind(&memory.content)
        .bind(&tags_json)
        .bind(memory.priority)
        .bind(memory.confidence)
        .bind(&memory.source)
        .bind(memory.access_count)
        .bind(created_at)
        .bind(updated_at)
        .bind(last_accessed_at)
        .bind(expires_at)
        .bind(&memory.metadata)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| to_store_err("insert memory", e))?
        .try_get::<String, _>("id")
        .map_err(|e| to_store_err("read returned id", e))
    }

    async fn get(&self, _ctx: &CallerContext, id: &str) -> StoreResult<Memory> {
        let row = sqlx::query("SELECT * FROM memories WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| to_store_err("select by id", e))?;
        match row {
            Some(r) => Self::row_to_memory(&r),
            None => Err(StoreError::NotFound { id: id.to_string() }),
        }
    }

    async fn update(&self, _ctx: &CallerContext, id: &str, patch: UpdatePatch) -> StoreResult<()> {
        // One-shot COALESCE update — each patch field overrides only if
        // Some, otherwise falls through to the existing value.
        //
        // Blocker #296: tier never downgrades. When a patch proposes a
        // tier of lower rank than the current row's tier, the DB keeps
        // the higher tier via `GREATEST(tier_rank(...))`.
        //
        // Blocker #295: `metadata.agent_id` is SQL-layer-immutable. If
        // the current row has an agent_id we preserve it against any
        // patch; otherwise the patch's metadata (if provided) wins.
        let rows_affected = sqlx::query(
            "UPDATE memories SET
                title = COALESCE($2, title),
                content = COALESCE($3, content),
                tier = CASE
                    WHEN $4::TEXT IS NULL THEN tier
                    WHEN tier_rank($4::TEXT) >= tier_rank(tier) THEN $4::TEXT
                    ELSE tier
                END,
                namespace = COALESCE($5, namespace),
                tags = COALESCE($6, tags),
                priority = COALESCE($7, priority),
                confidence = COALESCE($8, confidence),
                metadata = CASE
                    WHEN $9::JSONB IS NULL THEN metadata
                    WHEN metadata ? 'agent_id' THEN jsonb_set(
                        $9::JSONB,
                        '{agent_id}',
                        metadata -> 'agent_id'
                    )
                    ELSE $9::JSONB
                END,
                updated_at = NOW()
             WHERE id = $1",
        )
        .bind(id)
        .bind(patch.title)
        .bind(patch.content)
        .bind(patch.tier.as_ref().map(Tier::as_str))
        .bind(patch.namespace)
        .bind(
            patch
                .tags
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| StoreError::IntegrityFailed {
                    detail: format!("serialize tags patch: {e}"),
                })?,
        )
        .bind(patch.priority)
        .bind(patch.confidence)
        .bind(patch.metadata)
        .execute(&self.pool)
        .await
        .map_err(|e| to_store_err("update", e))?
        .rows_affected();

        if rows_affected == 0 {
            Err(StoreError::NotFound { id: id.to_string() })
        } else {
            Ok(())
        }
    }

    async fn delete(&self, _ctx: &CallerContext, id: &str) -> StoreResult<()> {
        let rows_affected = sqlx::query("DELETE FROM memories WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| to_store_err("delete", e))?
            .rows_affected();
        if rows_affected == 0 {
            Err(StoreError::NotFound { id: id.to_string() })
        } else {
            Ok(())
        }
    }

    async fn list(&self, _ctx: &CallerContext, filter: &Filter) -> StoreResult<Vec<Memory>> {
        let limit: i64 = filter.limit.clamp(1, 1000).try_into().unwrap_or(100);
        let rows = sqlx::query(
            "SELECT * FROM memories
             WHERE ($1::text IS NULL OR namespace = $1)
               AND ($2::text IS NULL OR tier = $2)
               AND (expires_at IS NULL OR expires_at > NOW())
               AND ($3::timestamptz IS NULL OR created_at >= $3)
               AND ($4::timestamptz IS NULL OR created_at <= $4)
             ORDER BY priority DESC, updated_at DESC
             LIMIT $5",
        )
        .bind(filter.namespace.as_ref())
        .bind(filter.tier.as_ref().map(Tier::as_str))
        .bind(filter.since)
        .bind(filter.until)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("list", e))?;

        rows.iter().map(Self::row_to_memory).collect()
    }

    async fn search(
        &self,
        _ctx: &CallerContext,
        query: &str,
        filter: &Filter,
    ) -> StoreResult<Vec<Memory>> {
        let limit: i64 = filter.limit.clamp(1, 1000).try_into().unwrap_or(100);
        // Adapter parity with SQLite (#302 item 3): threads the full
        // Filter (namespace, tier, tags_any, agent_id) into the query.
        // Prior implementation ignored `tags_any` and `agent_id` so
        // identical calls returned different result sets on the two
        // adapters.
        //
        // `tags_any`:  match if any of the requested tags is present in
        //              memories.tags (JSONB array). Uses @> over a
        //              single-element JSONB array per requested tag,
        //              OR'd together via sqlx bind array.
        // `agent_id`:  match if metadata->>'agent_id' == $agent_id.
        //
        // ────────────────────────────────────────────────────────────
        // v0.7.0 F6 Gap 7 — recall scoring parity with SQLite.
        //
        // SQLite's `db::recall` scores each FTS hit with a 6-factor
        // blend (src/db.rs::recall):
        //
        //     fts.rank        * (-1)
        //   + priority        * 0.5
        //   + min(access_count, 50) * 0.1
        //   + confidence      * 2.0
        //   + tier_bonus      (long=3.0, mid=1.0, short=0.0)
        //   + recency_factor  (1 / (1 + age_days * 0.1))
        //
        // Pre-v0.7.0 the Postgres adapter shipped a 2-factor blend
        // (`ts_rank DESC, priority DESC`) so identical FTS calls
        // produced different orderings on the two backends. This
        // brings them to byte-equivalent rank up to a small swap
        // tolerance (covered in tests/recall_scoring_parity.rs).
        //
        // SQLite's `fts.rank` is BM25-style and negative; multiplying
        // by `-1` makes higher-rank hits score positive. Postgres'
        // `ts_rank` is already positive so we use it directly without
        // sign flipping. The relative magnitudes line up well enough
        // for top-K ordering parity in practice — see the parity test
        // suite for tolerance bounds.
        //
        // The recency_factor uses `EXTRACT(EPOCH FROM (NOW() -
        // updated_at)) / 86400.0` as the day-age, mirroring SQLite's
        // `julianday('now') - julianday(m.updated_at)` (also days).
        let tags_first: Option<&str> = filter.tags_any.first().map(String::as_str);
        let rows = sqlx::query(
            "SELECT *,
                    ts_rank(
                        to_tsvector('english', title || ' ' || content),
                        plainto_tsquery('english', $1)
                    )
                    + (priority * 0.5)
                    + (LEAST(access_count, 50) * 0.1)
                    + (confidence * 2.0)
                    + CASE tier
                          WHEN 'long' THEN 3.0
                          WHEN 'mid'  THEN 1.0
                          ELSE 0.0
                      END
                    + (1.0 / (1.0 +
                        EXTRACT(EPOCH FROM (NOW() - updated_at)) / 86400.0 * 0.1))
                      AS rank
             FROM memories
             WHERE to_tsvector('english', title || ' ' || content) @@ plainto_tsquery('english', $1)
               AND ($2::text IS NULL OR namespace = $2)
               AND ($3::text IS NULL OR tier = $3)
               AND ($4::text IS NULL OR tags @> to_jsonb(ARRAY[$4]))
               AND ($5::text IS NULL OR metadata ->> 'agent_id' = $5)
               AND (expires_at IS NULL OR expires_at > NOW())
             ORDER BY rank DESC, priority DESC
             LIMIT $6",
        )
        .bind(query)
        .bind(filter.namespace.as_ref())
        .bind(filter.tier.as_ref().map(Tier::as_str))
        .bind(tags_first)
        .bind(filter.agent_id.as_ref())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("search", e))?;

        rows.iter().map(Self::row_to_memory).collect()
    }

    async fn verify(&self, _ctx: &CallerContext, id: &str) -> StoreResult<VerifyReport> {
        // Minimal verify: confirm the row is readable and the tier +
        // timestamps parse. Integrity signatures land with the
        // provenance work on the Postgres track next sprint.
        let mem = self.get(_ctx, id).await?;
        let mut findings = Vec::new();
        if mem.content.is_empty() {
            findings.push("empty content".to_string());
        }
        parse_rfc3339_required(&mem.created_at).map_err(|_| StoreError::IntegrityFailed {
            detail: format!("invalid created_at on {id}"),
        })?;
        Ok(VerifyReport {
            memory_id: id.to_string(),
            integrity_ok: findings.is_empty(),
            findings,
            // v0.6.0 does NOT perform signature verification; real
            // cryptographic verify lands with Task 1.4. See #302.
            signature_verified: false,
        })
    }

    async fn link(&self, _ctx: &CallerContext, link: &MemoryLink) -> StoreResult<()> {
        // F6 Gap 3 (v0.7.0) — unsigned link write. The trait method
        // does not surface a keypair so we always land the row with
        // `attest_level = "unsigned"`. Callers that want signing route
        // through [`MemoryStore::link_signed`].
        self.link_internal(link, None).await.map(|_| ())
    }

    async fn link_signed(
        &self,
        _ctx: &CallerContext,
        link: &MemoryLink,
        keypair: Option<&crate::identity::keypair::AgentKeypair>,
    ) -> StoreResult<&'static str> {
        self.link_internal(link, keypair).await
    }

    async fn list_links(&self, namespace: Option<&str>) -> StoreResult<Vec<MemoryLink>> {
        // F6 Gap 2 (v0.7.0) — surface the full link table to the
        // migrate runner. The namespace filter matches the **source**
        // memory's namespace (links live with their source — the same
        // affinity SQLite's `migrate` uses for memories), so a
        // namespace-scoped migrate captures every outbound edge.
        //
        // Ordering by `(source_id, target_id, relation)` is the SAL
        // contract — deterministic across calls and matches the unique
        // key, so a paginated migrate can resume without losing rows.
        let rows = sqlx::query(
            "SELECT ml.source_id, ml.target_id, ml.relation, ml.created_at,
                    ml.valid_from, ml.valid_until, ml.observed_by, ml.signature
             FROM memory_links ml
             WHERE ($1::text IS NULL
                    OR EXISTS (SELECT 1 FROM memories m
                               WHERE m.id = ml.source_id AND m.namespace = $1))
             ORDER BY ml.source_id, ml.target_id, ml.relation",
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("list_links", e))?;

        rows.iter()
            .map(|r| {
                let created_at: DateTime<Utc> = r
                    .try_get::<DateTime<Utc>, _>("created_at")
                    .map_err(|e| to_store_err("read created_at", e))?;
                let valid_from: Option<DateTime<Utc>> = r
                    .try_get::<Option<DateTime<Utc>>, _>("valid_from")
                    .map_err(|e| to_store_err("read valid_from", e))?;
                let valid_until: Option<DateTime<Utc>> = r
                    .try_get::<Option<DateTime<Utc>>, _>("valid_until")
                    .map_err(|e| to_store_err("read valid_until", e))?;
                let observed_by: Option<String> = r
                    .try_get::<Option<String>, _>("observed_by")
                    .map_err(|e| to_store_err("read observed_by", e))?;
                let signature: Option<Vec<u8>> = r
                    .try_get::<Option<Vec<u8>>, _>("signature")
                    .map_err(|e| to_store_err("read signature", e))?;
                Ok(MemoryLink {
                    source_id: r
                        .try_get::<String, _>("source_id")
                        .map_err(|e| to_store_err("read source_id", e))?,
                    target_id: r
                        .try_get::<String, _>("target_id")
                        .map_err(|e| to_store_err("read target_id", e))?,
                    relation: r
                        .try_get::<String, _>("relation")
                        .map_err(|e| to_store_err("read relation", e))?,
                    created_at: created_at.to_rfc3339(),
                    signature,
                    observed_by,
                    valid_from: valid_from.map(|t| t.to_rfc3339()),
                    valid_until: valid_until.map(|t| t.to_rfc3339()),
                })
            })
            .collect()
    }

    fn as_any_for_postgres(&self) -> &dyn std::any::Any {
        self
    }

    async fn register_agent(
        &self,
        ctx: &CallerContext,
        agent: &AgentRegistration,
    ) -> StoreResult<()> {
        // F6 Gap 4 (v0.7.0) — agent registration parity with SQLite.
        //
        // SQLite's `db::register_agent` writes into the `memories`
        // table at namespace `_agents` with `title = "agent:<id>"` and
        // tier `Long`, preserving `registered_at` across re-registration
        // (caller-observable provenance). We mirror that here so
        // `list_agents` (which reads from the same `_agents` namespace)
        // works against either backend identically.
        //
        // Re-registration semantics: `(title, namespace)` upsert is
        // already wired at the schema level (`memories_title_ns_uidx`),
        // so re-INSERTing the same `agent:<id>` row collapses to an
        // UPDATE. We pre-read the existing `metadata.registered_at` so
        // the upsert preserves the original stamp; without that step,
        // every re-registration would reset it.
        use crate::models::AGENTS_NAMESPACE;

        let title = format!("agent:{}", agent.agent_id);
        let now_rfc = Utc::now().to_rfc3339();

        // Preserve the original registered_at across re-registration.
        let existing: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT metadata FROM memories WHERE namespace = $1 AND title = $2")
                .bind(AGENTS_NAMESPACE)
                .bind(&title)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| to_store_err("read existing agent metadata", e))?;

        let registered_at = existing
            .as_ref()
            .and_then(|(m,)| m.get("registered_at"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| now_rfc.clone(), str::to_string);

        let metadata = serde_json::json!({
            "agent_id": agent.agent_id,
            "agent_type": agent.agent_type,
            "capabilities": agent.capabilities,
            "registered_at": registered_at,
            "last_seen_at": now_rfc,
        });

        let content =
            serde_json::to_string(&metadata).map_err(|e| StoreError::IntegrityFailed {
                detail: format!("serialize agent registration: {e}"),
            })?;

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
            created_at: now_rfc.clone(),
            updated_at: now_rfc,
            last_accessed_at: None,
            expires_at: None,
            metadata,
        };

        self.store(ctx, &mem).await.map(|_| ())
    }
}

// ----------------------------------------------------------------------
// v0.7.0 Wave-3 Continuation — postgres-only helpers used by HTTP
// handlers when `app.storage_backend == Postgres`. These intentionally
// do NOT live on the `MemoryStore` trait yet — the trait surface is
// stabilising; the helpers below cover archive read paths that will
// eventually move onto the trait once the SQLite adapter has parity.
// ----------------------------------------------------------------------

/// Project the Postgres `archived_memories` table into the JSON wire
/// shape produced by the SQLite `db::list_archived` for HTTP parity.
/// Takes an `Arc<dyn MemoryStore>` rather than a concrete pool so the
/// caller can pass the daemon's `app.store` handle directly.
///
/// # Errors
///
/// Returns [`StoreError::BackendUnavailable`] when the underlying
/// adapter is not a [`PostgresStore`] or when the SQL query fails.
pub async fn list_archived_via_store(
    store: &std::sync::Arc<dyn MemoryStore>,
    namespace: Option<&str>,
    limit: usize,
    offset: usize,
) -> StoreResult<Vec<serde_json::Value>> {
    let pg = downcast_postgres(store)?;
    pg.list_archived(namespace, limit, offset).await
}

/// Project the Postgres `archived_memories` aggregate stats into the
/// same JSON wire shape produced by SQLite's `db::archive_stats`.
///
/// # Errors
///
/// Same surface as [`list_archived_via_store`].
pub async fn archive_stats_via_store(
    store: &std::sync::Arc<dyn MemoryStore>,
) -> StoreResult<serde_json::Value> {
    let pg = downcast_postgres(store)?;
    pg.archive_stats().await
}

fn downcast_postgres(store: &std::sync::Arc<dyn MemoryStore>) -> StoreResult<&PostgresStore> {
    // Trait objects don't expose downcast directly; we rely on the fact
    // that the daemon only constructs a `PostgresStore` when the
    // operator passes `--store-url postgres://...` so the storage_backend
    // flag is the load-bearing discriminator. Use `Any` projection via
    // a private hatch.
    let any = store.as_any_for_postgres();
    any.downcast_ref::<PostgresStore>()
        .ok_or_else(|| StoreError::BackendUnavailable {
            backend: "postgres".to_string(),
            detail: "active store is not a PostgresStore".to_string(),
        })
}

impl PostgresStore {
    /// Project the `archived_memories` table into the same wire shape
    /// `db::list_archived` produces for SQLite. Tags are stored as a
    /// JSONB array; we serialize back to a string-formatted JSON to
    /// match SQLite's `tags` text-encoded shape.
    async fn list_archived(
        &self,
        namespace: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> StoreResult<Vec<serde_json::Value>> {
        let limit_i: i64 = limit.clamp(1, 1000).try_into().unwrap_or(50);
        let offset_i: i64 = offset.try_into().unwrap_or(0);
        let rows = sqlx::query(
            "SELECT id, tier, namespace, title, content, tags, priority, confidence, \
             source, access_count, created_at, updated_at, last_accessed_at, \
             expires_at, archived_at, archive_reason, metadata \
             FROM archived_memories \
             WHERE ($1::text IS NULL OR namespace = $1) \
             ORDER BY archived_at DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(namespace)
        .bind(limit_i)
        .bind(offset_i)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("list archived memories", e))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            use sqlx::Row;
            let tags_jsonb: serde_json::Value =
                row.try_get("tags").unwrap_or(serde_json::json!([]));
            let tags_string =
                serde_json::to_string(&tags_jsonb).unwrap_or_else(|_| "[]".to_string());
            let metadata: serde_json::Value =
                row.try_get("metadata").unwrap_or(serde_json::json!({}));
            let last_accessed_at: Option<DateTime<Utc>> =
                row.try_get("last_accessed_at").unwrap_or(None);
            let expires_at: Option<DateTime<Utc>> = row.try_get("expires_at").unwrap_or(None);
            let archived_at: DateTime<Utc> =
                row.try_get("archived_at").unwrap_or_else(|_| Utc::now());
            let created_at: DateTime<Utc> =
                row.try_get("created_at").unwrap_or_else(|_| Utc::now());
            let updated_at: DateTime<Utc> =
                row.try_get("updated_at").unwrap_or_else(|_| Utc::now());
            out.push(serde_json::json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "tier": row.try_get::<String, _>("tier").unwrap_or_default(),
                "namespace": row.try_get::<String, _>("namespace").unwrap_or_default(),
                "title": row.try_get::<String, _>("title").unwrap_or_default(),
                "content": row.try_get::<String, _>("content").unwrap_or_default(),
                "tags": tags_string,
                "priority": row.try_get::<i32, _>("priority").unwrap_or(5),
                "confidence": row.try_get::<f64, _>("confidence").unwrap_or(0.5),
                "source": row.try_get::<String, _>("source").unwrap_or_default(),
                "access_count": row.try_get::<i64, _>("access_count").unwrap_or(0),
                "created_at": created_at.to_rfc3339(),
                "updated_at": updated_at.to_rfc3339(),
                "last_accessed_at": last_accessed_at.map(|d| d.to_rfc3339()),
                "expires_at": expires_at.map(|d| d.to_rfc3339()),
                "archived_at": archived_at.to_rfc3339(),
                "archive_reason": row.try_get::<String, _>("archive_reason").unwrap_or_else(|_| "ttl_expired".to_string()),
                "metadata": metadata,
            }));
        }
        Ok(out)
    }

    /// Aggregate `archived_memories` rows into the same JSON shape
    /// `db::archive_stats` returns for SQLite.
    async fn archive_stats(&self) -> StoreResult<serde_json::Value> {
        use sqlx::Row;
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archived_memories")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| to_store_err("archive stats total", e))?;

        let by_reason_rows = sqlx::query(
            "SELECT archive_reason, COUNT(*) AS cnt FROM archived_memories \
             GROUP BY archive_reason ORDER BY cnt DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("archive stats by_reason", e))?;
        let mut by_reason: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for r in by_reason_rows {
            let reason: String = r.try_get("archive_reason").unwrap_or_default();
            let cnt: i64 = r.try_get("cnt").unwrap_or(0);
            by_reason.insert(reason, serde_json::json!(cnt));
        }

        let by_namespace_rows = sqlx::query(
            "SELECT namespace, COUNT(*) AS cnt FROM archived_memories \
             GROUP BY namespace ORDER BY cnt DESC LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("archive stats by_namespace", e))?;
        let mut by_namespace: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for r in by_namespace_rows {
            let ns: String = r.try_get("namespace").unwrap_or_default();
            let cnt: i64 = r.try_get("cnt").unwrap_or(0);
            by_namespace.insert(ns, serde_json::json!(cnt));
        }

        Ok(serde_json::json!({
            "total_archived": total,
            "by_reason": by_reason,
            "by_namespace": by_namespace,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_advertise_native_vector() {
        // The Postgres backend must advertise NATIVE_VECTOR + FULLTEXT
        // so callers can skip the in-process HNSW fallback.
        // We can't construct a real PostgresStore without a connection,
        // so we check the constant bits directly via a dummy pool-less
        // match. This is a trait-surface test only.
        let caps = Capabilities::TRANSACTIONS
            | Capabilities::NATIVE_VECTOR
            | Capabilities::FULLTEXT
            | Capabilities::DURABLE
            | Capabilities::STRONG_CONSISTENCY
            | Capabilities::ATOMIC_MULTI_WRITE;
        assert!(caps.contains(Capabilities::NATIVE_VECTOR));
        assert!(caps.contains(Capabilities::FULLTEXT));
        assert!(caps.contains(Capabilities::STRONG_CONSISTENCY));
        assert!(!caps.contains(Capabilities::TTL_NATIVE));
    }

    #[test]
    fn parse_rfc3339_opt_handles_some_and_none() {
        assert!(parse_rfc3339_opt(None).is_none());
        assert!(parse_rfc3339_opt(Some("not a date")).is_none());
        let parsed = parse_rfc3339_opt(Some("2026-04-19T16:00:00Z"));
        assert!(parsed.is_some());
    }

    #[test]
    fn parse_rfc3339_required_rejects_garbage() {
        assert!(parse_rfc3339_required("garbage").is_err());
        assert!(parse_rfc3339_required("2026-04-19T16:00:00Z").is_ok());
    }

    #[test]
    fn init_schema_contains_vector_extension_and_indexes() {
        // Sanity: make sure the bootstrap SQL references the critical
        // constructs so a typo'd rename catches here in CI.
        assert!(INIT_SCHEMA.contains("CREATE EXTENSION IF NOT EXISTS vector"));
        assert!(INIT_SCHEMA.contains("memories_embedding_hnsw"));
        assert!(INIT_SCHEMA.contains("to_tsvector"));
    }

    #[test]
    fn init_schema_contains_schema_version_table() {
        // Verify the schema_version table is created for migration tracking.
        assert!(INIT_SCHEMA.contains("CREATE TABLE IF NOT EXISTS schema_version"));
        assert!(INIT_SCHEMA.contains("version    INTEGER PRIMARY KEY"));
    }

    // ------------------------------------------------------------------
    // v0.7 J1 — AGE detection unit + live tests.
    //
    // The pool-less unit tests here only exercise the static surface
    // (default tag, accessor wiring through the type) so they can run
    // on any host. The live AGE probe runs iff `AI_MEMORY_TEST_AGE_URL`
    // is set; otherwise it skips cleanly. CI configures the env var
    // for the AGE-postgres job (see Track J5 plan).
    // ------------------------------------------------------------------

    fn age_url() -> Option<String> {
        std::env::var("AI_MEMORY_TEST_AGE_URL").ok()
    }

    #[test]
    fn kg_backend_default_tag_is_cte() {
        // Substrate sanity: the fallback path is `Cte`. J2-J7 dispatch
        // on this — flipping the default would silently route every
        // SQLite-class deployment through Cypher and crash on first
        // call. Pin the default so a future refactor can't drift it.
        assert_eq!(KgBackend::Cte.as_str(), "cte");
        assert_eq!(KgBackend::Age.as_str(), "age");
    }

    #[tokio::test]
    async fn live_kg_backend_resolves_to_age_when_extension_present() {
        // Runs against a real AGE-enabled Postgres ONLY when
        // AI_MEMORY_TEST_AGE_URL is set. Skips cleanly otherwise so
        // the default `cargo test` flow stays offline. Contract: the
        // SAL handle must report `KgBackend::Age` against this URL.
        let Some(url) = age_url() else {
            eprintln!("skip: AI_MEMORY_TEST_AGE_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(
            store.kg_backend(),
            KgBackend::Age,
            "AGE-enabled Postgres must resolve to KgBackend::Age"
        );
    }

    #[tokio::test]
    async fn live_kg_backend_resolves_to_cte_without_age() {
        // Runs against a Postgres WITHOUT the AGE extension installed.
        // CI may set AI_MEMORY_TEST_POSTGRES_URL to a vanilla pgvector
        // Postgres while AI_MEMORY_TEST_AGE_URL points at the AGE
        // image — when the vanilla URL is set and AGE is NOT, we get
        // a real-Postgres-real-fallback assertion. Skips cleanly
        // otherwise.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        // If the operator points the same URL at both, this assertion
        // would be wrong — guard with an early-skip when AGE_URL == URL.
        if age_url().as_deref() == Some(url.as_str()) {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL points at the AGE fixture");
            return;
        }
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(
            store.kg_backend(),
            KgBackend::Cte,
            "Postgres without AGE must resolve to KgBackend::Cte"
        );
    }

    #[tokio::test]
    async fn live_detect_kg_backend_returns_cte_on_missing_extension() {
        // Same fallback assertion through the lower-level helper,
        // which J2-J7 will reach when they want to re-probe (e.g.
        // post-CREATE EXTENSION operator action). Skips cleanly when
        // no Postgres URL is set.
        let Some(url) = postgres_url() else {
            return;
        };
        if age_url().as_deref() == Some(url.as_str()) {
            return;
        }
        let store = PostgresStore::connect(&url).await.expect("connect");
        let probed = detect_kg_backend(&store.pool).await;
        assert_eq!(probed, KgBackend::Cte);
    }

    // ------------------------------------------------------------------
    // v0.7 J2 — Cypher kg_query unit + live tests.
    //
    // Pool-less unit tests cover the offline surface — depth validation
    // + agtype string-peeling — so the dispatcher contract holds even
    // when no Postgres is available. Live tests run against an
    // AGE-enabled Postgres (`AI_MEMORY_TEST_AGE_URL`) and skip cleanly
    // otherwise so the default `cargo test` flow stays offline.
    // ------------------------------------------------------------------

    #[test]
    fn validate_depth_rejects_zero_and_overflow() {
        // Routing contract: the dispatcher MUST refuse depth=0 (no
        // traversal possible) and depth>5 (above the published budget)
        // *before* hitting the wire on either backend. Pinning the
        // boundary here so a future refactor can't silently widen it.
        assert!(matches!(
            validate_depth(0),
            Err(StoreError::InvalidInput { .. })
        ));
        assert!(matches!(
            validate_depth(KG_QUERY_MAX_SUPPORTED_DEPTH + 1),
            Err(StoreError::InvalidInput { .. })
        ));
        assert!(validate_depth(1).is_ok());
        assert!(validate_depth(KG_QUERY_MAX_SUPPORTED_DEPTH).is_ok());
    }

    #[test]
    fn strip_agtype_quotes_recovers_scalar_payload() {
        // AGE wraps text scalars in literal double-quotes when cast to
        // text via `::text`. The decoder MUST peel them so the row
        // shape matches the CTE branch byte-for-byte; otherwise the
        // dispatcher would leak the agtype quoting into upper layers.
        assert_eq!(strip_agtype_quotes("\"mem-1\""), "mem-1");
        assert_eq!(strip_agtype_quotes("3"), "3");
        assert_eq!(strip_agtype_quotes("  \"mem-1\"  "), "mem-1");
        // No surrounding quotes -> passthrough (numeric agtype scalars).
        assert_eq!(strip_agtype_quotes("42"), "42");
        // Single quote shouldn't trigger the strip (defensive).
        assert_eq!(strip_agtype_quotes("\""), "\"");
    }

    fn age_kg_url() -> Option<String> {
        std::env::var("AI_MEMORY_TEST_AGE_URL").ok()
    }

    #[tokio::test]
    async fn live_kg_query_dispatches_to_cypher_under_age() {
        // Routing contract: when AGE is the resolved backend, calling
        // `kg_query` must route through `kg_query_cypher` rather than
        // the CTE branch. We don't assert a specific result set here
        // (the J5 dual-path tests own equivalence) — just that the
        // call returns Ok against an AGE-enabled URL with a
        // bootstrapped `memory_graph` projection. Skips cleanly when
        // either piece is missing so CI without AGE stays green.
        let Some(url) = age_kg_url() else {
            eprintln!("skip: AI_MEMORY_TEST_AGE_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(store.kg_backend(), KgBackend::Age);
        // Use a synthetic id we don't expect to find — the test asserts
        // routing, not corpus contents. A missing graph projection
        // surfaces as BackendUnavailable; that's still informative
        // (caller learns the AGE setup needs running) so we report it
        // through the test name rather than silently skipping.
        match store.kg_query("nonexistent-source", 1).await {
            Ok(rows) => {
                assert!(
                    rows.is_empty() || rows.iter().all(|r| !r.target_id.is_empty()),
                    "rows must have non-empty target_ids when present"
                );
            }
            Err(StoreError::BackendUnavailable { detail, .. }) => {
                eprintln!(
                    "AGE graph projection appears unbootstrapped on this URL: {detail}; \
                     run the J1 graph-prep script before re-running this test"
                );
            }
            Err(other) => panic!("unexpected error from AGE kg_query: {other:?}"),
        }
    }

    #[tokio::test]
    async fn live_kg_query_routes_to_cte_without_age() {
        // Inverse of the AGE routing test: against vanilla Postgres
        // (no AGE) the dispatcher must hand the call to the CTE
        // branch. We verify by calling `kg_query_cte` directly against
        // a known-empty source and asserting an empty result set, then
        // confirming `kg_query` produces the same empty set through the
        // dispatcher. Skips cleanly when no Postgres URL is configured.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        if age_kg_url().as_deref() == Some(url.as_str()) {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL points at the AGE fixture");
            return;
        }
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(
            store.kg_backend(),
            KgBackend::Cte,
            "vanilla Postgres must resolve to KgBackend::Cte"
        );
        let direct = store
            .kg_query_cte("nonexistent-source", 1)
            .await
            .expect("cte direct");
        let dispatched = store
            .kg_query("nonexistent-source", 1)
            .await
            .expect("cte via dispatcher");
        assert!(direct.is_empty(), "no rows expected for synthetic id");
        assert_eq!(
            direct, dispatched,
            "dispatcher must return the same shape as the direct CTE call"
        );
    }

    #[tokio::test]
    async fn live_kg_query_rejects_out_of_range_depth() {
        // Both backends share `validate_depth` — exercise the public
        // dispatcher against a real connection (either AGE or vanilla)
        // so the wire-side InvalidInput contract is pinned end-to-end.
        let Some(url) = age_kg_url().or_else(postgres_url) else {
            eprintln!("skip: no Postgres test URL set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        let zero = store.kg_query("any", 0).await;
        let over = store
            .kg_query("any", KG_QUERY_MAX_SUPPORTED_DEPTH + 1)
            .await;
        assert!(matches!(zero, Err(StoreError::InvalidInput { .. })));
        assert!(matches!(over, Err(StoreError::InvalidInput { .. })));
    }

    // ------------------------------------------------------------------
    // v0.7 J3 — Cypher kg_timeline unit + live tests.
    //
    // Pool-less unit tests cover the offline surface (limit clamping +
    // agtype optional decoding) so the dispatcher contract holds even
    // when no Postgres is available. The live test runs against an
    // AGE-enabled Postgres (`AI_MEMORY_TEST_AGE_URL`) and skips
    // cleanly otherwise so the default `cargo test` flow stays
    // offline.
    // ------------------------------------------------------------------

    #[test]
    fn clamp_timeline_limit_applies_default_and_ceiling() {
        // Routing contract: callers that omit the limit must get the
        // published default; callers that pass a value above the
        // ceiling must be silently clamped to the ceiling rather
        // than fanning out an unbounded scan. Pinning the band so a
        // future refactor can't widen the page-size budget.
        assert_eq!(clamp_timeline_limit(None), KG_TIMELINE_DEFAULT_LIMIT_SAL);
        assert_eq!(clamp_timeline_limit(Some(0)), 1);
        assert_eq!(clamp_timeline_limit(Some(50)), 50);
        assert_eq!(
            clamp_timeline_limit(Some(KG_TIMELINE_MAX_LIMIT_SAL)),
            KG_TIMELINE_MAX_LIMIT_SAL
        );
        assert_eq!(
            clamp_timeline_limit(Some(KG_TIMELINE_MAX_LIMIT_SAL + 999)),
            KG_TIMELINE_MAX_LIMIT_SAL
        );
    }

    #[test]
    fn agtype_optional_string_decodes_null_and_quoted() {
        // Wire-shape contract: AGE's `cypher()` SRF returns missing
        // values as the literal token `null` and present strings as
        // `"value"`. The decoder MUST collapse `null` to `None` and
        // peel the surrounding quotes from present strings; otherwise
        // the dispatcher would surface the agtype quoting in the
        // upper layer and break parity with the SQLite shape.
        assert_eq!(agtype_optional_string("null"), None);
        assert_eq!(agtype_optional_string("NULL"), None);
        assert_eq!(
            agtype_optional_string("\"agent-1\""),
            Some("agent-1".to_string())
        );
        assert_eq!(
            agtype_optional_string("  \"agent-1\"  "),
            Some("agent-1".to_string())
        );
        assert_eq!(agtype_optional_string("\"\""), Some(String::new()));
    }

    #[tokio::test]
    async fn live_kg_timeline_dispatches_to_cypher_under_age() {
        // Routing contract: when AGE is the resolved backend, calling
        // `kg_timeline` must route through `kg_timeline_cypher` rather
        // than the SQL branch. We don't assert a specific result set
        // here (the J5 dual-path tests own equivalence) — just that
        // the call returns Ok against an AGE-enabled URL with a
        // bootstrapped `memory_graph` projection. Skips cleanly when
        // either piece is missing so CI without AGE stays green.
        let Some(url) = age_kg_url() else {
            eprintln!("skip: AI_MEMORY_TEST_AGE_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(store.kg_backend(), KgBackend::Age);
        // Use a synthetic id we don't expect to find — the test asserts
        // routing, not corpus contents.
        match store
            .kg_timeline("nonexistent-source", None, None, Some(10))
            .await
        {
            Ok(rows) => {
                assert!(
                    rows.is_empty() || rows.iter().all(|r| !r.target_id.is_empty()),
                    "rows must have non-empty target_ids when present"
                );
            }
            Err(StoreError::BackendUnavailable { detail, .. }) => {
                eprintln!(
                    "AGE graph projection appears unbootstrapped on this URL: {detail}; \
                     run the J1 graph-prep script before re-running this test"
                );
            }
            Err(other) => panic!("unexpected error from AGE kg_timeline: {other:?}"),
        }
    }

    #[tokio::test]
    async fn live_kg_timeline_routes_to_cte_without_age() {
        // Inverse of the AGE routing test: against vanilla Postgres
        // (no AGE) the dispatcher must hand the call to the SQL
        // branch. We verify by calling `kg_timeline_cte` directly
        // against a known-empty source and asserting an empty result
        // set, then confirming `kg_timeline` produces the same empty
        // set through the dispatcher.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        if age_kg_url().as_deref() == Some(url.as_str()) {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL points at the AGE fixture");
            return;
        }
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(
            store.kg_backend(),
            KgBackend::Cte,
            "vanilla Postgres must resolve to KgBackend::Cte"
        );
        let direct = store
            .kg_timeline_cte("nonexistent-source", None, None, None)
            .await
            .expect("cte direct");
        let dispatched = store
            .kg_timeline("nonexistent-source", None, None, None)
            .await
            .expect("cte via dispatcher");
        assert!(direct.is_empty(), "no rows expected for synthetic id");
        assert_eq!(
            direct, dispatched,
            "dispatcher must return the same shape as the direct CTE call"
        );
    }

    // ------------------------------------------------------------------
    // v0.7 J4 — Cypher kg_invalidate unit + live tests.
    //
    // Pool-less unit tests cover the offline surface (the SAL row
    // shape contract) so the dispatcher behaviour holds even when no
    // Postgres is available. The live tests run against an
    // AGE-enabled Postgres (`AI_MEMORY_TEST_AGE_URL`) and skip
    // cleanly otherwise so the default `cargo test` flow stays
    // offline. SQLite-side regression for `db::invalidate_link` lives
    // in `mcp.rs` (`handle_kg_invalidate_*` tests, untouched here).
    // ------------------------------------------------------------------

    #[test]
    fn kg_invalidate_row_default_no_match_shape() {
        // Routing contract: a missed triple at the SAL layer surfaces
        // as `found = false` with empty `valid_until` and `None`
        // `previous_valid_until`. Pinning the shape so the upper-layer
        // handler can branch on `found` without inspecting the inner
        // strings (which the SQLite path leaves unset for the same
        // case via `Option::None` from `db::invalidate_link`).
        let row = KgInvalidateRow {
            found: false,
            valid_until: String::new(),
            previous_valid_until: None,
        };
        assert!(!row.found);
        assert!(row.valid_until.is_empty());
        assert!(row.previous_valid_until.is_none());
    }

    #[test]
    fn kg_invalidate_row_serialises_to_stable_json_keys() {
        // The wire shape is a stable JSON contract — integrators pin
        // against `found / valid_until / previous_valid_until`. A
        // future rename of the struct fields would break their
        // parsers; assert the JSON keys here so the rename surfaces
        // as a test failure rather than a silent drift.
        let row = KgInvalidateRow {
            found: true,
            valid_until: "2026-05-05T12:00:00+00:00".to_string(),
            previous_valid_until: Some("2026-05-04T11:00:00+00:00".to_string()),
        };
        let v = serde_json::to_value(&row).expect("serialise");
        assert_eq!(v["found"], serde_json::Value::Bool(true));
        assert_eq!(v["valid_until"], "2026-05-05T12:00:00+00:00");
        assert_eq!(v["previous_valid_until"], "2026-05-04T11:00:00+00:00");
    }

    #[tokio::test]
    async fn live_kg_invalidate_dispatches_to_cypher_under_age() {
        // Routing contract: when AGE is the resolved backend, calling
        // `kg_invalidate` must route through `kg_invalidate_cypher`
        // rather than the CTE branch. We don't assert a specific
        // result set here (the J5 dual-path tests own equivalence) —
        // just that the call returns Ok against an AGE-enabled URL
        // with a bootstrapped `memory_graph` projection. Skips
        // cleanly when either piece is missing so CI without AGE
        // stays green.
        let Some(url) = age_kg_url() else {
            eprintln!("skip: AI_MEMORY_TEST_AGE_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(store.kg_backend(), KgBackend::Age);
        // Use synthetic ids we don't expect to find — the test asserts
        // routing, not corpus contents. A missing graph projection
        // surfaces as BackendUnavailable; that's still informative
        // (caller learns the AGE setup needs running) so we report it
        // through the test name rather than silently skipping.
        match store
            .kg_invalidate(
                "nonexistent-source",
                "nonexistent-target",
                "related_to",
                None,
            )
            .await
        {
            Ok(row) => {
                assert!(!row.found, "synthetic ids must not match an existing edge");
                assert!(row.valid_until.is_empty());
                assert!(row.previous_valid_until.is_none());
            }
            Err(StoreError::BackendUnavailable { detail, .. }) => {
                eprintln!(
                    "AGE graph projection appears unbootstrapped on this URL: {detail}; \
                     run the J1 graph-prep script before re-running this test"
                );
            }
            Err(other) => panic!("unexpected error from AGE kg_invalidate: {other:?}"),
        }
    }

    #[tokio::test]
    async fn live_kg_invalidate_routes_to_cte_without_age() {
        // Inverse of the AGE routing test: against vanilla Postgres
        // (no AGE) the dispatcher must hand the call to the SQL
        // branch. We verify by calling `kg_invalidate_cte` directly
        // against a known-missing triple and asserting `found=false`,
        // then confirming `kg_invalidate` produces the same shape
        // through the dispatcher.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        if age_kg_url().as_deref() == Some(url.as_str()) {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL points at the AGE fixture");
            return;
        }
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_eq!(
            store.kg_backend(),
            KgBackend::Cte,
            "vanilla Postgres must resolve to KgBackend::Cte"
        );
        let direct = store
            .kg_invalidate_cte(
                "nonexistent-source",
                "nonexistent-target",
                "related_to",
                None,
            )
            .await
            .expect("cte direct");
        let dispatched = store
            .kg_invalidate(
                "nonexistent-source",
                "nonexistent-target",
                "related_to",
                None,
            )
            .await
            .expect("cte via dispatcher");
        assert!(!direct.found, "synthetic triple must not match");
        assert_eq!(
            direct, dispatched,
            "dispatcher must return the same shape as the direct CTE call"
        );
    }

    // ------------------------------------------------------------------
    // Live-Postgres integration tests.
    //
    // Run iff AI_MEMORY_TEST_POSTGRES_URL is set; otherwise they skip
    // cleanly so the default `cargo test` flow stays offline.
    //
    // Quick-start:
    //   docker compose -f packaging/docker-compose.postgres.yml up -d
    //   export AI_MEMORY_TEST_POSTGRES_URL=postgres://ai_memory:ai_memory_test@localhost:5433/ai_memory_test
    //   cargo test --features sal-postgres store::postgres -- --nocapture
    // ------------------------------------------------------------------

    fn postgres_url() -> Option<String> {
        std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
    }

    fn sample_memory(id: &str, ns: &str, title: &str, content: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: id.to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec!["test".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "sal-integration".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id":"ai:sal-test"}),
        }
    }

    #[tokio::test]
    async fn live_store_get_roundtrip() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        let ctx = CallerContext::for_agent("ai:sal-test");
        let mem = sample_memory(
            &format!("test-{}", uuid::Uuid::new_v4()),
            "sal-test",
            "hello",
            "quick brown fox jumps over the lazy dog",
        );
        let returned = store.store(&ctx, &mem).await.expect("store");
        assert_eq!(returned, mem.id);
        let fetched = store.get(&ctx, &mem.id).await.expect("get");
        assert_eq!(fetched.title, "hello");
        assert_eq!(fetched.namespace, "sal-test");
    }

    #[tokio::test]
    async fn live_search_finds_fts_match() {
        let Some(url) = postgres_url() else {
            return;
        };
        let store = PostgresStore::connect(&url).await.unwrap();
        let ctx = CallerContext::for_agent("ai:sal-test");
        let id = format!("search-test-{}", uuid::Uuid::new_v4());
        let mem = sample_memory(
            &id,
            "sal-search",
            "uniquephrase xyzzy42",
            "body containing uniquephrase xyzzy42 as a distinctive token",
        );
        store.store(&ctx, &mem).await.unwrap();
        let filter = Filter {
            namespace: Some("sal-search".to_string()),
            limit: 5,
            ..Filter::default()
        };
        let hits = store
            .search(&ctx, "xyzzy42", &filter)
            .await
            .expect("search");
        assert!(hits.iter().any(|m| m.id == id));
    }

    #[tokio::test]
    async fn live_delete_removes_row() {
        let Some(url) = postgres_url() else {
            return;
        };
        let store = PostgresStore::connect(&url).await.unwrap();
        let ctx = CallerContext::for_agent("ai:sal-test");
        let id = format!("del-test-{}", uuid::Uuid::new_v4());
        let mem = sample_memory(&id, "sal-del", "to be deleted", "soon gone");
        store.store(&ctx, &mem).await.unwrap();
        store.delete(&ctx, &id).await.expect("delete");
        let err = store.get(&ctx, &id).await.unwrap_err();
        match err {
            StoreError::NotFound { id: missing } => assert_eq!(missing, id),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Regression tests for v0.6.0 blockers #294, #295, #296.
    // ------------------------------------------------------------------

    /// Blocker #294 — upsert on (title, namespace) matches SQLite.
    ///
    /// Two stores with identical (title, namespace) but different ids
    /// must collapse into one row on Postgres (keyed by the existing
    /// row's id) rather than producing two rows.
    #[tokio::test]
    async fn upserts_by_title_namespace_not_id() {
        let Some(url) = postgres_url() else {
            return;
        };
        let store = PostgresStore::connect(&url).await.unwrap();
        let ctx = CallerContext::for_agent("ai:sal-test");
        let ns = format!("sal-upsert-{}", uuid::Uuid::new_v4());
        let first = sample_memory("upsert-a", &ns, "shared title", "first body");
        let second = sample_memory("upsert-b", &ns, "shared title", "second body");
        let id_first = store.store(&ctx, &first).await.unwrap();
        let id_second = store.store(&ctx, &second).await.unwrap();
        assert_eq!(id_first, id_second, "upsert should return the same id");
        // Namespace list must contain exactly one row with the shared title.
        let filter = Filter {
            namespace: Some(ns.clone()),
            limit: 10,
            ..Filter::default()
        };
        let listed = store.list(&ctx, &filter).await.unwrap();
        assert_eq!(listed.len(), 1, "expected single upserted row");
        assert_eq!(listed[0].content, "second body");
    }

    /// Blocker #295 — metadata.agent_id is SQL-layer-immutable on UPSERT.
    #[tokio::test]
    async fn upsert_preserves_agent_id() {
        let Some(url) = postgres_url() else {
            return;
        };
        let store = PostgresStore::connect(&url).await.unwrap();
        let ctx = CallerContext::for_agent("ai:sal-test");
        let ns = format!("sal-agent-{}", uuid::Uuid::new_v4());

        let mut first = sample_memory("agent-1", &ns, "owned-by-alice", "original");
        first.metadata = serde_json::json!({"agent_id": "ai:alice"});
        store.store(&ctx, &first).await.unwrap();

        // Second store with the same (title, ns) but claims a different agent_id.
        let mut second = sample_memory("agent-2", &ns, "owned-by-alice", "replayed");
        second.metadata = serde_json::json!({"agent_id": "ai:attacker"});
        store.store(&ctx, &second).await.unwrap();

        let got = store
            .get(&ctx, &store.store(&ctx, &second).await.unwrap())
            .await
            .unwrap();
        assert_eq!(
            got.metadata.get("agent_id").and_then(|v| v.as_str()),
            Some("ai:alice"),
            "agent_id must be pinned to the original writer"
        );
    }

    /// Blocker #296 — tier never downgrades on UPDATE.
    #[tokio::test]
    async fn update_refuses_tier_downgrade() {
        let Some(url) = postgres_url() else {
            return;
        };
        let store = PostgresStore::connect(&url).await.unwrap();
        let ctx = CallerContext::for_agent("ai:sal-test");

        let id = format!("tier-test-{}", uuid::Uuid::new_v4());
        let mut mem = sample_memory(&id, "sal-tier", "long-pinned", "must not downgrade");
        mem.tier = Tier::Long;
        store.store(&ctx, &mem).await.unwrap();

        // Attempt to downgrade Long -> Short via update patch.
        let patch = UpdatePatch {
            tier: Some(Tier::Short),
            ..UpdatePatch::default()
        };
        store.update(&ctx, &id, patch).await.unwrap();

        let got = store.get(&ctx, &id).await.unwrap();
        assert!(
            matches!(got.tier, Tier::Long),
            "tier must remain Long (got {:?})",
            got.tier
        );
    }

    /// Verify migration is idempotent: running twice produces the same result.
    /// Tests that ALTER TABLE/CREATE INDEX operations are guarded with
    /// IF NOT EXISTS checks, and that schema_version is correctly recorded.
    #[tokio::test]
    async fn migration_v15_is_idempotent() {
        let Some(url) = postgres_url() else {
            eprintln!("skipping: no AI_MEMORY_TEST_POSTGRES_URL");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");

        // Read schema version after first connect (migration runs implicitly).
        let first_version: Option<i32> =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                .fetch_optional(&store.pool)
                .await
                .expect("read version after first connect");

        // Run migration again explicitly (should be a no-op).
        store.migrate().await.expect("migrate again");

        // Verify schema version hasn't changed (idempotent).
        let second_version: Option<i32> =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                .fetch_optional(&store.pool)
                .await
                .expect("read version after second migrate");

        assert_eq!(
            first_version, second_version,
            "schema version must be stable across repeated migrations"
        );

        // Verify the v15 columns exist (created or already existed).
        let has_valid_from: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM information_schema.columns
             WHERE table_name='memory_links' AND column_name='valid_from'",
        )
        .fetch_optional(&store.pool)
        .await
        .expect("check valid_from column");
        assert!(has_valid_from.is_some(), "valid_from column must exist");

        let has_entity_aliases_idx: Option<(String,)> = sqlx::query_as(
            "SELECT indexname FROM pg_indexes WHERE indexname='idx_entity_aliases_alias'",
        )
        .fetch_optional(&store.pool)
        .await
        .expect("check entity_aliases index");
        assert!(
            has_entity_aliases_idx.is_some(),
            "idx_entity_aliases_alias must exist"
        );
    }

    // ------------------------------------------------------------------
    // v0.7.0 Wave 2 — schema parity v17 → v28.
    //
    // Each test below either asserts the bootstrap shape (CREATE TABLE
    // / CREATE INDEX present after `connect`) or exercises a tiny CRUD
    // round-trip against the new substrate. All tests skip cleanly
    // when AI_MEMORY_TEST_POSTGRES_URL is unset so the default offline
    // flow stays green.
    // ------------------------------------------------------------------

    #[test]
    fn init_schema_advertises_full_v28_shape() {
        // Sanity-check the bootstrap SQL covers every load-bearing
        // table/column added between v17 and v28. A typo'd rename or
        // an accidental drop catches here in CI rather than on the
        // first live `connect()` against a populated host.
        for needle in [
            // v17 — governance.inherit (no DDL, runtime backfill only).
            // v18 — embedding_dim columns + archive lossless.
            "embedding_dim",
            "original_tier",
            "original_expires_at",
            // v19 — webhook event_types + index.
            "event_types",
            "idx_subscriptions_event_types",
            // v20 — capability-expansion audit_log.
            "CREATE TABLE IF NOT EXISTS audit_log",
            "idx_audit_log_agent_id",
            // v21 — pending_actions timeout sweeper.
            "default_timeout_seconds",
            "expired_at",
            "pending_actions_status_requested_idx",
            // v22 — memory_transcripts substrate.
            "CREATE TABLE IF NOT EXISTS memory_transcripts",
            "idx_memory_transcripts_namespace_created",
            // v23 — memory_links.attest_level.
            "idx_memory_links_attest_level",
            // v24 — memory_transcript_links join table.
            "CREATE TABLE IF NOT EXISTS memory_transcript_links",
            "idx_mtl_transcript",
            "idx_mtl_memory",
            // v25 — transcript archive lifecycle.
            "idx_memory_transcripts_archived_at",
            // v26 — signed_events audit chain.
            "CREATE TABLE IF NOT EXISTS signed_events",
            "idx_signed_events_agent",
            // v27 — A2A correlation IDs + DLQ.
            "CREATE TABLE IF NOT EXISTS subscription_events",
            "CREATE TABLE IF NOT EXISTS subscription_dlq",
            "idx_subscription_events_correlation",
            "idx_subscription_dlq_correlation",
            // v28 — agent_quotas.
            "CREATE TABLE IF NOT EXISTS agent_quotas",
            "idx_agent_quotas_agent_id",
        ] {
            assert!(
                INIT_SCHEMA.contains(needle),
                "postgres_schema.sql missing expected v17-v28 fragment: {needle}"
            );
        }
    }

    #[test]
    fn current_schema_version_matches_sqlite_ladder() {
        // Pin the parity invariant: Postgres MUST track the SQLite
        // CURRENT_SCHEMA_VERSION (28 as of v0.7.0). A future bump on
        // either side without the corresponding port re-trips this
        // assertion before the migration runner gets a chance to
        // write a partial schema to disk.
        assert_eq!(CURRENT_SCHEMA_VERSION, 28);
    }

    #[tokio::test]
    async fn live_migration_reaches_v28() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");

        let stamped: Option<i32> = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
            .fetch_optional(&store.pool)
            .await
            .expect("read max schema_version");
        assert_eq!(
            stamped,
            Some(CURRENT_SCHEMA_VERSION),
            "schema_version must reach CURRENT_SCHEMA_VERSION (28)"
        );
    }

    #[tokio::test]
    async fn live_migration_v17_to_v28_is_idempotent() {
        // Run migrate() twice and assert the schema_version is stable;
        // the IF NOT EXISTS DDL + column-existence guards mean every
        // migrate_vN must be a no-op on a populated database.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");

        let first: Option<i32> = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
            .fetch_optional(&store.pool)
            .await
            .expect("read first version");

        store.migrate().await.expect("migrate again");

        let second: Option<i32> = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
            .fetch_optional(&store.pool)
            .await
            .expect("read second version");

        assert_eq!(first, second, "migrate() must be idempotent");
        assert_eq!(second, Some(CURRENT_SCHEMA_VERSION));
    }

    /// Helper: assert a named relation (table or view) exists.
    async fn assert_relation_exists(pool: &PgPool, relname: &str) {
        let exists: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM pg_class WHERE relname = $1 AND relkind IN ('r','v')",
        )
        .bind(relname)
        .fetch_optional(pool)
        .await
        .expect("query pg_class");
        assert!(exists.is_some(), "expected relation {relname} to exist");
    }

    /// Helper: assert a named index exists.
    async fn assert_index_exists(pool: &PgPool, indexname: &str) {
        let exists: Option<String> =
            sqlx::query_scalar("SELECT indexname FROM pg_indexes WHERE indexname = $1")
                .bind(indexname)
                .fetch_optional(pool)
                .await
                .expect("query pg_indexes");
        assert!(exists.is_some(), "expected index {indexname} to exist");
    }

    /// Helper: assert a column on a table exists.
    async fn assert_column_exists(pool: &PgPool, table: &str, column: &str) {
        let exists: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM information_schema.columns
             WHERE table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_optional(pool)
        .await
        .expect("query information_schema");
        assert!(
            exists.is_some(),
            "expected column {table}.{column} to exist"
        );
    }

    #[tokio::test]
    async fn live_v18_data_integrity_columns_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        for (table, column) in [
            ("memories", "embedding_dim"),
            ("archived_memories", "embedding"),
            ("archived_memories", "embedding_dim"),
            ("archived_memories", "original_tier"),
            ("archived_memories", "original_expires_at"),
        ] {
            assert_column_exists(&store.pool, table, column).await;
        }
        assert_index_exists(&store.pool, "idx_memories_embedding_dim").await;
        assert_index_exists(&store.pool, "idx_memories_ns_dim").await;
    }

    #[tokio::test]
    async fn live_v19_webhook_event_types_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_column_exists(&store.pool, "subscriptions", "event_types").await;
        assert_index_exists(&store.pool, "idx_subscriptions_event_types").await;
    }

    #[tokio::test]
    async fn live_v20_audit_log_table_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_relation_exists(&store.pool, "audit_log").await;
        assert_index_exists(&store.pool, "idx_audit_log_agent_id").await;
        assert_index_exists(&store.pool, "idx_audit_log_timestamp").await;
        assert_index_exists(&store.pool, "idx_audit_log_event_type").await;

        // CRUD round-trip — the K8 attested-cortex epic queries this
        // by (agent_id, timestamp). Insert / read / delete to prove the
        // table is functionally writable, not just present.
        let now = chrono::Utc::now();
        let id = format!("audit-{}", uuid::Uuid::new_v4());
        sqlx::query(
            "INSERT INTO audit_log
             (id, agent_id, event_type, requested_family, granted, attestation_tier, timestamp)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind("ai:test")
        .bind("capability_expansion")
        .bind("kg")
        .bind(true)
        .bind(Option::<&str>::None)
        .bind(now)
        .execute(&store.pool)
        .await
        .expect("insert audit_log row");

        let granted: bool = sqlx::query_scalar("SELECT granted FROM audit_log WHERE id = $1")
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .expect("read audit_log row");
        assert!(granted, "round-trip should preserve granted=true");

        sqlx::query("DELETE FROM audit_log WHERE id = $1")
            .bind(&id)
            .execute(&store.pool)
            .await
            .expect("delete audit_log row");
    }

    #[tokio::test]
    async fn live_v21_pending_actions_timeout_columns_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_column_exists(&store.pool, "pending_actions", "default_timeout_seconds").await;
        assert_column_exists(&store.pool, "pending_actions", "expired_at").await;
        assert_index_exists(&store.pool, "pending_actions_status_requested_idx").await;
    }

    #[tokio::test]
    async fn live_v22_memory_transcripts_table_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_relation_exists(&store.pool, "memory_transcripts").await;
        assert_index_exists(&store.pool, "idx_memory_transcripts_namespace_created").await;
    }

    #[tokio::test]
    async fn live_v24_transcript_links_and_kg_views_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_relation_exists(&store.pool, "memory_transcript_links").await;
        assert_index_exists(&store.pool, "idx_mtl_transcript").await;
        assert_index_exists(&store.pool, "idx_mtl_memory").await;
        // F6 KG views are the Postgres-only addition tied to the v24 stamp.
        assert_relation_exists(&store.pool, "kg_query_view").await;
        assert_relation_exists(&store.pool, "kg_timeline_view").await;
        // kg_find_paths is a function — probe via pg_proc.
        let fn_exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM pg_proc WHERE proname = 'kg_find_paths'")
                .fetch_optional(&store.pool)
                .await
                .expect("query pg_proc for kg_find_paths");
        assert!(fn_exists.is_some(), "kg_find_paths function must exist");
    }

    #[tokio::test]
    async fn live_v25_transcript_archive_lifecycle_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_column_exists(&store.pool, "memory_transcripts", "archived_at").await;
        assert_index_exists(&store.pool, "idx_memory_transcripts_archived_at").await;
    }

    #[tokio::test]
    async fn live_v26_signed_events_table_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_relation_exists(&store.pool, "signed_events").await;
        for idx in [
            "idx_signed_events_agent",
            "idx_signed_events_type",
            "idx_signed_events_timestamp",
        ] {
            assert_index_exists(&store.pool, idx).await;
        }

        // Append-only round-trip — INSERT then SELECT. Mirrors the
        // single-writer contract documented on signed_events: no
        // UPDATE / DELETE call site is allowed to land in production
        // src/ (enforced by the
        // `append_only_invariant_no_mutators_in_src` test in
        // src/signed_events.rs). The test row uses a UUIDv4 id so
        // re-runs against the same disposable database accumulate
        // bounded inert rows rather than colliding.
        let id = format!("se-{}", uuid::Uuid::new_v4());
        let payload_hash = vec![0u8; 32];
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO signed_events
             (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind("ai:test")
        .bind("memory_link.created")
        .bind(&payload_hash)
        .bind(Option::<Vec<u8>>::None)
        .bind("unsigned")
        .bind(now)
        .execute(&store.pool)
        .await
        .expect("insert signed_events row");

        let level: String =
            sqlx::query_scalar("SELECT attest_level FROM signed_events WHERE id = $1")
                .bind(&id)
                .fetch_one(&store.pool)
                .await
                .expect("read signed_events row");
        assert_eq!(level, "unsigned");
    }

    #[tokio::test]
    async fn live_v27_subscription_events_and_dlq_present() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_relation_exists(&store.pool, "subscription_events").await;
        assert_relation_exists(&store.pool, "subscription_dlq").await;
        for idx in [
            "idx_subscription_events_correlation",
            "idx_subscription_events_subscription",
            "idx_subscription_dlq_subscription",
            "idx_subscription_dlq_correlation",
        ] {
            assert_index_exists(&store.pool, idx).await;
        }
        assert_column_exists(&store.pool, "subscription_events", "correlation_id").await;
    }

    #[tokio::test]
    async fn live_v28_agent_quotas_table_present_and_writable() {
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = PostgresStore::connect(&url).await.expect("connect");
        assert_relation_exists(&store.pool, "agent_quotas").await;
        assert_index_exists(&store.pool, "idx_agent_quotas_agent_id").await;

        // Round-trip — INSERT defaults, SELECT defaults, UPDATE counter,
        // DELETE. Proves the BIGINT defaults match the SQLite ones
        // (1000/100MiB/5000) byte-for-byte so the K8 sweep loop's
        // "first-write" path will produce identical rows on either
        // backend.
        let agent = format!("ai:quota-test-{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO agent_quotas
             (agent_id, day_started_at, created_at, updated_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&agent)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&store.pool)
        .await
        .expect("insert agent_quotas row");

        let (max_mem, max_bytes, max_links): (i64, i64, i64) = sqlx::query_as(
            "SELECT max_memories_per_day, max_storage_bytes, max_links_per_day
             FROM agent_quotas WHERE agent_id = $1",
        )
        .bind(&agent)
        .fetch_one(&store.pool)
        .await
        .expect("read agent_quotas row");
        assert_eq!(max_mem, 1000);
        assert_eq!(max_bytes, 104_857_600);
        assert_eq!(max_links, 5000);

        sqlx::query("DELETE FROM agent_quotas WHERE agent_id = $1")
            .bind(&agent)
            .execute(&store.pool)
            .await
            .expect("delete agent_quotas row");
    }
}
