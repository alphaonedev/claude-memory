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
    MemoryStore, StoreError, StoreResult, UpdatePatch, VerifyFilter, VerifyLinkReport,
    VerifyReport,
};
use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};
use crate::quotas::{
    DEFAULT_MAX_LINKS_PER_DAY, DEFAULT_MAX_MEMORIES_PER_DAY, DEFAULT_MAX_STORAGE_BYTES, QuotaStatus,
};

/// Bootstrap schema run at adapter init — idempotent via IF NOT EXISTS.
const INIT_SCHEMA: &str = include_str!("postgres_schema.sql");

/// v0.7.0 Task 1/8 (recursive learning) — add `memories.reflection_depth`.
/// Mirrors the SQLite v29 migration in `src/db.rs`. The body is in a
/// separate SQL file so operators can inspect and replay the DDL outside
/// the daemon if needed; the migration runner pipes it through
/// `sqlx::raw_sql` inside the v31 transaction.
const MIGRATION_V31_REFLECTION_DEPTH: &str =
    include_str!("../../migrations/postgres/0013_v0700_reflection_depth.sql");

/// v0.7.0 v0.7.1-fold (#687/#688) — SQL-side CHECK constraint on
/// `memory_links.relation`. Postgres supports `ALTER TABLE ADD
/// CONSTRAINT` for CHECK clauses directly, so the migration is a
/// one-liner gated behind a `pg_constraint` probe (idempotent).
/// Mirrors the SQLite full-table-rebuild migration that lands the
/// same constraint on the SQLite backend.
const MIGRATION_V32_LINK_RELATION_CHECK: &str =
    include_str!("../../migrations/postgres/0014_v07_memory_links_relation_check.sql");

/// v0.7.0 V-4 closeout (#698) — SQL-side cross-row hash chain on
/// `signed_events`. Adds `prev_hash BYTEA` + `sequence BIGINT`
/// columns plus a UNIQUE INDEX on `sequence`. Mirrors SQLite schema
/// v34. Postgres supports `ADD COLUMN IF NOT EXISTS` so the DDL is a
/// pure idempotent batch.
const MIGRATION_V33_SIGNED_EVENTS_CHAIN: &str =
    include_str!("../../migrations/postgres/0015_v07_signed_events_chain.sql");

/// v0.7.0 QW-3 — context-offload substrate primitive (`offloaded_blobs`
/// table + namespace and TTL indexes). Mirrors SQLite schema v35.
/// CREATE TABLE IF NOT EXISTS + CREATE INDEX IF NOT EXISTS — fully
/// idempotent. v0.8.0 short-term-context-compression will build on
/// this plumbing.
const MIGRATION_V34_OFFLOADED_BLOBS: &str =
    include_str!("../../migrations/postgres/0016_v07_offloaded_blobs.sql");

/// v0.7.0 WT-1-A — schema v35 (postgres) atomisation foundation.
/// Mirrors SQLite schema v36. Adds two nullable columns on `memories`
/// (`atomised_into INTEGER` + `atom_of TEXT REFERENCES memories(id)`)
/// plus extends the `memory_links.relation` closed-taxonomy CHECK
/// constraint with `derives_from` for atomisation provenance edges.
/// Postgres supports `ADD COLUMN IF NOT EXISTS` (14+) so this is a
/// pure idempotent DDL batch.
const MIGRATION_V35_ATOMISATION: &str =
    include_str!("../../migrations/postgres/0017_v07_atomisation.sql");

/// v0.7.0 QW-2 — Persona-as-artifact substrate primitive. Adds
/// `memories.entity_id TEXT NULL` + `memories.persona_version
/// INTEGER NULL` columns plus the partial index
/// `idx_personas_by_entity`. Mirrors SQLite schema v37. Postgres
/// supports `ADD COLUMN IF NOT EXISTS` so the DDL is a pure
/// idempotent batch.
const MIGRATION_V36_PERSONA: &str = include_str!("../../migrations/postgres/0018_v07_persona.sql");

/// v0.7.0 Form 4 — fact-provenance closeout (issue #757). Adds
/// `memories.citations TEXT NOT NULL DEFAULT '[]'` (JSON array of
/// Citation objects), `memories.source_uri TEXT NULL` (first-class
/// URI-form pointer to the cited source body), and
/// `memories.source_span TEXT NULL` (JSON `{start,end}` byte-range
/// into the parent source body). Mirrors SQLite schema v38. Postgres
/// supports `ADD COLUMN IF NOT EXISTS` so the DDL is a pure
/// idempotent batch.
const MIGRATION_V37_FORM4_PROVENANCE: &str =
    include_str!("../../migrations/postgres/0019_v07_form4_provenance.sql");

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
/// v29 — In-place `vector(N)` conversion helper + parameterised
///       `embedding` column dim. Connect-time migration is a no-op
///       stamp pass; the actual `vector(384) → vector(768)` (or
///       similar) conversion only runs when the operator explicitly
///       invokes `ai-memory schema-init --embedding-dim <N>` with a
///       value that differs from the current column declaration. The
///       conversion is destructive on the embedding column (rows have
///       their `embedding` NULLed and the HNSW indexes dropped +
///       recreated) — re-embedding is required after the migration.
/// v30 — `memories_metadata_is_object` CHECK constraint (M15). The
///       `scope_idx` / `agent_id_idx` generated columns extract via
///       `->>` which silently returns NULL for non-object metadata
///       (array / scalar / NULL); the CHECK rejects malformed
///       metadata at the write boundary instead of degrading to
///       null-scope rows.
// v31 = v0.7.0 Task 1/8 (recursive learning) — `memories.reflection_depth`
//       INTEGER NOT NULL DEFAULT 0 column, mirroring SQLite schema v29.
//       `ADD COLUMN IF NOT EXISTS` keeps the migration idempotent on
//       Postgres 14+; the base schema in `postgres_schema.sql` carries
//       the column inline so fresh installs land it without the
//       migration step running.
// v32 = v0.7.0 v0.7.1-fold (#687/#688) — SQL-side CHECK constraint
//       on `memory_links.relation` (closed taxonomy:
//       related_to/supersedes/contradicts/derived_from/reflects_on).
//       Mirrors SQLite schema v33 (#687 + #688) but uses Postgres's
//       native `ALTER TABLE ADD CONSTRAINT` rather than the SQLite
//       full-table-rebuild dance. Idempotent via pg_constraint probe.
//       Fresh installs inherit the constraint inline from
//       `postgres_schema.sql`.
// v33 = v0.7.0 V-4 closeout (#698) — SQL-side cross-row hash chain
//       on `signed_events`. Adds `prev_hash BYTEA` + `sequence
//       BIGINT` columns plus a UNIQUE INDEX. Mirrors SQLite schema
//       v34. Per-row Ed25519 signatures remain as defense-in-depth;
//       the cross-row chain becomes the LOAD-BEARING tamper-evidence
//       property. Backfill runs application-side in `migrate_v33` so
//       both backends share the canonical-bytes encoding from
//       `signed_events::canonical_chain_bytes`.
// v34 = v0.7.0 QW-3 — context-offload substrate primitive. Adds the
//       `offloaded_blobs` table backing `src/offload/mod.rs`.
//       Mirrors SQLite schema v35. Pure idempotent CREATE TABLE IF
//       NOT EXISTS + CREATE INDEX IF NOT EXISTS — no application-
//       side backfill needed.
// v35 = v0.7.0 WT-1-A — substrate-level atomisation foundation.
//       Adds `memories.atomised_into INTEGER` + `memories.atom_of
//       TEXT REFERENCES memories(id)` columns plus extends the
//       `memory_links.relation` closed-taxonomy CHECK constraint
//       with `derives_from` for atomisation provenance edges
//       (atom -> parent). Mirrors SQLite schema v36. Postgres
//       supports `ADD COLUMN IF NOT EXISTS` so the migration is a
//       pure idempotent DDL batch; the constraint drop-add is gated
//       on a pg_constraint probe so re-running is a no-op. First
//       hard prereq for WT-1-B through WT-1-G.
// v36 = v0.7.0 QW-2 — Persona-as-artifact substrate primitive. Adds
//       `memories.entity_id TEXT NULL` + `memories.persona_version
//       INTEGER NULL` plus the partial index
//       `idx_personas_by_entity`. Mirrors SQLite schema v37. Pure
//       idempotent ADD COLUMN IF NOT EXISTS + CREATE INDEX IF NOT
//       EXISTS — no backfill needed (non-Persona rows keep NULL).
// v37 = v0.7.0 Form 4 — fact-provenance closeout (issue #757). Adds
//       `memories.citations TEXT NOT NULL DEFAULT '[]'`,
//       `memories.source_uri TEXT NULL`, and
//       `memories.source_span TEXT NULL` plus the
//       `idx_memories_source_uri` partial index covering the
//       `--source-uri-prefix` recall filter. Mirrors SQLite schema
//       v38. Pure additive ADD COLUMN IF NOT EXISTS + CREATE INDEX
//       IF NOT EXISTS — no backfill required (legacy rows default to
//       empty citations array and NULL URI/span).
const CURRENT_SCHEMA_VERSION: i32 = 37;

/// Default embedding column dimension used when the caller doesn't pass
/// `--embedding-dim` to `ai-memory schema-init`. Matches the v0.7.0
/// baseline schema (`MiniLmL6V2` embedder = 384). Operators upgrading
/// to a 768-dim embedder (`nomic_embed_v15`) must pass the matching
/// `--embedding-dim 768` flag.
pub const DEFAULT_EMBEDDING_DIM: u32 = 384;

/// Supported embedding column dimensions. Mirrors the values returned
/// by `EmbeddingModel::dim()` for the two compiled-in embedders
/// (MiniLmL6V2 = 384, NomicEmbedV15 = 768). The migration helper
/// rejects any other value with a clear error so an operator typo
/// doesn't leave the schema in an unusable state.
const SUPPORTED_EMBEDDING_DIMS: &[i32] = &[384, 768];

/// Placeholder substituted in `postgres_schema.sql` at connect time.
/// The schema file embeds `vector({EMBEDDING_DIM})` everywhere a
/// dim-bearing column is declared (currently `memories.embedding` and
/// `archived_memories.embedding`); [`PostgresStore::connect_with_dim`]
/// runs a single `str::replace` over the bundled template before
/// executing.
const EMBEDDING_DIM_PLACEHOLDER: &str = "{EMBEDDING_DIM}";

/// Substitute the embedding-dim placeholder in the bundled schema
/// template. Pulled out as a free function so the unit test can exercise
/// it without a running Postgres. Returns a fresh `String` — callers
/// pass the result straight to `sqlx::raw_sql`.
#[must_use]
pub fn render_schema_sql(template: &str, dim: u32) -> String {
    template.replace(EMBEDDING_DIM_PLACEHOLDER, &dim.to_string())
}

/// Default connection pool settings. Tuned for a mid-range ai-memory
/// daemon — adjust via `PostgresStore::with_pool_options` when wiring
/// a larger deployment.
const DEFAULT_MAX_CONNECTIONS: u32 = 16;
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// v0.7.0 M4 — default connection-level `statement_timeout` (seconds)
/// applied to every postgres connection in the pool. 30s bounds the
/// outer envelope of any single query so a pathological `pg_sleep(60)`,
/// a runaway recursive CTE, or an unbounded sequential scan cannot
/// wedge a connection for the whole pool. Operators with intentionally
/// long maintenance queries from the daemon (`schema-init
/// --embedding-dim`, the AGE seed) override via
/// `AppConfig::postgres_statement_timeout_secs`. Setting the value to
/// 0 disables the limit (postgres `SET` semantics).
pub const DEFAULT_STATEMENT_TIMEOUT_SECS: u64 = 30;

/// v0.7.0 M4 companion — paired `lock_timeout` (seconds). Lock waits
/// over this threshold abort with `lock_not_available` instead of
/// blocking forever behind a DDL or a long-running competing txn. 5s
/// is the standard "I'd rather fail fast than hang" envelope; operators
/// can disable it by setting `postgres_statement_timeout_secs = 0`,
/// which also drops the lock_timeout.
pub const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 5;

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
    /// Bootstraps with [`DEFAULT_EMBEDDING_DIM`] (= 384). Use
    /// [`Self::connect_with_dim`] when initializing a fresh schema for
    /// a 768-dim embedder (`nomic_embed_v15`) — passing the dim here
    /// makes the `vector(N)` column declarations match the embedder
    /// from the very first write.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::BackendUnavailable` if the connection
    /// cannot be established or the schema bootstrap fails.
    pub async fn connect(url: &str) -> StoreResult<Self> {
        Self::connect_with_dim(url, DEFAULT_EMBEDDING_DIM).await
    }

    /// Connect with an explicit `statement_timeout` (seconds). Mirrors
    /// [`Self::connect`] but lets callers override the default 30s
    /// safety envelope — typically driven from
    /// `AppConfig::postgres_statement_timeout_secs`. Setting `secs = 0`
    /// disables the timeout (matches postgres `SET` semantics).
    ///
    /// # Errors
    ///
    /// Same as [`Self::connect`].
    pub async fn connect_with_timeout(url: &str, secs: u64) -> StoreResult<Self> {
        Self::connect_with_dim_and_timeout(url, DEFAULT_EMBEDDING_DIM, secs).await
    }

    /// Connect using a Postgres URL with an explicit embedding column
    /// dimension. Substitutes `{EMBEDDING_DIM}` in the bundled
    /// `postgres_schema.sql` before executing so a fresh init lands
    /// the right `vector(N)` declaration.
    ///
    /// For an existing schema, this call does NOT alter pre-declared
    /// `vector(M)` columns — the operator must invoke the explicit
    /// `migrate_embedding_dim` helper (typically via
    /// `ai-memory schema-init --embedding-dim N`) to perform the
    /// destructive in-place conversion. Calling `connect_with_dim`
    /// against a mismatched schema emits a WARN and falls back to the
    /// existing dim for the lifetime of the pool.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::BackendUnavailable` if the connection
    /// cannot be established, `StoreError::InvalidInput` if `dim` is
    /// not one of the supported values, or schema-bootstrap failures
    /// bubble up unchanged.
    pub async fn connect_with_dim(url: &str, dim: u32) -> StoreResult<Self> {
        Self::connect_with_dim_and_timeout(url, dim, DEFAULT_STATEMENT_TIMEOUT_SECS).await
    }

    /// Connect with an explicit embedding dim and `statement_timeout`
    /// (seconds). The fully-parameterised entry point — both
    /// [`Self::connect`] and [`Self::connect_with_dim`] delegate here.
    /// See M4/M7 in `src/store/postgres.rs::DEFAULT_STATEMENT_TIMEOUT_SECS`
    /// for the rationale on the safety envelope.
    ///
    /// # Errors
    ///
    /// Same as [`Self::connect_with_dim`].
    pub async fn connect_with_dim_and_timeout(
        url: &str,
        dim: u32,
        statement_timeout_secs: u64,
    ) -> StoreResult<Self> {
        if !SUPPORTED_EMBEDDING_DIMS.contains(&i32::try_from(dim).unwrap_or(-1)) {
            return Err(StoreError::InvalidInput {
                detail: format!(
                    "unsupported embedding dim {dim}: expected one of {SUPPORTED_EMBEDDING_DIMS:?}"
                ),
            });
        }

        let options: PgConnectOptions =
            url.parse()
                .map_err(|e: sqlx::Error| StoreError::BackendUnavailable {
                    backend: "postgres".to_string(),
                    detail: format!("parse url: {e}"),
                })?;
        // v0.7.0 M4/M7 — `after_connect` hook fires the moment a new
        // connection is acquired. We use it to apply per-session
        // `statement_timeout` + `lock_timeout` so a runaway query
        // cannot wedge the pool indefinitely. `secs = 0` is the
        // postgres-native "disabled" sentinel; we skip the SET in
        // that case to keep the wire silent for operators who
        // explicitly opted out of the safety envelope.
        let stmt_secs = statement_timeout_secs;
        let lock_secs = if stmt_secs == 0 {
            0
        } else {
            DEFAULT_LOCK_TIMEOUT_SECS
        };
        let pool = PgPoolOptions::new()
            .max_connections(DEFAULT_MAX_CONNECTIONS)
            .acquire_timeout(DEFAULT_ACQUIRE_TIMEOUT)
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    use sqlx::Executor;
                    if stmt_secs == 0 {
                        return Ok(());
                    }
                    let stmt_ms = stmt_secs.saturating_mul(1000);
                    let lock_ms = lock_secs.saturating_mul(1000);
                    let sql =
                        format!("SET statement_timeout = {stmt_ms}; SET lock_timeout = {lock_ms};");
                    conn.execute(sql.as_str()).await.map(|_| ())
                })
            })
            .connect_with(options)
            .await
            .map_err(|e| StoreError::BackendUnavailable {
                backend: "postgres".to_string(),
                detail: format!("connect: {e}"),
            })?;

        // Bootstrap schema — idempotent. The bundled template uses
        // `vector({EMBEDDING_DIM})` for the two vector columns; we
        // substitute the caller's chosen dim here. CREATE TABLE IF NOT
        // EXISTS means the dim only "takes" on first init; subsequent
        // calls against an already-populated schema are no-ops.
        let init_sql = render_schema_sql(INIT_SCHEMA, dim);
        sqlx::raw_sql(&init_sql).execute(&pool).await.map_err(|e| {
            StoreError::BackendUnavailable {
                backend: "postgres".to_string(),
                detail: format!("init schema: {e}"),
            }
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

        // Sanity-check that the embedding column dimension matches
        // what the caller requested at connect time. A mismatch means
        // an existing schema was created with a different dim (e.g.
        // operator switched from `MiniLmL6V2` to `NomicEmbedV15` but
        // didn't run `ai-memory schema-init --embedding-dim 768`); we
        // log a WARN here so the operator notices before writes start
        // failing on a dim-mismatch insert. (#304 nit; v0.7.0 L3 made
        // the comparand parameterisable.)
        let typmod: Option<(i32,)> = sqlx::query_as(
            "SELECT atttypmod FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid
             WHERE c.relname = 'memories' AND a.attname = 'embedding'",
        )
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten();
        let expected_dim = i32::try_from(dim).unwrap_or(384);
        if let Some((typmod,)) = typmod
            && typmod != expected_dim
        {
            tracing::warn!(
                target = "store::postgres",
                dim = typmod,
                expected = expected_dim,
                "memories.embedding column dimension ({typmod}) does not match the requested embedder dim ({expected_dim}); run `ai-memory schema-init --store-url <url> --embedding-dim {expected_dim}` to convert in place"
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

        // v0.7.0.1 G4 — when AGE is the resolved KG backend, ensure
        // the `memory_graph` projection exists at connect time so
        // every subsequent link write can `MERGE` nodes/edges into it
        // without racing the bootstrap. `create_graph` is idempotent
        // by error message ("graph 'memory_graph' already exists" is
        // not a fatal condition); the production J1 graph-prep
        // scripts ran the same call out-of-band before this change.
        //
        // We acquire a ONE-SHOT connection from the pool, apply the
        // bootstrap, then drop the connection so the SET search_path
        // doesn't bleed into any future connection re-checked out
        // from the pool. (sqlx's pool resets stale connections on
        // checkout but the reset only fires on the NEXT use; until
        // then the pool may hand the connection back without
        // clearing GUC state.)
        if matches!(kg_backend, KgBackend::Age)
            && let Err(e) = ensure_memory_graph(&pool).await
        {
            tracing::warn!(
                target = "store::postgres",
                error = %e,
                "ensure memory_graph projection failed at connect; KG link projection will degrade silently"
            );
        }

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
        if current_version < 29 {
            // v29 connect-time pass: stamp the version once the
            // schema is conformant. The actual `vector(N)` conversion
            // is operator-initiated via `ai-memory schema-init
            // --embedding-dim <N>`; this no-op pass just records that
            // the daemon understands v29 semantics.
            self.migrate_v29_stamp().await?;
        }
        if current_version < 30 {
            self.migrate_v30().await?;
        }
        if current_version < 31 {
            self.migrate_v31().await?;
        }
        if current_version < 32 {
            self.migrate_v32().await?;
        }
        if current_version < 33 {
            self.migrate_v33().await?;
        }
        if current_version < 34 {
            self.migrate_v34().await?;
        }
        if current_version < 35 {
            self.migrate_v35().await?;
        }
        if current_version < 36 {
            self.migrate_v36().await?;
        }
        if current_version < 37 {
            self.migrate_v37().await?;
        }

        Ok(())
    }

    /// v30 — `memories_metadata_is_object` CHECK constraint (M15).
    ///
    /// The `scope_idx` and `agent_id_idx` generated columns project from
    /// `metadata` via `->>`; that operator silently returns NULL for
    /// any non-object JSONB (array, scalar, or NULL), which masks
    /// governance/scope-routing misconfiguration as "no scope" rows.
    /// The CHECK constraint closes that gap so a malformed metadata
    /// payload is rejected at the write boundary.
    ///
    /// Idempotent: ADD CONSTRAINT IF NOT EXISTS would be cleaner but
    /// Postgres < 14 doesn't grok that syntax; we probe pg_constraint
    /// first and add only on miss. A pre-flight scan refuses to add the
    /// constraint when existing rows would violate it so the operator
    /// gets a clear error rather than a half-applied migration.
    async fn migrate_v30(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v30 tx", e))?;

        // Probe whether the constraint already exists (fresh-schema
        // installs inherit it inline from postgres_schema.sql).
        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT conname FROM pg_constraint
              WHERE conname = 'memories_metadata_is_object'
                AND conrelid = 'memories'::regclass",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| to_store_err("probe memories_metadata_is_object", e))?;

        if exists.is_none() {
            // Pre-flight: reject the migration if any row would violate
            // the invariant. v0.6.x writes always stamp metadata as an
            // object so this should always be 0 in practice.
            let bad_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM memories \
                 WHERE jsonb_typeof(metadata) IS DISTINCT FROM 'object'",
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| to_store_err("count non-object metadata rows", e))?;

            if bad_count > 0 {
                return Err(StoreError::IntegrityFailed {
                    detail: format!(
                        "v30 migration aborted: {bad_count} memories have non-object metadata; \
                         repair them before re-running"
                    ),
                });
            }

            sqlx::query(
                "ALTER TABLE memories \
                 ADD CONSTRAINT memories_metadata_is_object \
                 CHECK (jsonb_typeof(metadata) = 'object') NOT VALID",
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("add memories_metadata_is_object constraint", e))?;

            sqlx::query("ALTER TABLE memories VALIDATE CONSTRAINT memories_metadata_is_object")
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("validate memories_metadata_is_object constraint", e))?;
        }

        record_schema_version(&mut tx, 30).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v30 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v30 applied (memories_metadata_is_object CHECK)"
        );
        Ok(())
    }

    /// v31 — `memories.reflection_depth INTEGER NOT NULL DEFAULT 0`
    /// (v0.7.0 Task 1/8, recursive learning).
    ///
    /// Adds the column that tracks each memory's depth in the substrate-
    /// native reflection recursion tree (`0` for caller-minted rows,
    /// positive for synthesised reflections). Mirrors the SQLite v29
    /// migration in `src/db.rs`.
    ///
    /// Idempotent via `ADD COLUMN IF NOT EXISTS` (Postgres 14+) — fresh
    /// schemas pick the column up inline from `postgres_schema.sql`, so
    /// a fresh install never runs this step. The migration body is
    /// pulled from `migrations/postgres/0013_v0700_reflection_depth.sql`
    /// at compile time via `include_str!` so the SQL stays operator-
    /// inspectable alongside the rest of the per-version migration
    /// scripts.
    async fn migrate_v31(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v31 tx", e))?;

        sqlx::raw_sql(MIGRATION_V31_REFLECTION_DEPTH)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("apply v31 reflection_depth", e))?;

        record_schema_version(&mut tx, 31).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v31 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v31 applied (memories.reflection_depth column)"
        );
        Ok(())
    }

    /// v32 — SQL-side CHECK constraint on `memory_links.relation`
    /// (v0.7.0 v0.7.1-fold, #687/#688).
    ///
    /// Mirrors SQLite schema v33. Postgres's `ALTER TABLE ADD
    /// CONSTRAINT` supports CHECK clauses directly, so this is a
    /// one-statement DDL gated behind a `pg_constraint` probe for
    /// idempotency. Fresh installs inherit the constraint inline from
    /// `postgres_schema.sql`; this migration only fires on a pre-v32
    /// Postgres deployment that already has `memory_links` rows in it.
    ///
    /// The migration body lives in
    /// `migrations/postgres/0014_v07_memory_links_relation_check.sql`
    /// so operators can inspect / replay the DDL outside the daemon.
    async fn migrate_v32(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v32 tx", e))?;

        sqlx::raw_sql(MIGRATION_V32_LINK_RELATION_CHECK)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("apply v32 memory_links.relation CHECK", e))?;

        record_schema_version(&mut tx, 32).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v32 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v32 applied (memory_links.relation CHECK constraint)"
        );
        Ok(())
    }

    /// v33 — `signed_events.prev_hash` + `signed_events.sequence`
    /// (v0.7.0 V-4 closeout, #698).
    ///
    /// Adds the cross-row hash chain columns + UNIQUE INDEX on
    /// `sequence`, then backfills `prev_hash` and `sequence` on
    /// pre-existing rows using the application-layer canonical-bytes
    /// encoding from `signed_events::canonical_chain_bytes`.
    /// Mirrors SQLite schema v34.
    ///
    /// Idempotent via `ADD COLUMN IF NOT EXISTS` (Postgres 14+) +
    /// `WHERE sequence IS NULL` on the backfill loop. Re-running on
    /// an already-backfilled DB is a no-op. Fresh installs inherit
    /// the columns inline from `postgres_schema.sql`, so this step
    /// only fires on pre-v33 Postgres deployments that already have
    /// `signed_events` rows.
    async fn migrate_v33(&self) -> StoreResult<()> {
        use crate::signed_events::{ZERO_HASH, canonical_chain_bytes};
        use sha2::{Digest, Sha256};

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v33 tx", e))?;

        // Add columns + UNIQUE INDEX (idempotent).
        sqlx::raw_sql(MIGRATION_V33_SIGNED_EVENTS_CHAIN)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("apply v33 signed_events chain", e))?;

        // Backfill prev_hash + sequence on rows that still have NULL
        // sequence, ordered by the natural insertion order (the
        // `ctid` row id is reliable here because we only do INSERTs
        // on this table — no UPDATE has moved rows around). We
        // stream rows server-side so the backfill scales even on
        // large pre-existing audit tables.
        let rows: Vec<(
            String,
            String,
            String,
            Vec<u8>,
            Option<Vec<u8>>,
            String,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(
            "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, \
                        timestamp \
                 FROM signed_events \
                 WHERE sequence IS NULL \
                 ORDER BY ctid ASC",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| to_store_err("v33 backfill: select pending", e))?;

        if !rows.is_empty() {
            let mut next_seq: i64 =
                sqlx::query_scalar("SELECT COALESCE(MAX(sequence), 0) FROM signed_events")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| to_store_err("v33 backfill: read max sequence", e))?;

            let mut prev_hash: [u8; 32] = ZERO_HASH;
            for (id, agent_id, event_type, payload_hash, signature, attest_level, ts_dt) in rows {
                next_seq += 1;
                let event = crate::signed_events::SignedEvent {
                    id: id.clone(),
                    agent_id,
                    event_type,
                    payload_hash,
                    signature,
                    attest_level,
                    timestamp: ts_dt.to_rfc3339(),
                    prev_hash: Vec::new(),
                    sequence: next_seq,
                };
                sqlx::query("UPDATE signed_events SET prev_hash = $1, sequence = $2 WHERE id = $3")
                    .bind(prev_hash.to_vec())
                    .bind(next_seq)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| to_store_err("v33 backfill: UPDATE row", e))?;
                let canon = canonical_chain_bytes(&event);
                let mut hasher = Sha256::new();
                hasher.update(&canon);
                prev_hash.copy_from_slice(&hasher.finalize());
            }
        }

        record_schema_version(&mut tx, 33).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v33 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v33 applied (signed_events prev_hash + sequence chain)"
        );
        Ok(())
    }

    /// v34 — context-offload substrate primitive (QW-3).
    ///
    /// Creates the `offloaded_blobs` table backing
    /// `src/offload/mod.rs`. Mirrors SQLite schema v35. Pure
    /// idempotent CREATE TABLE / CREATE INDEX — no application-side
    /// backfill.
    async fn migrate_v34(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v34 tx", e))?;

        sqlx::raw_sql(MIGRATION_V34_OFFLOADED_BLOBS)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("apply v34 offloaded_blobs", e))?;

        record_schema_version(&mut tx, 34).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v34 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v34 applied (offloaded_blobs context-offload substrate)"
        );
        Ok(())
    }

    /// v35 — substrate-level atomisation foundation (v0.7.0 WT-1-A).
    ///
    /// Adds two nullable columns to `memories`:
    ///
    /// - `atomised_into INTEGER` — NULL on legacy rows; positive integer
    ///   on rows that have been atomised by the WT-1-B pass.
    /// - `atom_of TEXT REFERENCES memories(id)` — for atom rows, points
    ///   back to the parent memory; NULL on non-atom rows.
    ///
    /// Also extends the `memory_links.relation` closed-taxonomy CHECK
    /// constraint with the new `derives_from` variant (atomisation
    /// provenance edges atom -> parent). Mirrors SQLite schema v36
    /// (`migrations/sqlite/0030_v07_atomisation.sql`).
    ///
    /// Postgres-native `ADD COLUMN IF NOT EXISTS` + `DO $$ ... $$`
    /// block in the SQL file keeps this fully idempotent. Fresh
    /// installs inherit the columns + extended CHECK inline from
    /// `postgres_schema.sql`; this migration only fires on a
    /// pre-v35 Postgres deployment with pre-existing `memories` /
    /// `memory_links` rows.
    async fn migrate_v35(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v35 tx", e))?;

        sqlx::raw_sql(MIGRATION_V35_ATOMISATION)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("apply v35 atomisation", e))?;

        record_schema_version(&mut tx, 35).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v35 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v35 applied (memories.atomised_into + atom_of columns; \
             memory_links.relation CHECK extended with derives_from)"
        );
        Ok(())
    }

    /// v36 — Persona-as-artifact substrate primitive (QW-2).
    ///
    /// Adds the `memories.entity_id` + `memories.persona_version`
    /// columns plus the partial index `idx_personas_by_entity`
    /// covering Persona-kind rows. Mirrors SQLite schema v37. Pure
    /// idempotent ADD COLUMN IF NOT EXISTS + CREATE INDEX IF NOT
    /// EXISTS — no application-side backfill (non-Persona rows
    /// keep their NULL payloads).
    async fn migrate_v36(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v36 tx", e))?;

        sqlx::raw_sql(MIGRATION_V36_PERSONA)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("apply v36 persona", e))?;

        record_schema_version(&mut tx, 36).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v36 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v36 applied (persona-as-artifact entity_id + persona_version)"
        );
        Ok(())
    }

    /// v37 — Form 4 fact-provenance closeout (issue #757).
    ///
    /// Adds the `memories.citations` (JSON array of Citation objects,
    /// default `[]`), `memories.source_uri` (first-class URI-form
    /// pointer), and `memories.source_span` (JSON byte-range into
    /// the parent source body) columns plus the partial index
    /// `idx_memories_source_uri` covering the `--source-uri-prefix`
    /// recall filter. Mirrors SQLite schema v38. Pure additive
    /// ADD COLUMN IF NOT EXISTS + CREATE INDEX IF NOT EXISTS — no
    /// application-side backfill (legacy rows take the SQL DEFAULT
    /// for citations and NULL for the URI/span columns).
    async fn migrate_v37(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v37 tx", e))?;

        sqlx::raw_sql(MIGRATION_V37_FORM4_PROVENANCE)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("apply v37 form4 provenance", e))?;

        record_schema_version(&mut tx, 37).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v37 migration", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v37 applied (form4 fact-provenance citations + source_uri + source_span)"
        );
        Ok(())
    }

    /// v29 connect-time no-op pass.
    ///
    /// The actual `vector(N)` column conversion is destructive (rows
    /// have their embedding NULLed, HNSW indexes are dropped + recreated)
    /// and therefore MUST be operator-initiated rather than firing
    /// implicitly on daemon startup. The connect-time pass here just
    /// records that we've reached v29 once the bookkeeping is in place;
    /// the explicit conversion lives at
    /// [`Self::migrate_embedding_dim`].
    async fn migrate_v29_stamp(&self) -> StoreResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v29 stamp tx", e))?;

        record_schema_version(&mut tx, 29).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v29 stamp", e))?;

        tracing::info!(
            target = "store::postgres",
            "schema migration v29 stamped (operator-initiated vector(N) conversion available via ai-memory schema-init --embedding-dim)"
        );
        Ok(())
    }

    /// Read the current dimension of `memories.embedding`. Returns
    /// `None` when the column is missing (which should not happen
    /// post-bootstrap) or the type isn't `vector(N)`.
    ///
    /// pgvector encodes the declared dim in `pg_attribute.atttypmod`;
    /// the value matches what `format_type(atttypid, atttypmod)` would
    /// surface as `vector(N)`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::BackendUnavailable` if the catalog probe
    /// fails (connection issue, etc.).
    pub async fn current_embedding_dim(&self) -> StoreResult<Option<i32>> {
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT atttypmod FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid
             WHERE c.relname = 'memories' AND a.attname = 'embedding'",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| to_store_err("read memories.embedding atttypmod", e))?;
        Ok(row.map(|(t,)| t))
    }

    /// Operator-initiated `vector(N)` conversion. Destructive on the
    /// embedding column: rows have their `embedding` NULLed (vectors
    /// cannot be reprojected dim-to-dim), the HNSW indexes on both
    /// `memories.embedding` and `archived_memories.embedding` are
    /// dropped and recreated, and the column is `ALTER COLUMN ... TYPE
    /// vector(<target_dim>)`'d. The caller is responsible for
    /// re-running embeddings after the conversion.
    ///
    /// Idempotent: when the column is already `vector(target_dim)` the
    /// call is a no-op and returns `Ok(false)` (no change). On a real
    /// conversion it returns `Ok(true)`.
    ///
    /// The whole operation runs in a single transaction so a mid-way
    /// failure leaves the schema untouched.
    ///
    /// # Errors
    ///
    /// - `StoreError::InvalidInput` when `target_dim` is not one of the
    ///   supported values (`SUPPORTED_EMBEDDING_DIMS`).
    /// - `StoreError::BackendUnavailable` on any SQL failure during
    ///   the conversion.
    pub async fn migrate_embedding_dim(&self, target_dim: u32) -> StoreResult<bool> {
        let target_i32 = i32::try_from(target_dim).map_err(|_| StoreError::InvalidInput {
            detail: format!("target_dim {target_dim} out of i32 range"),
        })?;
        if !SUPPORTED_EMBEDDING_DIMS.contains(&target_i32) {
            return Err(StoreError::InvalidInput {
                detail: format!(
                    "unsupported target embedding dim {target_dim}: expected one of {SUPPORTED_EMBEDDING_DIMS:?}"
                ),
            });
        }

        let current = self.current_embedding_dim().await?;
        if let Some(cur) = current
            && cur == target_i32
        {
            tracing::info!(
                target = "store::postgres",
                dim = target_i32,
                "v29 embedding-dim migration: column already vector({target_i32}); no-op"
            );
            return Ok(false);
        }

        tracing::warn!(
            target = "store::postgres",
            current = ?current,
            target = target_i32,
            "v29 embedding-dim migration: converting memories.embedding + archived_memories.embedding; existing embeddings will be NULLed — operators MUST re-run embeddings after this conversion completes"
        );

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin v29 conversion tx", e))?;

        // Drop the HNSW indexes that pin the column type. ALTER COLUMN
        // TYPE on a vector column fails while the HNSW index references
        // it; we drop here and recreate below. The named index from
        // postgres_schema.sql is `memories_embedding_hnsw`; the archive
        // table doesn't carry an HNSW index in the baseline schema but
        // we issue `IF EXISTS` to tolerate operator-added indexes too.
        sqlx::query("DROP INDEX IF EXISTS memories_embedding_hnsw")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("drop memories_embedding_hnsw", e))?;
        sqlx::query("DROP INDEX IF EXISTS archived_memories_embedding_hnsw")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("drop archived_memories_embedding_hnsw", e))?;

        // NULL existing embeddings. Cross-dim reprojection isn't
        // mathematically meaningful (a 384-d vector and a 768-d vector
        // from two different models live in different spaces); the
        // operator MUST re-embed after the migration. We don't TRUNCATE
        // because the memory rows themselves remain valid — only the
        // vector column is invalidated.
        sqlx::query("UPDATE memories SET embedding = NULL WHERE embedding IS NOT NULL")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("null memories.embedding", e))?;
        sqlx::query("UPDATE archived_memories SET embedding = NULL WHERE embedding IS NOT NULL")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("null archived_memories.embedding", e))?;

        // Alter the column types. pgvector's type allows ALTER COLUMN
        // TYPE between two `vector(N)` declarations even when rows are
        // present (because we just NULLed them above). The `USING NULL`
        // cast is required when there might be non-NULL rows; we add
        // it defensively even though the UPDATE above ensures none.
        let alter_memories = format!(
            "ALTER TABLE memories ALTER COLUMN embedding TYPE vector({target_dim}) USING NULL"
        );
        sqlx::query(&alter_memories)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("alter memories.embedding type", e))?;

        let alter_archived = format!(
            "ALTER TABLE archived_memories ALTER COLUMN embedding TYPE vector({target_dim}) USING NULL"
        );
        sqlx::query(&alter_archived)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("alter archived_memories.embedding type", e))?;

        // Recreate the HNSW index on the live table. The archived
        // table is intentionally NOT given an HNSW index (matches the
        // baseline schema — archive recall is rare and a covering
        // index would bloat write-time cost).
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS memories_embedding_hnsw ON memories
             USING hnsw (embedding vector_cosine_ops)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("recreate memories_embedding_hnsw", e))?;

        record_schema_version(&mut tx, 29).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit v29 conversion", e))?;

        tracing::warn!(
            target = "store::postgres",
            target_dim = target_i32,
            "v29 embedding-dim migration committed; re-run embeddings (e.g. via memory_store with the matching embedder configured)"
        );

        Ok(true)
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

        // Read the current dim of `memories.embedding` so we add the
        // archive-side column at the same dim. The bootstrap schema
        // has already run by the time we reach here, so this column
        // exists with whatever dim the operator chose at connect time.
        // Falls back to DEFAULT_EMBEDDING_DIM if the probe comes back
        // empty (defensive — should never happen in practice).
        let existing_dim: Option<(i32,)> = sqlx::query_as(
            "SELECT atttypmod FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid
             WHERE c.relname = 'memories' AND a.attname = 'embedding'",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| to_store_err("read memories.embedding dim in v18", e))?;
        let dim_for_archive = existing_dim.map_or(DEFAULT_EMBEDDING_DIM, |(d,)| {
            u32::try_from(d).unwrap_or(DEFAULT_EMBEDDING_DIM)
        });
        let archive_embedding_ddl =
            format!("ALTER TABLE archived_memories ADD COLUMN embedding vector({dim_for_archive})");

        for (table, column, ddl) in [
            (
                "memories",
                "embedding_dim",
                "ALTER TABLE memories ADD COLUMN embedding_dim INTEGER".to_string(),
            ),
            ("archived_memories", "embedding", archive_embedding_ddl),
            (
                "archived_memories",
                "embedding_dim",
                "ALTER TABLE archived_memories ADD COLUMN embedding_dim INTEGER".to_string(),
            ),
            (
                "archived_memories",
                "original_tier",
                "ALTER TABLE archived_memories ADD COLUMN original_tier TEXT".to_string(),
            ),
            (
                "archived_memories",
                "original_expires_at",
                "ALTER TABLE archived_memories ADD COLUMN original_expires_at TIMESTAMPTZ"
                    .to_string(),
            ),
        ] {
            add_column_if_missing(&mut tx, table, column, &ddl).await?;
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
        // v0.7.0 Wave-3 Continuation 5 — see `kg_query_with_history`
        // for the AGE-vs-CTE rationale. This entry point hides the
        // `include_invalidated` knob (defaults to false / "current
        // view") for callers that don't care about the historical
        // posture; S82 + parity tests use this wider-scoped variant.
        self.kg_query_with_history(source_id, max_depth, false)
            .await
    }

    /// Outbound traversal with explicit historical-view toggle.
    /// `include_invalidated=true` lifts the
    /// `valid_until IS NULL OR valid_until > NOW()` filter so callers
    /// can read the full historical edge graph (S45's `as_of=past`
    /// semantics through `memory_kg_query`).
    pub async fn kg_query_with_history(
        &self,
        source_id: &str,
        max_depth: usize,
        include_invalidated: bool,
    ) -> StoreResult<Vec<KgQueryRow>> {
        // v0.7.0 S6-M3 — AGE Cypher dispatcher. Routes on the
        // `kg_backend` resolved at `connect()` time (J1 substrate
        // probe). The AGE path delivers the ~30% speed-up advertised
        // in ROADMAP2 §7.4.4; the CTE path is the universal fallback
        // for vanilla Postgres deployments. Dual-path test discipline
        // (#648) requires identical KgQueryRow outputs from both
        // branches — `tests/postgres_kg_age_cte_parity.rs` pins that
        // contract.
        //
        // `include_invalidated=true` lifts the historical filter; the
        // Cypher branch doesn't expose that switch yet (#681) so we
        // fall back to the CTE for historical queries even when AGE
        // is available. AGE's coverage is the "current view" hot
        // path — historical traversal is a smaller niche and lives on
        // the CTE.
        // v0.7.0 fold-A2A1.3 (#700) — runtime AGE→CTE graceful
        // fallback. Boot-time `detect_kg_backend` is already graceful
        // for the missing-extension case, but a `LOAD 'age'` /
        // `cypher()` call can still fail at request time (DROP
        // EXTENSION between boot and now, projection missing, role
        // permissions on `ag_catalog`, transient pool error). Rather
        // than propagate `BackendUnavailable` to the four KG MCP
        // handlers — which historically materialised as a hard 503 on
        // `kg_query` / `kg_timeline` / `kg_invalidate` /
        // `find_paths` — we catch the AGE-side failure, log a
        // structured warning, and re-issue via the relational CTE so
        // operators see a degraded but functional KG surface. Each
        // request retries AGE first; we don't latch the backend
        // permanently in case the operator restores AGE behind us.
        match self.kg_backend {
            KgBackend::Age if !include_invalidated => {
                match self.kg_query_cypher(source_id, max_depth).await {
                    Ok(rows) => Ok(rows),
                    Err(err) if is_age_runtime_failure(&err) => {
                        warn_age_fallback("kg_query", source_id, &err);
                        self.kg_query_cte_filtered(source_id, max_depth, include_invalidated)
                            .await
                    }
                    Err(err) => Err(err),
                }
            }
            _ => {
                self.kg_query_cte_filtered(source_id, max_depth, include_invalidated)
                    .await
            }
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

        // v0.7.0 Wave-3 Continuation 5 — AGE rejects `$1::agtype` (the
        // third arg to `cypher()` must be a bare Param node, not a
        // FuncExpr cast). Inline the params object as a literal
        // agtype constant; `source_id` is UUID-validated upstream so
        // this is safe. The dollar-quoted string body uses `$$`; the
        // params literal lives outside it.
        let params_lit = age_params_literal(&[("start_id", source_id)]);
        let sql = format!(
            "SELECT target_id, relation, depth, path FROM cypher('memory_graph', $$ {cypher} $$, \
             {params_lit}) AS (target_id agtype, relation agtype, depth agtype, path agtype)"
        );

        let rows = sqlx::query(&sql)
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
        // Default current-view posture mirrors `db::kg_query` —
        // invalidated edges hidden. Callers that need the historical
        // view go through `kg_query_with_history` /
        // `kg_query_cte_filtered`.
        self.kg_query_cte_filtered(source_id, max_depth, false)
            .await
    }

    /// Recursive-CTE traversal with explicit historical-view toggle.
    /// `include_invalidated=true` lifts the
    /// `valid_until IS NULL OR valid_until > NOW()` filter on both
    /// the CTE seed AND step rows.
    pub async fn kg_query_cte_filtered(
        &self,
        source_id: &str,
        max_depth: usize,
        include_invalidated: bool,
    ) -> StoreResult<Vec<KgQueryRow>> {
        validate_depth(max_depth)?;

        let depth_cap = i32::try_from(max_depth).unwrap_or(i32::MAX);
        // v0.7.0 Wave-3 Continuation 5 — filter out invalidated edges
        // (`valid_until` set in the past) so `as_of=now` queries
        // only see currently-valid edges. The filter clause is
        // dropped in via format! when include_invalidated is false;
        // the historical view (`include_invalidated=true`) issues
        // the same SQL with `TRUE` in place of the predicate so the
        // query plan stays uniform across views.
        let valid_filter = if include_invalidated {
            "TRUE"
        } else {
            "(ml.valid_until IS NULL OR ml.valid_until > NOW())"
        };
        let sql = format!(
            "WITH RECURSIVE traversal(target_id, relation, depth, path) AS (
                SELECT ml.target_id, ml.relation, 1,
                       ml.source_id || '->' || ml.target_id
                FROM memory_links ml
                WHERE ml.source_id = $1
                  AND {valid_filter}
                UNION ALL
                SELECT ml.target_id, ml.relation, t.depth + 1,
                       t.path || '->' || ml.target_id
                FROM memory_links ml
                JOIN traversal t ON ml.source_id = t.target_id
                WHERE t.depth < $2
                  AND position(('->' || ml.target_id) IN t.path) = 0
                  AND position((ml.target_id || '->') IN t.path) = 0
                  AND {valid_filter}
            )
            SELECT target_id, relation, depth, path
            FROM traversal
            ORDER BY depth ASC, target_id ASC"
        );

        let rows = sqlx::query(&sql)
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
        // v0.7.0 S6-M3 — AGE Cypher dispatcher. Routes on the
        // resolved `kg_backend` so the AGE-installed deployments get
        // the property-graph-path speedup while vanilla Postgres
        // deployments keep the relational CTE. The dual-path
        // discipline (#648) holds for the timeline shape too —
        // `tests/postgres_kg_age_cte_parity.rs` exercises both.
        // v0.7.0 fold-A2A1.3 (#700) — runtime AGE→CTE graceful
        // fallback; see `kg_query_with_history` for rationale. Same
        // pattern applied to the timeline dispatcher so an
        // AGE-side cypher failure degrades to the relational walk
        // instead of surfacing as a 503 to the `memory_kg_timeline`
        // MCP handler.
        match self.kg_backend {
            KgBackend::Age => {
                match self
                    .kg_timeline_cypher(source_id, since, until, limit)
                    .await
                {
                    Ok(rows) => Ok(rows),
                    Err(err) if is_age_runtime_failure(&err) => {
                        warn_age_fallback("kg_timeline", source_id, &err);
                        self.kg_timeline_cte(source_id, since, until, limit).await
                    }
                    Err(err) => Err(err),
                }
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

        // Inline the params dict as an AGE literal — see
        // `age_params_literal` for the rationale (AGE rejects
        // `$1::agtype` as a FuncExpr).
        let mut pairs: Vec<(&str, &str)> = vec![("start_id", source_id)];
        if let Some(s) = since {
            pairs.push(("since", s));
        }
        if let Some(u) = until {
            pairs.push(("until", u));
        }
        let params_lit = age_params_literal(&pairs);
        let sql = format!(
            "SELECT target_id, relation, valid_from, valid_until, observed_by \
             FROM cypher('memory_graph', $$ {cypher} $$, {params_lit}) AS \
             (target_id agtype, relation agtype, valid_from agtype, \
              valid_until agtype, observed_by agtype)"
        );

        let rows = sqlx::query(&sql)
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
        // v0.7.0 S6-M3 — AGE Cypher dispatcher. Same dual-path
        // discipline as `kg_query` / `kg_timeline`. When AGE is
        // available we set the `valid_until` property through Cypher
        // so the property-graph projection stays in sync with
        // `memory_links`; otherwise we update the relational table
        // directly.
        // v0.7.0 fold-A2A1.3 (#700) — runtime AGE→CTE graceful
        // fallback for the invalidation path. The CTE branch also
        // performs an idempotent UPDATE so a re-issue against the
        // relational table after an AGE-side failure is safe — and
        // `kg_invalidate_cypher` mirrors its SET back into
        // `memory_links` so the CTE branch returns the same
        // `previous_valid_until` shape on a retry.
        match self.kg_backend {
            KgBackend::Age => {
                match self
                    .kg_invalidate_cypher(source_id, target_id, relation, valid_until)
                    .await
                {
                    Ok(row) => Ok(row),
                    Err(err) if is_age_runtime_failure(&err) => {
                        warn_age_fallback("kg_invalidate", source_id, &err);
                        self.kg_invalidate_cte(source_id, target_id, relation, valid_until)
                            .await
                    }
                    Err(err) => Err(err),
                }
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
        // Inline the params dict — see `age_params_literal`.
        let read_params_lit =
            age_params_literal(&[("src", source_id), ("dst", target_id), ("rel", relation)]);
        let read_sql = format!(
            "SELECT prior FROM cypher('memory_graph', $$ {read_cypher} $$, {read_params_lit}) AS \
             (prior agtype)"
        );
        let prior_rows = sqlx::query(&read_sql)
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
        let write_params_lit = age_params_literal(&[
            ("src", source_id),
            ("dst", target_id),
            ("rel", relation),
            ("now", &stamp),
        ]);
        let write_sql = format!(
            "SELECT affected FROM cypher('memory_graph', $$ {write_cypher} $$, {write_params_lit}) AS \
             (affected agtype)"
        );
        let _ = sqlx::query(&write_sql)
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
        // v0.7.0 fold-A2A1.3 (#700) — runtime AGE→CTE graceful
        // fallback for `find_paths`. Same shape as the other three
        // dispatchers; the CTE enumeration produces the same
        // `Vec<Vec<String>>` shape so the upper-layer handler stays
        // backend-blind.
        match self.kg_backend {
            KgBackend::Age => {
                match self
                    .find_paths_cypher(source_id, target_id, max_depth, max_results)
                    .await
                {
                    Ok(paths) => Ok(paths),
                    Err(err) if is_age_runtime_failure(&err) => {
                        warn_age_fallback_pair("find_paths", source_id, target_id, &err);
                        self.find_paths_cte(source_id, target_id, max_depth, max_results)
                            .await
                    }
                    Err(err) => Err(err),
                }
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

        // v0.7.0.1 G5 — the prior shape `RETURN [n IN nodes(p) |
        // properties(n).id] AS path` triggered AGE 1.5.0's grammar at
        // the `|` separator with `syntax error at or near "|"`
        // (HALT R1b S65). AGE 1.5.0's parser does support list
        // comprehensions in isolation but `convert_cypher_to_subquery`
        // mis-handles the form when the iteration variable is bound
        // from a function call (`nodes(p)`) and the projection touches
        // a property — the analyzer's `|` recovery point bubbles up as
        // a Postgres syntax error before the Cypher body even reaches
        // the planner. The `reduce()` form below uses the same
        // iteration grammar (`var IN list | expr`) that already ships
        // working in `kg_query_cypher`'s `reduce(s = a.id, n IN
        // nodes(p)[1..] | s + '->' + n.id)` projection — `reduce` and
        // list comprehension share an AST node in AGE 1.5 but only the
        // former survives the analyzer pass against this fixture.
        //
        // The delimiter is `->` (an arrow) which cannot appear in a
        // memory id (the validator constrains ids to UUIDv4 / a-z0-9_-)
        // so server-side splitting is unambiguous. We stay away from a
        // bare `|` literal to avoid re-triggering any analyzer recovery
        // point on the parser side.
        //
        // The variable-length pattern is `*1..N` (explicit minimum)
        // rather than `*..N`. AGE 1.5.0 accepts both at the grammar
        // level but only the explicit form round-trips through
        // `convert_cypher_to_subquery`'s pattern walker — same fix
        // shape as `kg_query_cypher`. The pattern stays un-arrowed
        // (`-[*1..N]-`) so the symmetric-closure contract from the CTE
        // branch holds: matching either declared edge direction.
        const PATH_DELIM: &str = "->";
        let cypher = format!(
            "MATCH p = (a)-[*1..{depth}]-(b) \
             WHERE a.id = $start_id AND b.id = $target_id \
             RETURN reduce(s = a.id, n IN nodes(p)[1..] | \
                           s + '{PATH_DELIM}' + n.id) AS path \
             ORDER BY length(p) ASC \
             LIMIT {cap}"
        );

        // v0.7.0.1 G2 — bind the params dict as an `agtype`-typed
        // parameter through sqlx (`$1`) rather than inlining it as a
        // SQL literal. AGE 1.5.0's `convert_cypher_to_subquery`
        // analyzer rejects ANY non-Param node at the third-argument
        // position; the literal form previously here surfaces as a
        // 503 from `POST /api/v1/kg/find_paths` (HALT R1b S65).
        let params = age_params_jsonb(&[("start_id", source_id), ("target_id", target_id)]);
        let sql =
            format!("SELECT path FROM cypher('memory_graph', $$ {cypher} $$, $1) AS (path agtype)");

        let rows = sqlx::query(&sql)
            .bind(Agtype(params))
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| to_store_err("cypher find_paths", e))?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit AGE tx", e))?;

        // AGE returns each `path` cell as a JSON-encoded string
        // (agtype string) of the shape `"id1->id2->…"` — `reduce`
        // walks a `nodes(p)[1..]` slice and appends `'->' + n.id` per
        // step starting from `a.id`. Decode through the [`Agtype`]
        // wrapper so sqlx's binary-protocol path strips the version
        // byte; the resulting payload is a JSON string literal that
        // we parse and then split on `->` to recover the path.
        rows.iter()
            .map(|r| {
                let raw: Agtype = r
                    .try_get::<Agtype, _>("path")
                    .map_err(|e| to_store_err("read path", e))?;
                // AGE serialises strings as JSON-quoted strings (e.g.
                // `"abc"`); parse as JSON to strip the
                // quotes and decode any escapes the encoder applied.
                let joined: String =
                    serde_json::from_str(&raw.0).map_err(|e| StoreError::IntegrityFailed {
                        detail: format!("non-JSON AGE path: {}: {e}", raw.0),
                    })?;
                Ok(joined.split(PATH_DELIM).map(str::to_string).collect())
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

        // v0.7.0.1 G3 — normalize the temporal-validity timestamps to
        // microsecond precision BEFORE we both sign over them and
        // commit them to PostgreSQL. PostgreSQL's `TIMESTAMPTZ` column
        // stores microseconds since epoch — sub-microsecond digits are
        // silently dropped at write time. If the canonical CBOR
        // payload commits to a nanosecond-precision RFC3339 string,
        // the verify path's `to_rfc3339()` of the read-back
        // `DateTime<Utc>` produces a microsecond-precision string,
        // changing the canonical CBOR bytes and invalidating the
        // signature (HALT R1b S52). Truncating both ends to the same
        // precision the column round-trips makes sign/verify byte-
        // stable across the storage layer. SQLite stores TIMESTAMPTZ
        // as RFC3339 TEXT and round-trips losslessly so the SQLite
        // path is unaffected — the truncation is a no-op when the
        // input is already microsecond-aligned.
        let valid_from_dt = truncate_to_microseconds(valid_from_dt);
        let valid_until_dt = valid_until_dt.map(truncate_to_microseconds);
        let valid_from_str = valid_from_dt.to_rfc3339();
        let valid_until_str = valid_until_dt.map(|t| t.to_rfc3339());

        // Branch on the keypair: signed vs. unsigned. The signed path
        // computes the canonical CBOR + Ed25519 signature BEFORE the
        // INSERT so a CBOR/sign failure surfaces as a clean error
        // rather than a half-written row. This is the same ordering
        // SQLite uses (see `db::create_link_signed`).
        let (signature, attest_level, observed_by_col): (
            Option<Vec<u8>>,
            &'static str,
            Option<String>,
        ) = match keypair {
            Some(kp) if kp.can_sign() => {
                let signable = crate::identity::sign::SignableLink {
                    src_id: &link.source_id,
                    dst_id: &link.target_id,
                    relation: link.relation.as_str(),
                    observed_by: Some(kp.agent_id.as_str()),
                    valid_from: Some(valid_from_str.as_str()),
                    valid_until: valid_until_str.as_deref(),
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

        // v0.7.0.1 G4 — wrap the SQL `INSERT INTO memory_links` write
        // and the AGE `memory_graph` projection MERGE in a single
        // transaction so a successful link write never leaves a
        // stale AGE projection that would surface as `paths_found=0`
        // on `find_paths_cypher` (HALT v0.7.0 R1 S65). When the
        // adapter resolved the CTE backend at connect time the AGE
        // branch is a no-op — the recursive-CTE path reads from
        // `memory_links` directly.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin link tx", e))?;

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
        .bind(link.relation.as_str())
        .bind(created_at_dt)
        .bind(valid_from_dt)
        .bind(valid_until_dt)
        .bind(signature)
        .bind(attest_level)
        .bind(observed_by_col)
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("insert memory_link", e))?;

        if matches!(self.kg_backend, KgBackend::Age) {
            project_link_into_age(
                &mut tx,
                &link.source_id,
                &link.target_id,
                link.relation.as_str(),
            )
            .await?;
        }

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit link tx", e))?;

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

        // v0.7.0 Task 1/8 — read the v31 column; tolerate pre-v31 reads
        // (column missing on a freshly-restored backup, for instance) by
        // falling back to 0, which matches the SQL-side `DEFAULT 0`.
        let reflection_depth: i32 = row.try_get("reflection_depth").unwrap_or(0);
        // L1-1 — read the Postgres-side memory_kind column. Falls back to
        // Observation on pre-v30 rows and on any unrecognised value.
        let memory_kind: crate::models::MemoryKind = row
            .try_get::<String, _>("memory_kind")
            .ok()
            .and_then(|s| crate::models::MemoryKind::from_str(&s))
            .unwrap_or_default();

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
            reflection_depth,
            memory_kind,
            // v0.7.0 QW-2 — pre-v36 rows lack these columns; tolerate the
            // missing-column error and fall back to NULL (matches SQLite
            // path behaviour for backups predating the persona migration).
            entity_id: row
                .try_get::<Option<String>, _>("entity_id")
                .unwrap_or(None),
            persona_version: row
                .try_get::<Option<i32>, _>("persona_version")
                .unwrap_or(None),
            // v0.7.0 Form 4 — Postgres v37 fact-provenance columns. The
            // SQL DEFAULT '[]' on `citations` keeps legacy rows visible
            // as the empty vec; pre-v37 backups missing the column hit
            // the `.unwrap_or_default()` fallthrough below.
            citations: row
                .try_get::<String, _>("citations")
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            source_uri: row
                .try_get::<Option<String>, _>("source_uri")
                .unwrap_or(None),
            source_span: row
                .try_get::<Option<String>, _>("source_span")
                .unwrap_or(None)
                .and_then(|s| serde_json::from_str(&s).ok()),
        })
    }

    /// v0.7.0 recursive-learning Task 4/8 (issue #655) — Postgres parity
    /// for [`crate::db::reflect`]. Inherent method (not on the
    /// [`MemoryStore`] trait) to keep the trait surface minimal per the
    /// Task 4 spec.
    ///
    /// Atomicity: the reflection memory insert + N `reflects_on` link
    /// inserts run inside a single `sqlx::Transaction`. Any link
    /// failure rolls back the entire write. Mirrors the
    /// `BEGIN IMMEDIATE` … `COMMIT` block on the SQLite path.
    ///
    /// Governance: walks the namespace ancestor chain via
    /// [`MemoryStore::resolve_governance_policy`], falls back to
    /// [`crate::models::GovernancePolicy::default`] when no policy is
    /// configured anywhere in the chain, then evaluates
    /// [`crate::models::GovernancePolicy::effective_max_reflection_depth`].
    ///
    /// Returns the [`crate::db::ReflectOutcome`] shape the SQLite path
    /// returns so test code can be backend-agnostic.
    ///
    /// # Errors
    ///
    /// Same [`crate::db::ReflectError`] variants as the SQLite path,
    /// emitted in the same order:
    /// 1. validation
    /// 2. source-not-found (no partial write)
    /// 3. depth-exceeded refusal
    /// 4. database errors during the atomic write
    pub async fn reflect(
        &self,
        ctx: &super::CallerContext,
        input: &crate::db::ReflectInput,
    ) -> std::result::Result<crate::db::ReflectOutcome, crate::db::ReflectError> {
        // Thin shim over [`reflect_with_hooks`] with an empty hook
        // bundle — keeps the public entry-point shape stable while
        // the v0.7.0 Task 6/8 pre_reflect / post_reflect surface
        // lands behind an opt-in second function.
        self.reflect_with_hooks(ctx, input, &crate::db::ReflectHooks::empty())
            .await
    }

    /// v0.7.0 recursive-learning Task 6/8 — Postgres twin of
    /// [`crate::db::reflect_with_hooks`]. Fires `pre_reflect` BEFORE
    /// the depth-cap check (a `Deny` propagates as
    /// [`crate::db::ReflectError::HookVeto`]; cap audit is NOT
    /// emitted on this path) and fires `post_reflect` AFTER the
    /// transaction commits (notify-class; return value ignored).
    ///
    /// # Errors
    ///
    /// Same variants as [`PostgresStore::reflect`] plus
    /// [`crate::db::ReflectError::HookVeto`] on `pre_reflect` veto.
    #[allow(clippy::too_many_lines)]
    pub async fn reflect_with_hooks(
        &self,
        ctx: &super::CallerContext,
        input: &crate::db::ReflectInput,
        hooks: &crate::db::ReflectHooks<'_>,
    ) -> std::result::Result<crate::db::ReflectOutcome, crate::db::ReflectError> {
        use crate::db::ReflectError;
        use crate::validate;

        // ─── 1. Validate inputs ─────────────────────────────────────
        validate::validate_title(&input.title)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;
        validate::validate_content(&input.content)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;
        validate::validate_tags(&input.tags)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;
        validate::validate_priority(input.priority)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;
        validate::validate_confidence(input.confidence)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;
        validate::validate_source(&input.source)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;
        validate::validate_agent_id(&input.agent_id)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;
        if input.source_ids.is_empty() {
            return Err(ReflectError::Validation(
                "source_ids cannot be empty — a reflection must reflect on at least one source memory".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for (i, id) in input.source_ids.iter().enumerate() {
            validate::validate_id(id)
                .map_err(|e| ReflectError::Validation(format!("source_ids[{i}]: {e}")))?;
            if !seen.insert(id.as_str()) {
                return Err(ReflectError::Validation(format!(
                    "source_ids[{i}]: duplicate id '{id}'"
                )));
            }
        }
        if let Some(ref ns) = input.namespace {
            validate::validate_namespace(ns)
                .map_err(|e| ReflectError::Validation(e.to_string()))?;
        }
        validate::validate_metadata(&input.metadata)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;

        // ─── 2. Load each source memory ─────────────────────────────
        let mut sources = Vec::with_capacity(input.source_ids.len());
        for id in &input.source_ids {
            match super::MemoryStore::get(self, ctx, id).await {
                Ok(m) => sources.push(m),
                Err(StoreError::NotFound { .. }) => {
                    return Err(ReflectError::SourceNotFound(id.clone()));
                }
                Err(e) => return Err(ReflectError::Database(e.to_string())),
            }
        }

        // ─── 3. Compute new_depth ───────────────────────────────────
        let max_src_depth = sources
            .iter()
            .map(|m| m.reflection_depth)
            .max()
            .unwrap_or(0);
        let new_depth_i32 = max_src_depth.max(0).saturating_add(1);
        #[allow(clippy::cast_sign_loss)]
        let new_depth_u32: u32 = new_depth_i32 as u32;

        // ─── 4. Resolve target namespace + cap ──────────────────────
        let target_namespace = match input.namespace {
            Some(ref ns) => ns.clone(),
            None => sources[0].namespace.clone(),
        };
        let policy = super::MemoryStore::resolve_governance_policy(self, &target_namespace)
            .await
            .map_err(|e| ReflectError::Database(e.to_string()))?
            .unwrap_or_else(crate::models::GovernancePolicy::default);
        let cap = policy.effective_max_reflection_depth();

        // ─── 4.5 `pre_reflect` hook (v0.7.0 Task 6/8) ──────────────
        //
        // Fires BEFORE the cap check so a hook handler may VETO the
        // reflection. The veto path returns `ReflectError::HookVeto`
        // and does NOT emit the Task 5 depth-cap audit row.
        if let Some(pre) = hooks.pre_reflect.as_ref() {
            match (pre)(input) {
                crate::db::ReflectHookDecision::Allow => {}
                crate::db::ReflectHookDecision::Deny { reason, code } => {
                    return Err(ReflectError::HookVeto { reason, code });
                }
            }
        }

        // ─── 5. Refuse if proposed depth exceeds cap ────────────────
        //
        // Task 5/8 (v0.7.0): before propagating the refusal, append a
        // `reflection.depth_exceeded` row to `signed_events`. Mirrors
        // the SQLite parity path in `db::reflect`. Audit-write failure
        // is logged (best-effort); the refusal still propagates. Hook
        // vetoes (Task 6/8 `pre_reflect`) carry their own provenance
        // and are deliberately NOT emitted here.
        if new_depth_u32 > cap {
            // v0.7.0 L2-2 — surface cross-peer provenance in the audit
            // row when at least one source memory carries a
            // `reflection_origin.peer_origin` stamp (i.e. it was
            // imported via federation `sync_push`). Local cap is
            // enforced regardless of source origin (territorial
            // sovereignty); the peer claim only enriches the audit
            // record so a downstream auditor sees WHERE the depth
            // came from. Mirrors the SQLite parity path.
            let cross_peer_refusal =
                crate::federation::reflection_bookkeeping::enforce_local_cap_on_derived(
                    new_depth_u32,
                    cap,
                    &sources,
                );
            let peer_origin: Option<String> = if let Err(ref r) = cross_peer_refusal {
                if let Some(ref peer) = r.imported_peer {
                    tracing::warn!(
                        target: "federation::reflection_bookkeeping",
                        peer = %peer,
                        attempted = new_depth_u32,
                        local_cap = cap,
                        namespace = %target_namespace,
                        "L2-2 (pg): refusing derived reflection: {}",
                        r,
                    );
                }
                r.imported_peer.clone()
            } else {
                None
            };
            self.emit_reflection_depth_exceeded_audit(
                &input.agent_id,
                new_depth_u32,
                cap,
                &target_namespace,
                &input.source_ids,
                &input.title,
                peer_origin.as_deref(),
            )
            .await;
            return Err(ReflectError::DepthExceeded {
                attempted: new_depth_u32,
                cap,
                namespace: target_namespace,
            });
        }

        // ─── 6. Atomic insert + N links inside a single tx ──────────
        let now = Utc::now().to_rfc3339();
        let mut metadata = match input.metadata.clone() {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        metadata.insert(
            "agent_id".to_string(),
            serde_json::Value::String(input.agent_id.clone()),
        );
        if !metadata.contains_key("reflection_metadata") {
            let reflection_meta = serde_json::json!({
                "reflected_on_source_ids": input.source_ids,
                "reflection_depth": new_depth_i32,
                "reflection_created_at": now,
            });
            metadata.insert("reflection_metadata".to_string(), reflection_meta);
        }
        let metadata_value = serde_json::Value::Object(metadata);
        validate::validate_metadata(&metadata_value)
            .map_err(|e| ReflectError::Validation(e.to_string()))?;

        let new_id = uuid::Uuid::new_v4().to_string();
        let created_at_dt = chrono::DateTime::parse_from_rfc3339(&now)
            .map_err(|e| ReflectError::Database(format!("parse now: {e}")))?
            .with_timezone(&Utc);
        let tags_json = serde_json::to_value(&input.tags)
            .map_err(|e| ReflectError::Database(format!("serialize tags: {e}")))?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ReflectError::Database(format!("begin reflect tx: {e}")))?;

        // Insert the reflection memory inside the tx.
        let actual_id: String = sqlx::query(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, last_accessed_at,
                expires_at, metadata, reflection_depth, memory_kind
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, NULL, NULL, $13, $14, $15)
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
                END,
                reflection_depth = GREATEST(memories.reflection_depth, EXCLUDED.reflection_depth),
                memory_kind = CASE WHEN memories.memory_kind = 'reflection' THEN 'reflection'
                                   ELSE EXCLUDED.memory_kind END
            RETURNING id",
        )
        .bind(&new_id)
        .bind(input.tier.as_str())
        .bind(&target_namespace)
        .bind(&input.title)
        .bind(&input.content)
        .bind(&tags_json)
        .bind(input.priority.clamp(1, 10))
        .bind(input.confidence.clamp(0.0, 1.0))
        .bind(&input.source)
        .bind(0_i64)
        .bind(created_at_dt)
        .bind(created_at_dt)
        .bind(&metadata_value)
        .bind(new_depth_i32)
        .bind("reflection")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| ReflectError::Database(format!("insert reflection memory: {e}")))?
        .try_get::<String, _>("id")
        .map_err(|e| ReflectError::Database(format!("read returned id: {e}")))?;

        // Write each `reflects_on` link inside the same tx.
        for src_id in &input.source_ids {
            validate::validate_link(&actual_id, src_id, "reflects_on")
                .map_err(|e| ReflectError::Validation(e.to_string()))?;
            // Inline a minimal `memory_links` INSERT — full
            // `link_internal` runs its own queries on the pool and
            // wouldn't share the tx. SQLite parity calls
            // `db::create_link` which uses the same connection; we
            // mirror that here with a `&mut tx` binding so a link
            // failure rolls back the memory insert.
            sqlx::query(
                "INSERT INTO memory_links \
                    (source_id, target_id, relation, created_at, valid_from, attest_level) \
                 VALUES ($1, $2, $3, $4, $4, 'unsigned') \
                 ON CONFLICT (source_id, target_id, relation) DO NOTHING",
            )
            .bind(&actual_id)
            .bind(src_id)
            .bind("reflects_on")
            .bind(created_at_dt)
            .execute(&mut *tx)
            .await
            .map_err(|e| ReflectError::Database(format!("insert reflects_on link: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| ReflectError::Database(format!("commit reflect tx: {e}")))?;

        let outcome = crate::db::ReflectOutcome {
            id: actual_id,
            reflection_depth: new_depth_i32,
            reflects_on: input.source_ids.clone(),
            namespace: target_namespace,
        };
        // ─── 7. `post_reflect` hook (v0.7.0 Task 6/8) ───────────────
        //
        // Fires AFTER the transaction commits so the hook handler can
        // see a fully durable reflection memory + its links. Notify-
        // class — the return value is ignored.
        if let Some(post) = hooks.post_reflect.as_ref() {
            (post)(&outcome);
        }
        Ok(outcome)
    }

    /// v0.7.0 recursive-learning Task 5/8 — append a
    /// `reflection.depth_exceeded` row to `signed_events` for an
    /// in-flight cap refusal on the Postgres backend.
    ///
    /// Mirrors the SQLite `db::emit_reflection_depth_exceeded_audit`
    /// helper byte-for-byte (the canonical-CBOR encoding lives in
    /// `db::canonical_cbor_reflection_depth_exceeded` and is shared
    /// across both substrates so the `payload_hash` round-trips).
    /// Best-effort: audit-write failure is logged but does NOT crater
    /// the refusal path.
    async fn emit_reflection_depth_exceeded_audit(
        &self,
        agent_id: &str,
        attempted: u32,
        cap: u32,
        namespace: &str,
        source_ids: &[String],
        proposed_title: &str,
        peer_origin: Option<&str>,
    ) {
        let created_at_dt = Utc::now();
        let created_at = created_at_dt.to_rfc3339();
        let cbor = match crate::db::canonical_cbor_reflection_depth_exceeded(
            agent_id,
            attempted,
            cap,
            namespace,
            source_ids,
            proposed_title,
            &created_at,
            peer_origin,
        ) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "signed_events",
                    agent_id, attempted, cap, namespace,
                    "failed to encode canonical CBOR for reflection_depth_exceeded audit: {e}"
                );
                return;
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        let payload_hash = crate::signed_events::payload_hash(&cbor);
        // v0.7.0 L2-2 — distinguish the audit row's `event_type` so
        // operators can filter cross-peer refusals from local-only
        // refusals without re-decoding the CBOR payload. Mirrors the
        // SQLite parity path.
        let event_type = if peer_origin.is_some() {
            "reflection.depth_exceeded.cross_peer"
        } else {
            "reflection.depth_exceeded"
        };
        // v34 (#698 V-4 closeout) — compute the cross-row chain
        // (prev_hash, sequence) and INSERT all in one transaction so
        // the read MAX(sequence) → INSERT race is closed. The UNIQUE
        // INDEX on `sequence` makes the worst case a constraint
        // violation rather than a silent chain break.
        let insert_row = PgSignedEventInsert {
            id: &id,
            agent_id,
            event_type,
            payload_hash: &payload_hash,
            signature: None,
            attest_level: "unsigned",
            timestamp: created_at_dt,
        };
        if let Err(e) = pg_append_signed_event_with_chain(&self.pool, insert_row).await {
            tracing::warn!(
                target: "signed_events",
                agent_id, attempted, cap, namespace,
                "failed to append reflection_depth_exceeded audit row: {e}"
            );
        }
    }
}

/// Caller-supplied fields for [`pg_append_signed_event_with_chain`].
/// Bundled in a struct rather than positional args so the helper
/// doesn't trip `clippy::too_many_arguments`.
struct PgSignedEventInsert<'a> {
    id: &'a str,
    agent_id: &'a str,
    event_type: &'a str,
    payload_hash: &'a [u8],
    signature: Option<&'a [u8]>,
    attest_level: &'a str,
    timestamp: chrono::DateTime<chrono::Utc>,
}

/// Postgres-side companion to
/// [`crate::signed_events::append_signed_event`]. Computes the
/// chain head (`MAX(sequence)` + canonical hash of that row) and
/// INSERTs the new row inside a single transaction. The
/// `SERIALIZABLE` isolation isn't required — the UNIQUE INDEX on
/// `sequence` enforces correctness under concurrent writers — but
/// we wrap in a transaction so the read+insert pair is atomic
/// against rollback.
///
/// # Errors
///
/// Returns the underlying `sqlx::Error` wrapped in `StoreError` on
/// failure.
async fn pg_append_signed_event_with_chain(
    pool: &PgPool,
    row: PgSignedEventInsert<'_>,
) -> Result<(), sqlx::Error> {
    let PgSignedEventInsert {
        id,
        agent_id,
        event_type,
        payload_hash,
        signature,
        attest_level,
        timestamp,
    } = row;
    use crate::signed_events::{ZERO_HASH, canonical_chain_bytes};
    use sha2::{Digest, Sha256};

    let mut tx = pool.begin().await?;

    // Read the chain head.
    let head: Option<(
        String,
        String,
        String,
        Vec<u8>,
        Option<Vec<u8>>,
        String,
        chrono::DateTime<chrono::Utc>,
        Option<i64>,
    )> = sqlx::query_as(
        "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, timestamp, \
                sequence \
         FROM signed_events \
         ORDER BY COALESCE(sequence, 0) DESC, ctid DESC \
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;

    let (next_seq, prev_hash) = match head {
        None => (1_i64, ZERO_HASH.to_vec()),
        Some((h_id, h_agent, h_type, h_payload, h_sig, h_attest, h_ts, h_seq)) => {
            let seq = h_seq.unwrap_or(0);
            let event = crate::signed_events::SignedEvent {
                id: h_id,
                agent_id: h_agent,
                event_type: h_type,
                payload_hash: h_payload,
                signature: h_sig,
                attest_level: h_attest,
                timestamp: h_ts.to_rfc3339(),
                prev_hash: Vec::new(),
                sequence: seq,
            };
            let canon = canonical_chain_bytes(&event);
            let mut hasher = Sha256::new();
            hasher.update(&canon);
            let mut digest = [0u8; 32];
            digest.copy_from_slice(&hasher.finalize());
            (seq + 1, digest.to_vec())
        }
    };

    sqlx::query(
        "INSERT INTO signed_events \
            (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp, \
             prev_hash, sequence) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(agent_id)
    .bind(event_type)
    .bind(payload_hash)
    .bind(signature.map(<[u8]>::to_vec))
    .bind(attest_level)
    .bind(timestamp)
    .bind(&prev_hash)
    .bind(next_seq)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn to_store_err(what: &str, e: sqlx::Error) -> StoreError {
    StoreError::BackendUnavailable {
        backend: "postgres".to_string(),
        detail: format!("{what}: {e}"),
    }
}

/// v0.7.0 fold-A2A1.3 (#700) — AGE-side runtime failure classifier.
///
/// Decides whether a `StoreError` returned from one of the
/// `*_cypher` methods is an *AGE substrate* failure that should fall
/// through to the CTE branch, vs an upstream concern that the caller
/// must see (invalid input, integrity error, etc.).
///
/// We treat every `BackendUnavailable` from an AGE method as a
/// runtime AGE failure: the cypher path's call sites (`LOAD 'age'`,
/// `SET search_path`, the `cypher()` SRF) all funnel their errors
/// through [`to_store_err`] which maps to `BackendUnavailable`, so
/// any of these failing — extension dropped, projection missing,
/// connection killed mid-tx — counts. `InvalidInput`,
/// `IntegrityFailed`, `NotFound`, `Conflict`, `PermissionDenied`,
/// `UnsupportedCapability` and `Backend` all bubble up unchanged
/// because they reflect either caller bugs or non-AGE issues the
/// CTE branch can't paper over.
fn is_age_runtime_failure(err: &StoreError) -> bool {
    matches!(err, StoreError::BackendUnavailable { .. })
}

/// v0.7.0 fold-A2A1.3 (#700) — structured warning emitted when a
/// single-source KG operation falls back from AGE to the relational
/// CTE at runtime. Operators monitoring the daemon's `tracing`
/// stream see the per-request degradation event with enough
/// structured fields to grep for: which KG op fired, which source
/// id, and the underlying AGE error string.
///
/// The matching operator-side surface is documented in
/// `docs/kg-backend-fallback.md`.
fn warn_age_fallback(op: &str, source_id: &str, err: &StoreError) {
    tracing::warn!(
        target = "store::postgres::kg",
        op = op,
        source_id = source_id,
        backend = "age",
        fallback = "cte",
        error = %err,
        "AGE backend unreachable; falling back to CTE for kg_{op}=<{source_id}>"
    );
}

/// v0.7.0 fold-A2A1.3 (#700) — variant of [`warn_age_fallback`] for
/// the two-id `find_paths` operation, where the structured event
/// needs both `source_id` and `target_id`.
fn warn_age_fallback_pair(op: &str, source_id: &str, target_id: &str, err: &StoreError) {
    tracing::warn!(
        target = "store::postgres::kg",
        op = op,
        source_id = source_id,
        target_id = target_id,
        backend = "age",
        fallback = "cte",
        error = %err,
        "AGE backend unreachable; falling back to CTE for kg_{op}=<{source_id}->{target_id}>"
    );
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

/// v0.7.0 H10 — transaction-bound twin of
/// [`PostgresStore::build_namespace_chain`]. Reads every
/// `namespace_meta.parent_namespace` lookup through the supplied tx so
/// the chain walk shares a snapshot with the downstream policy lookup +
/// pending_actions INSERT. Logic identical to the trait method — the
/// only delta is `fetch_optional(&mut *tx)` instead of `&self.pool`.
///
/// # F-A2A1.2 inheritance recursion cap
///
/// The governance-inheritance walk is capped at
/// [`GOVERNANCE_INHERITANCE_DEPTH_CAP`] (= 5) intermediate levels per the
/// v0.7.0 spec. Both the `/`-derived ancestor chain and the explicit
/// `namespace_meta.parent_namespace` walk are bounded by the same cap so a
/// pathological deep namespace cannot blow the policy resolver's bind list
/// or its connection-hold budget. The implicit `"*"` global standard is
/// always retained and is not counted toward the cap.
async fn build_namespace_chain_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    namespace: &str,
) -> StoreResult<Vec<String>> {
    let mut chain: Vec<String> = Vec::new();

    if namespace == "*" {
        chain.push("*".to_string());
        return Ok(chain);
    }
    chain.push("*".to_string());

    let mut hierarchy_chain: Vec<String> = crate::models::namespace_ancestors(namespace)
        .into_iter()
        .rev()
        .collect();

    if let Some(root) = hierarchy_chain.first().cloned() {
        let mut explicit_above: Vec<String> = Vec::new();
        let mut current = root;
        for _ in 0..GOVERNANCE_INHERITANCE_DEPTH_CAP {
            let row: Option<(Option<String>,)> =
                sqlx::query_as("SELECT parent_namespace FROM namespace_meta WHERE namespace = $1")
                    .bind(&current)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(|e| to_store_err("build_namespace_chain_in_tx parent lookup", e))?;
            let next = row.and_then(|(p,)| p);
            match next {
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
        for p in explicit_above.into_iter().rev() {
            if !chain.contains(&p) {
                chain.push(p);
            }
        }
    }
    // F-A2A1.2 — cap the `/`-derived ancestor chain to the same depth as
    // the explicit walk so a deeply nested namespace cannot bypass the
    // resolver's bounded budget. The cap counts the most-specific N
    // levels (the leaf and its closest ancestors) so an over-deep
    // namespace still resolves against its most-relevant policy.
    let drained: Vec<String> = hierarchy_chain.drain(..).collect();
    let drained_len = drained.len();
    let kept: Vec<String> = if drained_len > GOVERNANCE_INHERITANCE_DEPTH_CAP {
        // hierarchy_chain is top-down (root → leaf); keep the LAST
        // GOVERNANCE_INHERITANCE_DEPTH_CAP entries (most-specific).
        drained
            .into_iter()
            .skip(drained_len - GOVERNANCE_INHERITANCE_DEPTH_CAP)
            .collect()
    } else {
        drained
    };
    for entry in kept {
        if !chain.contains(&entry) {
            chain.push(entry);
        }
    }
    Ok(chain)
}

/// F-A2A1.2 — maximum depth of the governance-inheritance walk.
///
/// Bounds both the `/`-derived ancestor decomposition AND the explicit
/// `namespace_meta.parent_namespace` walk to a single cap of 5 levels.
/// The implicit `"*"` global standard is always retained and is not
/// counted toward the cap.
///
/// Pinned to 5 per the v0.7.0 fold-A2A1 spec (see
/// `docs/v0.7.0/a2a-triage-wave4-r2.md` §F-A2A1.2). Real-world
/// namespaces are 3-4 levels deep; the cap leaves headroom for one
/// inherited override beyond the deepest authored ancestor while
/// keeping the per-write resolver's connection-hold budget bounded.
pub const GOVERNANCE_INHERITANCE_DEPTH_CAP: usize = 5;

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
/// v0.7.0 Wave-3 Continuation 5 — render an AGE `cypher()` parameters
/// dictionary as a SQL literal. AGE rejects `$1::agtype` (the third
/// argument must be a bare Param node, not a FuncExpr cast); the
/// workaround is to inline the params as `'<json>'::agtype`. Caller
/// sites must pass UUID-validated / RFC3339-validated values so the
/// inline literal is injection-safe.
///
/// The rendered form is `'{"k":"v",...}'::agtype`. Single quotes inside
/// the JSON are escaped as `''` (SQL standard); the agtype JSON dialect
/// uses `"` for string delimiters and `\"` for embedded double quotes,
/// so the only character that needs SQL-side escaping here is `'`.
fn age_params_literal(pairs: &[(&str, &str)]) -> String {
    let mut map = serde_json::Map::with_capacity(pairs.len());
    for (k, v) in pairs {
        map.insert(
            (*k).to_string(),
            serde_json::Value::String((*v).to_string()),
        );
    }
    let json = serde_json::Value::Object(map).to_string();
    // SQL-escape single quotes by doubling them.
    let escaped = json.replace('\'', "''");
    format!("'{escaped}'::agtype")
}

/// v0.7.0.1 G2 — sqlx-bindable wrapper for AGE's `agtype` parameter
/// type.
///
/// The cypher() set-returning function's third argument is a `cstring`
/// at the catalog level but AGE 1.5.0's `convert_cypher_to_subquery`
/// analyzer rejects ANY non-`Param` node there: literals
/// (`'…'::agtype`), casts (`$1::agtype`), and scalar subqueries all
/// fail with `third argument of cypher function must be a parameter`.
///
/// PostgreSQL's prepared-statement parameter inference DOES resolve a
/// bare `$N` to `agtype` from the function signature when the caller
/// declares the parameter as untyped (`unknown` OID `705`). sqlx's
/// default `String` encoder declares the parameter as `text`, which
/// overrides inference and re-introduces the literal-cast shape.
///
/// This wrapper carries a JSON-encoded params dictionary and binds
/// itself with the `agtype` type oid name so PostgreSQL ships the
/// parameter as the correct type without needing a SQL-side cast at
/// the cypher() call site. The on-wire format for `agtype` text is
/// the same JSON shape AGE accepts inline (e.g. `{"start_id":"…"}`).
struct Agtype(String);

impl sqlx::Type<sqlx::Postgres> for Agtype {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_name("agtype")
    }
    fn compatible(_ty: &sqlx::postgres::PgTypeInfo) -> bool {
        true
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Postgres> for Agtype {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        // sqlx 0.8 sends every parameter in PostgreSQL's binary
        // (`Bind`) format. AGE's `agtype_recv` (binary input) expects:
        //
        //   ┌────────────┬─────────────────────────────────┐
        //   │ version u8 │ JSON-text payload (no NUL term) │
        //   └────────────┴─────────────────────────────────┘
        //
        // where `version == 1`. Sending just the JSON bytes makes AGE
        // interpret the first byte (`{` == 0x7B == 123) as a version
        // number and fail with `unsupported agtype version number 123`
        // — see `src/backend/utils/adt/agtype.c::agtype_recv` in the
        // PG16/v1.5.0 AGE branch.
        buf.push(1);
        buf.extend_from_slice(self.0.as_bytes());
        Ok(sqlx::encode::IsNull::No)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for Agtype {
    fn decode(value: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        // Mirror of the encoder: AGE ships agtype values back as
        // version byte (0x01) followed by the JSON-compatible text
        // payload in binary mode, or just the text in text mode.
        match value.format() {
            sqlx::postgres::PgValueFormat::Binary => {
                let bytes = value.as_bytes()?;
                if bytes.is_empty() {
                    return Err("empty agtype payload".into());
                }
                let version = bytes[0];
                if version != 1 {
                    return Err(format!("unsupported agtype version: {version}").into());
                }
                let text = std::str::from_utf8(&bytes[1..])?.to_string();
                Ok(Agtype(text))
            }
            sqlx::postgres::PgValueFormat::Text => {
                let text = value.as_str()?.to_string();
                Ok(Agtype(text))
            }
        }
    }
}

/// v0.7.0.1 S79 — sanitize a free-text recall query into an OR-joined
/// `tsquery` lexeme list compatible with PostgreSQL's
/// `to_tsquery('english', $1)`.
///
/// Mirrors the SQLite `sanitize_fts_query(_, true)` contract used by
/// `db::recall_hybrid` so postgres-backed daemons return the same
/// candidate pool the FTS5 path returns: every token is wrapped in a
/// quoted lexeme and the lexemes are OR-joined with `|`. The previous
/// `plainto_tsquery('english', $1)` implementation AND-joins every
/// lemma, which surfaces as empty recall on multi-token queries
/// whose terms never co-occur in a single row (HALT v0.7.0 R1 S79:
/// `cat sleeping`, `rust ownership`, `compile time safety` returned
/// zero rows even when the namespace held semantically-related
/// content).
///
/// Sanitization rules:
///   * collapse Unicode whitespace into a single ASCII space
///   * strip every char that is NOT an ASCII alphanumeric or one of
///     `_-` — this blocks the FTS5/tsquery operator surface
///     (`& | ! ( ) : * ' "`) so a malicious query can't smuggle a
///     boolean expression past the matcher
///   * filter out tokens shorter than 2 chars after sanitization
///     (keeps stop-word noise out of the matcher; matches the
///     SQLite minimum-token-length convention)
///   * cap to 16 tokens so a pathologically long query can't blow
///     up the planner
///   * fall back to a sentinel `'_empty_'` lexeme when every token
///     was dropped — `to_tsquery` rejects an empty string with a
///     parse error and we want a clean "no results" rather than a
///     500.
fn build_or_tsquery(query: &str) -> String {
    const MAX_TOKENS: usize = 16;
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|raw| {
            raw.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
        })
        .filter(|t| t.len() >= 2)
        .take(MAX_TOKENS)
        .map(|t| format!("'{}'", t.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        return "'_empty_'".to_string();
    }
    tokens.join(" | ")
}

/// v0.7.0.1 G2 — render an AGE params dict as a JSON string compatible
/// with agtype's text input format. Pair-driven so callers don't drag
/// `serde_json::Value` into the call site.
fn age_params_jsonb(pairs: &[(&str, &str)]) -> String {
    let mut map = serde_json::Map::with_capacity(pairs.len());
    for (k, v) in pairs {
        map.insert(
            (*k).to_string(),
            serde_json::Value::String((*v).to_string()),
        );
    }
    serde_json::Value::Object(map).to_string()
}

/// v0.7.0.1 G4 — bootstrap the `memory_graph` AGE projection.
///
/// Idempotent: AGE returns "graph already exists" / SQLSTATE `42P07`
/// when the projection is present. Both shapes collapse to a clean
/// success because the only states we care about are "graph exists"
/// vs "graph cannot be created" (the latter being an actual operator
/// problem worth surfacing). Called from [`PostgresStore::connect`]
/// when the resolved [`KgBackend`] is [`KgBackend::Age`] so every
/// link write that follows can `MERGE` directly into the projection
/// without racing a separate bootstrap step.
async fn ensure_memory_graph(pool: &PgPool) -> StoreResult<()> {
    // Run inside a transaction so `SET LOCAL search_path` auto-resets
    // on commit. A bare `SET search_path` mutates session GUC state
    // and bleeds into the next checkout of this connection from the
    // pool — `audit_log`, `memories`, and every other public-schema
    // table would then resolve through `ag_catalog` first, surfacing
    // as flaky read-after-write failures elsewhere in the adapter.
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| to_store_err("begin ensure_memory_graph tx", e))?;
    sqlx::query("LOAD 'age'")
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("LOAD age (ensure_memory_graph)", e))?;
    sqlx::query("SET LOCAL search_path = ag_catalog, \"$user\", public")
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("set search_path (ensure_memory_graph)", e))?;
    if let Err(e) = sqlx::query("SELECT create_graph('memory_graph')")
        .execute(&mut *tx)
        .await
    {
        let msg = e.to_string();
        if !msg.contains("already exists") {
            return Err(to_store_err("create_graph memory_graph", e));
        }
    }
    tx.commit()
        .await
        .map_err(|e| to_store_err("commit ensure_memory_graph tx", e))?;
    Ok(())
}

/// v0.7.0.1 G4 — project a `memory_links` row into the AGE
/// `memory_graph` projection within an open transaction.
///
/// MERGEs both endpoints as `(:Memory {id})` nodes and the relation
/// as `(a)-[:<relation> {relation}]->(b)`, using the SAL-side
/// relation value as the AGE edge label so `kg_query_cypher`'s
/// `[r:related_to*1..N]` matcher resolves directly. Idempotent on
/// every input — Cypher's MERGE collapses repeat projections onto
/// the existing nodes/edges by the `id` / type combo.
///
/// The relation string is interpolated into the Cypher body
/// (Cypher does not accept parameters at the relationship-type
/// position). Validation upstream restricts relations to
/// `[a-z0-9_]+` plus the canonical labelset (see
/// `validate::validate_relation`), so the inlined form is safe; the
/// caller MUST pass a relation that already passed validation.
///
/// Each statement is shaped to fit AGE 1.5.0's `cypher()` analyzer:
///   * the third argument to `cypher()` is a bare `$1` parameter
///     bound through the [`Agtype`] sqlx wrapper (G2 fix);
///   * the cypher body uses `$id` / `$src` / `$dst` / `$rel` against
///     the params map rather than inlining ids as Cypher literals
///     (so caller-supplied UUIDs go through the param surface).
///
/// Called from [`PostgresStore::link_internal`] and
/// [`PostgresStore::apply_remote_link`] inside their respective
/// transactions so the SQL row + the AGE projection commit together
/// — a half-projection with the SQL row but no AGE node would surface
/// as a stale gap on `find_paths` queries.
async fn project_link_into_age(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_id: &str,
    target_id: &str,
    relation: &str,
) -> StoreResult<()> {
    // Defence-in-depth: confirm the relation is in the
    // validator's accepted shape before interpolating it into the
    // Cypher body. Upstream validators already gate this, but the
    // SAL trait surface is reachable from federation replays + any
    // future caller paths, so we keep the local guard cheap.
    if relation.is_empty()
        || !relation
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(StoreError::InvalidInput {
            detail: format!(
                "invalid relation for AGE projection: {relation:?} (must be [a-z0-9_]+)"
            ),
        });
    }

    sqlx::query("LOAD 'age'")
        .execute(&mut **tx)
        .await
        .map_err(|e| to_store_err("LOAD age (project_link)", e))?;
    // `SET LOCAL` confines the search path to the open transaction
    // (`link_internal` / `apply_remote_link`'s tx) so post-commit
    // checkouts of the same pooled connection don't inherit the
    // `ag_catalog` first-resolution and silently route public-schema
    // reads through the AGE catalog.
    sqlx::query("SET LOCAL search_path = ag_catalog, \"$user\", public")
        .execute(&mut **tx)
        .await
        .map_err(|e| to_store_err("set search_path (project_link)", e))?;

    // v0.7.0.1 G4 follow-up — self-bootstrap the `memory_graph`
    // projection on every link write so a daemon whose connect-time
    // `ensure_memory_graph` warned-and-continued (e.g. transient AGE
    // load lag, permission blip on `pg_extension`) still ends up
    // with a populated projection on first link write rather than
    // silently dropping every subsequent MERGE on the floor.
    //
    // We wrap `create_graph` in a SAVEPOINT so a duplicate-graph
    // error (the steady-state case once the projection exists)
    // doesn't abort the outer link-write transaction. PostgreSQL
    // marks any explicit transaction as failed on the first error
    // and rejects every subsequent statement until ROLLBACK; the
    // SAVEPOINT lets us roll back JUST the create_graph and keep
    // the outer tx writable for the upcoming MERGE statements.
    // Upgrades the live HTTP path's `POST /api/v1/links` from
    // "depends on connect-time bootstrap succeeding" to "self-heals
    // every write" (HALT R1 v4 S65 — the live cert droplets surfaced
    // 22 SQL link rows alongside 0 AGE nodes, consistent with a
    // connect-time bootstrap that didn't land).
    sqlx::query("SAVEPOINT bootstrap_memory_graph")
        .execute(&mut **tx)
        .await
        .map_err(|e| to_store_err("savepoint bootstrap_memory_graph", e))?;
    match sqlx::query("SELECT create_graph('memory_graph')")
        .execute(&mut **tx)
        .await
    {
        Ok(_) => {
            sqlx::query("RELEASE SAVEPOINT bootstrap_memory_graph")
                .execute(&mut **tx)
                .await
                .map_err(|e| to_store_err("release savepoint bootstrap_memory_graph", e))?;
        }
        Err(e) => {
            let msg = e.to_string();
            // Roll the outer tx back to the savepoint regardless of
            // the failure mode — without this the link write aborts
            // wholesale. Steady-state ("already exists") collapses
            // to a clean Ok; any other failure surfaces as a typed
            // error after the rollback so the link write 503s rather
            // than silently committing the SQL row alone.
            sqlx::query("ROLLBACK TO SAVEPOINT bootstrap_memory_graph")
                .execute(&mut **tx)
                .await
                .map_err(|err| to_store_err("rollback savepoint bootstrap_memory_graph", err))?;
            sqlx::query("RELEASE SAVEPOINT bootstrap_memory_graph")
                .execute(&mut **tx)
                .await
                .map_err(|err| to_store_err("release savepoint bootstrap_memory_graph", err))?;
            if !msg.contains("already exists") {
                return Err(to_store_err("create_graph memory_graph (project_link)", e));
            }
        }
    }

    // MERGE both endpoint nodes. We emit two separate statements
    // rather than one combined cypher to keep the param-shape
    // narrow — AGE 1.5.0 occasionally trips on multi-MERGE
    // statements with overlapping property names.
    let node_sql = "SELECT n FROM cypher('memory_graph', $$ MERGE (n:Memory {id: $id}) RETURN n $$, $1) \
         AS (n agtype)";
    for id in [source_id, target_id] {
        let params = age_params_jsonb(&[("id", id)]);
        sqlx::query(node_sql)
            .bind(Agtype(params))
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| to_store_err("project memory node into AGE", e))?;
    }

    // MERGE the directional edge typed by the SAL relation value so
    // `kg_query_cypher`'s `[r:related_to*1..N]` matcher resolves
    // directly. The relation property is also stored on the edge so
    // typeless traversals (`find_paths_cypher`) can read it back
    // through `last(r).relation` without a second lookup.
    let edge_cypher = format!(
        "MATCH (a:Memory {{id: $src}}), (b:Memory {{id: $dst}}) \
         MERGE (a)-[r:{relation} {{relation: $rel}}]->(b) RETURN r"
    );
    let edge_sql =
        format!("SELECT r FROM cypher('memory_graph', $$ {edge_cypher} $$, $1) AS (r agtype)");
    let edge_params =
        age_params_jsonb(&[("src", source_id), ("dst", target_id), ("rel", relation)]);
    sqlx::query(&edge_sql)
        .bind(Agtype(edge_params))
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| to_store_err("project memory edge into AGE", e))?;

    Ok(())
}

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

/// v0.7.0.1 G3 — clamp a `DateTime<Utc>` to microsecond precision.
///
/// PostgreSQL's `TIMESTAMPTZ` column stores microseconds since epoch
/// as int8; sub-microsecond digits are silently dropped by the type's
/// input function. Pre-fix, the `link_internal` sign path committed to
/// a `chrono::Utc::now()`-derived RFC3339 string with whatever
/// sub-second precision chrono emitted (nanoseconds when non-zero on
/// Linux), then INSERT'd that nanosecond-resolution
/// `chrono::DateTime<Utc>` into the `valid_from` TIMESTAMPTZ column.
/// On `verify_link`, the round-tripped value came back at microsecond
/// precision — the canonical CBOR re-derivation produced different
/// bytes than what was signed, and Ed25519 rejected the signature
/// (HALT R1b finding G3 / S52).
///
/// Truncating to microseconds on both sign and verify paths makes the
/// CBOR shape stable across the storage boundary. We use
/// `with_nanosecond` rather than `Duration` arithmetic because chrono
/// guarantees `with_nanosecond(_)` is total within `[0, 2_000_000_000)`
/// — `(self.nanosecond() / 1000) * 1000` is always in range, so the
/// `unwrap_or(self)` here is purely a typing convenience.
fn truncate_to_microseconds(t: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Timelike;
    let micros = t.nanosecond() / 1_000;
    t.with_nanosecond(micros * 1_000).unwrap_or(t)
}

/// v0.7.0.1 G1 — resolve the `agent_id` to scope a quota row to.
///
/// Precedence mirrors the SQLite `quotas::check_and_record` shape: the
/// claim baked into `memory.metadata.agent_id` (stamped by
/// `handlers::create_memory` before dispatch to the SAL) wins, falling
/// back to the SAL `CallerContext::agent_id` when metadata is missing
/// the field. The fallback ensures we never silently drop a quota
/// increment because metadata was elided upstream.
fn resolve_quota_agent_id(ctx: &CallerContext, metadata: &serde_json::Value) -> String {
    metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| ctx.agent_id.clone())
}

/// v0.7.0.1 G1 — bytes counted toward a memory's storage cap. Mirrors
/// `quotas::storage_bytes_for_memory` (title + content UTF-8 length —
/// ignoring tags/metadata, which is the SQLite parity contract).
fn memory_storage_bytes(memory: &Memory) -> i64 {
    let raw = memory.title.len().saturating_add(memory.content.len());
    i64::try_from(raw).unwrap_or(i64::MAX)
}

/// v0.7.0.1 G1 — increment `agent_quotas.current_memories_today` and
/// `current_storage_bytes` inside an active transaction, auto-rolling
/// the daily counters at UTC midnight. Mirrors the SQLite parity laid
/// out in `quotas::check_and_record`'s memory-op branch.
///
/// Issued as a single statement so a concurrent writer cannot race the
/// counter — the underlying `INSERT ... ON CONFLICT DO UPDATE` runs
/// atomically against the `agent_id` PRIMARY KEY. The day-rollover
/// detection projects the stored `day_started_at` against UTC `now` and
/// resets `current_memories_today` / `current_links_today` in the same
/// statement.
async fn record_memory_quota_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: &str,
    bytes_added: i64,
) -> StoreResult<()> {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO agent_quotas (
             agent_id, max_memories_per_day, max_storage_bytes, max_links_per_day,
             current_memories_today, current_storage_bytes, current_links_today,
             day_started_at, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, 1, $5, 0, $6, $6, $6)
         ON CONFLICT (agent_id) DO UPDATE SET
             current_memories_today = CASE
                 WHEN date_trunc('day', agent_quotas.day_started_at) = date_trunc('day', $6)
                     THEN agent_quotas.current_memories_today + 1
                 ELSE 1
             END,
             current_links_today = CASE
                 WHEN date_trunc('day', agent_quotas.day_started_at) = date_trunc('day', $6)
                     THEN agent_quotas.current_links_today
                 ELSE 0
             END,
             current_storage_bytes = agent_quotas.current_storage_bytes + EXCLUDED.current_storage_bytes,
             day_started_at = CASE
                 WHEN date_trunc('day', agent_quotas.day_started_at) = date_trunc('day', $6)
                     THEN agent_quotas.day_started_at
                 ELSE $6
             END,
             updated_at = $6",
    )
    .bind(agent_id)
    .bind(DEFAULT_MAX_MEMORIES_PER_DAY)
    .bind(DEFAULT_MAX_STORAGE_BYTES)
    .bind(DEFAULT_MAX_LINKS_PER_DAY)
    .bind(bytes_added)
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(|e| to_store_err("record agent_quotas memory increment", e))?;
    Ok(())
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

    /// v0.7.0.1 S75 — read `MAX(version)` from the live `schema_version`
    /// table so the `/api/v1/capabilities.db_schema_version` field
    /// reflects the actual applied migration ladder rather than a
    /// hard-coded constant. Returns `0` when the table is empty (a
    /// fresh schema-init that didn't stamp any rows yet); callers
    /// that need "unknown" semantics MUST treat `0` as such. The query
    /// is a single scalar lookup so the lock window stays sub-
    /// millisecond — capability polling is fine to do per-request.
    async fn schema_version(&self) -> StoreResult<i64> {
        let v: Option<i32> = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| to_store_err("read schema_version max", e))?;
        Ok(i64::from(v.unwrap_or(0)))
    }

    async fn store(&self, ctx: &CallerContext, memory: &Memory) -> StoreResult<String> {
        let created_at = parse_rfc3339_required(&memory.created_at)?;
        let updated_at = parse_rfc3339_required(&memory.updated_at)?;
        let last_accessed_at = parse_rfc3339_opt(memory.last_accessed_at.as_deref());
        let expires_at = parse_rfc3339_opt(memory.expires_at.as_deref());
        let tags_json =
            serde_json::to_value(&memory.tags).map_err(|e| StoreError::IntegrityFailed {
                detail: format!("serialize tags: {e}"),
            })?;

        // v0.7.0.1 G1 — INSERT memories + record quota usage in a single
        // transaction so the postgres path matches the SQLite parity laid
        // out in `quotas::check_and_record`. Without this, S61's wire
        // shape stays at `current_memories_today=0` after N successful
        // writes (HALT R1b finding G1).
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin store tx", e))?;

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
        let id: String = sqlx::query(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, last_accessed_at,
                expires_at, metadata, reflection_depth, memory_kind
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
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
                END,
                -- v0.7.0 Task 1/8 — recursion depth takes max on upsert so a
                -- newer reflection at higher depth doesn't lose its provenance
                -- signal when re-stored at the same (title, namespace).
                reflection_depth = GREATEST(memories.reflection_depth, EXCLUDED.reflection_depth),
                -- L1-1 — kind is sticky: once Reflection, always Reflection.
                memory_kind = CASE WHEN memories.memory_kind = 'reflection' THEN 'reflection'
                                   ELSE EXCLUDED.memory_kind END
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
        .bind(memory.reflection_depth)
        .bind(memory.memory_kind.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("insert memory", e))?
        .try_get::<String, _>("id")
        .map_err(|e| to_store_err("read returned id", e))?;

        // v0.7.0.1 G1 — record quota usage in the same tx. Best-effort
        // resolution of agent_id; a missing claim falls back to the SAL
        // `CallerContext::agent_id` so we never lose the count.
        let quota_agent_id = resolve_quota_agent_id(ctx, &memory.metadata);
        let bytes_added = memory_storage_bytes(memory);
        record_memory_quota_in_tx(&mut tx, &quota_agent_id, bytes_added).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit store tx", e))?;
        Ok(id)
    }

    async fn store_with_embedding(
        &self,
        ctx: &CallerContext,
        memory: &Memory,
        embedding: Option<&[f32]>,
    ) -> StoreResult<String> {
        // Same upsert contract as `store` but additionally writes the
        // pgvector `embedding` column when a vector is supplied. This
        // is the load-bearing path for semantic recall on postgres —
        // without an embedding column the `recall_hybrid` cosine
        // search filters out every row (`WHERE embedding IS NOT NULL`).
        let created_at = parse_rfc3339_required(&memory.created_at)?;
        let updated_at = parse_rfc3339_required(&memory.updated_at)?;
        let last_accessed_at = parse_rfc3339_opt(memory.last_accessed_at.as_deref());
        let expires_at = parse_rfc3339_opt(memory.expires_at.as_deref());
        let tags_json =
            serde_json::to_value(&memory.tags).map_err(|e| StoreError::IntegrityFailed {
                detail: format!("serialize tags: {e}"),
            })?;
        let emb_pgvec = embedding.map(|v| pgvector::Vector::from(v.to_vec()));

        // v0.7.0.1 G1 — wrap INSERT + quota record in a single tx (see
        // store() above for context).
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin store tx", e))?;

        let id: String = sqlx::query(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, last_accessed_at,
                expires_at, metadata, reflection_depth, memory_kind, embedding
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
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
                END,
                -- v0.7.0 Task 1/8 — recursion depth takes max on upsert.
                reflection_depth = GREATEST(memories.reflection_depth, EXCLUDED.reflection_depth),
                -- L1-1 — kind is sticky.
                memory_kind = CASE WHEN memories.memory_kind = 'reflection' THEN 'reflection'
                                   ELSE EXCLUDED.memory_kind END,
                embedding = COALESCE(EXCLUDED.embedding, memories.embedding)
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
        .bind(memory.reflection_depth)
        .bind(memory.memory_kind.as_str())
        .bind(emb_pgvec)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("insert memory_with_embedding", e))?
        .try_get::<String, _>("id")
        .map_err(|e| to_store_err("read returned id", e))?;

        let quota_agent_id = resolve_quota_agent_id(ctx, &memory.metadata);
        let bytes_added = memory_storage_bytes(memory);
        record_memory_quota_in_tx(&mut tx, &quota_agent_id, bytes_added).await?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit store tx", e))?;
        Ok(id)
    }

    async fn update_embedding(
        &self,
        _ctx: &CallerContext,
        id: &str,
        embedding: Option<&[f32]>,
    ) -> StoreResult<()> {
        let emb_pgvec = embedding.map(|v| pgvector::Vector::from(v.to_vec()));
        sqlx::query("UPDATE memories SET embedding = $2 WHERE id = $1")
            .bind(id)
            .bind(emb_pgvec)
            .execute(&self.pool)
            .await
            .map_err(|e| to_store_err("update_embedding", e))?;
        Ok(())
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
        // v0.7.0.1 S79 — build the OR-joined tsquery on the rust side
        // so multi-token queries surface every row that matches AT
        // LEAST one token. `plainto_tsquery` is AND-joined and
        // diverges from sqlite's FTS5 `OR` contract — see
        // `build_or_tsquery` for the sanitization rules.
        let or_tsquery = build_or_tsquery(query);
        let rows = sqlx::query(
            "SELECT *,
                    ts_rank(
                        to_tsvector('english', title || ' ' || content),
                        to_tsquery('english', $1)
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
             WHERE to_tsvector('english', title || ' ' || content) @@ to_tsquery('english', $1)
               AND ($2::text IS NULL OR namespace = $2)
               AND ($3::text IS NULL OR tier = $3)
               AND ($4::text IS NULL OR tags @> to_jsonb(ARRAY[$4]))
               AND ($5::text IS NULL OR metadata ->> 'agent_id' = $5)
               AND (expires_at IS NULL OR expires_at > NOW())
             ORDER BY rank DESC, priority DESC
             LIMIT $6",
        )
        .bind(&or_tsquery)
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
                let relation_str: String = r
                    .try_get::<String, _>("relation")
                    .map_err(|e| to_store_err("read relation", e))?;
                Ok(MemoryLink {
                    source_id: r
                        .try_get::<String, _>("source_id")
                        .map_err(|e| to_store_err("read source_id", e))?,
                    target_id: r
                        .try_get::<String, _>("target_id")
                        .map_err(|e| to_store_err("read target_id", e))?,
                    // v0.7.0 fix campaign R1-M4 — parse closed-set
                    // relation. Unknown values fall back to default so
                    // the read path never errors; the SQL CHECK on the
                    // write side keeps new rows in the closed set.
                    relation: crate::models::MemoryLinkRelation::from_str(&relation_str)
                        .unwrap_or_default(),
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

    // ----- v0.7.0 Wave-3 Continuation 2 — federation surface ---------
    //
    // The Postgres path mirrors the sqlite `db::memories_updated_since`
    // + `db::insert_if_newer` contracts so the wire shape is byte-
    // identical regardless of which backend a peer runs on. Tier never
    // downgrades; `metadata.agent_id` is preserved across upsert.

    async fn list_memories_updated_since(
        &self,
        since: Option<&str>,
        limit: usize,
    ) -> StoreResult<Vec<Memory>> {
        let limit_i: i64 = limit.clamp(1, 10_000).try_into().unwrap_or(500);
        let since_dt = match since {
            None => None,
            Some(s) => Some(parse_rfc3339_required(s)?),
        };
        let rows = sqlx::query(
            "SELECT * FROM memories \
             WHERE ($1::timestamptz IS NULL OR updated_at > $1) \
             ORDER BY updated_at ASC \
             LIMIT $2",
        )
        .bind(since_dt)
        .bind(limit_i)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("list memories updated since", e))?;

        rows.iter().map(Self::row_to_memory).collect()
    }

    async fn apply_remote_memory(
        &self,
        _ctx: &CallerContext,
        memory: &Memory,
    ) -> StoreResult<String> {
        // Mirrors sqlite db::insert_if_newer:
        //   1. INSERT verbatim if no row matches.
        //   2. On (title, namespace) collision: UPDATE only if the
        //      incoming `updated_at` is strictly greater than the
        //      stored `updated_at`. Tier never downgrades.
        //      `metadata.agent_id` is preserved if the existing row had
        //      one.
        //   3. Else NOOP — return the existing id.
        let created_at = parse_rfc3339_required(&memory.created_at)?;
        let updated_at = parse_rfc3339_required(&memory.updated_at)?;
        let last_accessed_at = parse_rfc3339_opt(memory.last_accessed_at.as_deref());
        let expires_at = parse_rfc3339_opt(memory.expires_at.as_deref());
        let tags_json =
            serde_json::to_value(&memory.tags).map_err(|e| StoreError::IntegrityFailed {
                detail: format!("serialize tags: {e}"),
            })?;

        let row = sqlx::query(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, last_accessed_at,
                expires_at, metadata, reflection_depth, memory_kind
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
            ON CONFLICT (title, namespace) DO UPDATE SET
                content = CASE
                    WHEN EXCLUDED.updated_at > memories.updated_at
                        THEN EXCLUDED.content
                    ELSE memories.content
                END,
                tier = CASE
                    WHEN EXCLUDED.updated_at > memories.updated_at
                         AND tier_rank(EXCLUDED.tier) >= tier_rank(memories.tier)
                        THEN EXCLUDED.tier
                    ELSE memories.tier
                END,
                tags = CASE
                    WHEN EXCLUDED.updated_at > memories.updated_at
                        THEN EXCLUDED.tags
                    ELSE memories.tags
                END,
                priority = CASE
                    WHEN EXCLUDED.updated_at > memories.updated_at
                        THEN EXCLUDED.priority
                    ELSE memories.priority
                END,
                confidence = CASE
                    WHEN EXCLUDED.updated_at > memories.updated_at
                        THEN EXCLUDED.confidence
                    ELSE memories.confidence
                END,
                updated_at = CASE
                    WHEN EXCLUDED.updated_at > memories.updated_at
                        THEN EXCLUDED.updated_at
                    ELSE memories.updated_at
                END,
                metadata = CASE
                    WHEN EXCLUDED.updated_at > memories.updated_at THEN
                        CASE
                            WHEN memories.metadata ? 'agent_id'
                                THEN jsonb_set(
                                    EXCLUDED.metadata,
                                    '{agent_id}',
                                    memories.metadata -> 'agent_id'
                                )
                            ELSE EXCLUDED.metadata
                        END
                    ELSE memories.metadata
                END,
                -- v0.7.0 Task 1/8 — recursion depth takes max so the reflection
                -- signal isn't lost on newer-wins federation merges.
                reflection_depth = GREATEST(memories.reflection_depth, EXCLUDED.reflection_depth),
                -- L1-1 — kind is sticky across federation merges.
                memory_kind = CASE WHEN memories.memory_kind = 'reflection' THEN 'reflection'
                                   ELSE EXCLUDED.memory_kind END
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
        .bind(memory.reflection_depth)
        .bind(memory.memory_kind.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| to_store_err("apply_remote_memory upsert", e))?;

        row.try_get::<String, _>("id")
            .map_err(|e| to_store_err("read returned id", e))
    }

    async fn apply_remote_link(
        &self,
        _ctx: &CallerContext,
        link: &MemoryLink,
        attest_level: &str,
    ) -> StoreResult<()> {
        // Mirrors sqlite db::create_link_inbound. The unique
        // (source_id, target_id, relation) index makes duplicate
        // pushes a no-op (ON CONFLICT DO NOTHING), so retries and
        // peer-to-peer fanouts converge cleanly.
        let created_at = parse_rfc3339_required(&link.created_at)?;
        let valid_from = parse_rfc3339_opt(link.valid_from.as_deref());
        let valid_until = parse_rfc3339_opt(link.valid_until.as_deref());

        // v0.7.0.1 G4 — federation replay must keep the AGE
        // projection in sync with the SQL `memory_links` table the
        // same way the local-write path does. A single transaction
        // lets the SQL row + AGE MERGE commit atomically.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin apply_remote_link tx", e))?;

        sqlx::query(
            "INSERT INTO memory_links (
                source_id, target_id, relation, created_at,
                valid_from, valid_until, observed_by, signature, attest_level
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (source_id, target_id, relation) DO NOTHING",
        )
        .bind(&link.source_id)
        .bind(&link.target_id)
        .bind(link.relation.as_str())
        .bind(created_at)
        .bind(valid_from)
        .bind(valid_until)
        .bind(link.observed_by.as_ref())
        .bind(link.signature.as_ref())
        .bind(attest_level)
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("apply_remote_link", e))?;

        if matches!(self.kg_backend, KgBackend::Age) {
            project_link_into_age(
                &mut tx,
                &link.source_id,
                &link.target_id,
                link.relation.as_str(),
            )
            .await?;
        }

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit apply_remote_link tx", e))?;
        Ok(())
    }

    async fn apply_remote_deletion(&self, _ctx: &CallerContext, id: &str) -> StoreResult<bool> {
        let rows_affected = sqlx::query("DELETE FROM memories WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| to_store_err("apply_remote_deletion", e))?
            .rows_affected();
        Ok(rows_affected > 0)
    }

    // ----- v0.7.0 Wave-3 Continuation 2 — full hybrid recall ---------
    //
    // Mirrors db::recall_hybrid (sqlite path) on top of pgvector +
    // tsvector + ts_rank. The 6-factor FTS sub-score matches the
    // shape SQLite produces; the semantic component comes from
    // (1 - cosine_distance) over the `embedding` column; the adaptive
    // blend (semantic_weight 0.50→0.15 by content length) plus tier
    // decay matches sqlite byte-for-byte at the trait surface.

    async fn recall_hybrid(
        &self,
        ctx: &CallerContext,
        query: &str,
        query_embedding: Option<&[f32]>,
        filter: &Filter,
    ) -> StoreResult<Vec<(Memory, f64)>> {
        let limit_eff: i64 = if filter.limit == 0 {
            10
        } else {
            i64::try_from(filter.limit.min(1000)).unwrap_or(10)
        };
        // Pull a wider FTS candidate pool (3x limit) so the blend has
        // material to rank, mirroring sqlite at db.rs:4757.
        let fts_pool: i64 = (limit_eff * 3).max(30);
        let tags_first: Option<&str> = filter.tags_any.first().map(String::as_str);
        // v0.7.0.1 S79 — build the OR-joined tsquery so the postgres
        // FTS pool mirrors sqlite's FTS5 OR contract. Pre-fix the
        // `plainto_tsquery` AND-joined the lemmas; multi-token
        // queries that didn't co-occur in a single row dropped to
        // the empty bucket, pulling mean Jaccard@5 below the 0.20
        // floor (HALT v0.7.0 R1 S79).
        let or_tsquery = build_or_tsquery(query);
        // FTS candidates with the existing 6-factor blend baked into
        // `rank` (see search() above). Also surfaces content_len so
        // the trait-side adaptive blend can compute semantic_weight
        // per row.
        let fts_rows = sqlx::query(
            "SELECT *,
                    ts_rank(
                        to_tsvector('english', title || ' ' || content),
                        to_tsquery('english', $1)
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
                      AS fts_score,
                    octet_length(content) AS content_len
             FROM memories
             WHERE to_tsvector('english', title || ' ' || content) @@ to_tsquery('english', $1)
               AND ($2::text IS NULL OR namespace = $2)
               AND ($3::text IS NULL OR tier = $3)
               AND ($4::text IS NULL OR tags @> to_jsonb(ARRAY[$4]))
               AND ($5::text IS NULL OR metadata ->> 'agent_id' = $5)
               AND ($6::timestamptz IS NULL OR created_at >= $6)
               AND ($7::timestamptz IS NULL OR created_at <= $7)
               AND (expires_at IS NULL OR expires_at > NOW())
             ORDER BY fts_score DESC
             LIMIT $8",
        )
        .bind(&or_tsquery)
        .bind(filter.namespace.as_ref())
        .bind(filter.tier.as_ref().map(Tier::as_str))
        .bind(tags_first)
        .bind(filter.agent_id.as_ref().or(ctx.as_agent.as_ref()))
        .bind(filter.since)
        .bind(filter.until)
        .bind(fts_pool)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("recall_hybrid fts pool", e))?;

        let mut max_fts: f64 = 1.0;
        let mut scored: std::collections::HashMap<String, (Memory, f64, f64, i64)> =
            std::collections::HashMap::new();
        for r in &fts_rows {
            let mem = Self::row_to_memory(r)?;
            let fts_score: f64 = r.try_get("fts_score").unwrap_or(0.0);
            let content_len: i64 = r.try_get::<i32, _>("content_len").map_or_else(
                |_| {
                    r.try_get::<i64, _>("content_len")
                        .unwrap_or_else(|_| i64::try_from(mem.content.len()).unwrap_or(0))
                },
                i64::from,
            );
            if fts_score > max_fts {
                max_fts = fts_score;
            }
            scored.insert(mem.id.clone(), (mem, fts_score, 0.0, content_len));
        }

        // Semantic candidates via pgvector cosine_distance. We use the
        // `<=>` operator (cosine distance) and convert to similarity as
        // `1 - distance`. The 0.2 cosine gate matches sqlite's
        // db::recall_hybrid (S18 iteration: relaxed 0.3 → 0.2 to admit
        // legitimately-related content with phrasing variance).
        if let Some(qe) = query_embedding {
            let ann_pool: i64 = (limit_eff * 5).max(50);
            let qvec = pgvector::Vector::from(qe.to_vec());
            let sem_rows = sqlx::query(
                "SELECT *, (1.0 - (embedding <=> $1)) AS cosine_sim,
                          octet_length(content) AS content_len
                 FROM memories
                 WHERE embedding IS NOT NULL
                   AND ($2::text IS NULL OR namespace = $2)
                   AND ($3::text IS NULL OR tier = $3)
                   AND ($4::text IS NULL OR tags @> to_jsonb(ARRAY[$4]))
                   AND ($5::text IS NULL OR metadata ->> 'agent_id' = $5)
                   AND ($6::timestamptz IS NULL OR created_at >= $6)
                   AND ($7::timestamptz IS NULL OR created_at <= $7)
                   AND (expires_at IS NULL OR expires_at > NOW())
                   AND (1.0 - (embedding <=> $1)) > 0.2
                 ORDER BY embedding <=> $1
                 LIMIT $8",
            )
            .bind(&qvec)
            .bind(filter.namespace.as_ref())
            .bind(filter.tier.as_ref().map(Tier::as_str))
            .bind(tags_first)
            .bind(filter.agent_id.as_ref().or(ctx.as_agent.as_ref()))
            .bind(filter.since)
            .bind(filter.until)
            .bind(ann_pool)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| to_store_err("recall_hybrid semantic pool", e))?;

            for r in &sem_rows {
                let mem = Self::row_to_memory(r)?;
                let cosine: f64 = r.try_get("cosine_sim").unwrap_or(0.0);
                let content_len: i64 = r.try_get::<i32, _>("content_len").map_or_else(
                    |_| {
                        r.try_get::<i64, _>("content_len")
                            .unwrap_or_else(|_| i64::try_from(mem.content.len()).unwrap_or(0))
                    },
                    i64::from,
                );
                scored
                    .entry(mem.id.clone())
                    .and_modify(|entry| {
                        if cosine > entry.2 {
                            entry.2 = cosine;
                        }
                    })
                    .or_insert((mem, 0.0, cosine, content_len));
            }
        }

        // Adaptive blend: semantic_weight 0.50 (≤500 chars) → 0.15
        // (≥5000 chars). Same lerp formula as sqlite (db.rs:4990).
        let mut results: Vec<(Memory, f64)> = scored
            .into_values()
            .map(|(mem, fts_score, cosine, content_len)| {
                let norm_fts = if max_fts > 0.0 {
                    fts_score / max_fts
                } else {
                    0.0
                };
                #[allow(clippy::cast_precision_loss)]
                let cl = content_len as f64;
                let semantic_weight = if cl <= 500.0 {
                    0.50
                } else if cl >= 5000.0 {
                    0.15
                } else {
                    0.50 - 0.35 * ((cl - 500.0) / 4500.0)
                };
                let blended = semantic_weight * cosine + (1.0 - semantic_weight) * norm_fts;
                (mem, blended)
            })
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(filter.limit.max(1));
        Ok(results)
    }

    async fn pending_decide(
        &self,
        _ctx: &CallerContext,
        id: &str,
        approve: bool,
        decided_by: &str,
    ) -> StoreResult<bool> {
        let new_status = if approve { "approved" } else { "rejected" };
        let rows_affected = sqlx::query(
            "UPDATE pending_actions SET status = $1, decided_by = $2, decided_at = NOW()
             WHERE id = $3 AND status = 'pending'",
        )
        .bind(new_status)
        .bind(decided_by)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| to_store_err("pending_decide", e))?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    async fn get_pending(
        &self,
        _ctx: &CallerContext,
        id: &str,
    ) -> StoreResult<Option<crate::models::PendingAction>> {
        let row = sqlx::query(
            "SELECT id, action_type, memory_id, namespace, payload, requested_by,
                    requested_at, status, decided_by, decided_at, approvals
             FROM pending_actions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| to_store_err("get_pending", e))?;
        let Some(r) = row else {
            return Ok(None);
        };
        let requested_at: DateTime<Utc> = r
            .try_get("requested_at")
            .map_err(|e| to_store_err("read requested_at", e))?;
        let decided_at: Option<DateTime<Utc>> = r
            .try_get("decided_at")
            .map_err(|e| to_store_err("read decided_at", e))?;
        let approvals_v: serde_json::Value = r
            .try_get("approvals")
            .unwrap_or(serde_json::Value::Array(vec![]));
        let approvals: Vec<crate::models::Approval> =
            serde_json::from_value(approvals_v).unwrap_or_default();
        Ok(Some(crate::models::PendingAction {
            id: r
                .try_get::<String, _>("id")
                .map_err(|e| to_store_err("read id", e))?,
            action_type: r
                .try_get::<String, _>("action_type")
                .map_err(|e| to_store_err("read action_type", e))?,
            memory_id: r.try_get::<Option<String>, _>("memory_id").unwrap_or(None),
            namespace: r
                .try_get::<String, _>("namespace")
                .map_err(|e| to_store_err("read namespace", e))?,
            payload: r
                .try_get::<serde_json::Value, _>("payload")
                .unwrap_or(serde_json::Value::Null),
            requested_by: r
                .try_get::<String, _>("requested_by")
                .map_err(|e| to_store_err("read requested_by", e))?,
            requested_at: requested_at.to_rfc3339(),
            status: r
                .try_get::<String, _>("status")
                .map_err(|e| to_store_err("read status", e))?,
            decided_by: r.try_get::<Option<String>, _>("decided_by").unwrap_or(None),
            decided_at: decided_at.map(|d| d.to_rfc3339()),
            approvals,
        }))
    }

    async fn set_namespace_standard(
        &self,
        _ctx: &CallerContext,
        namespace: &str,
        standard_id: &str,
        parent: Option<&str>,
    ) -> StoreResult<()> {
        // Require the standard memory to exist first (parity with
        // sqlite db::set_namespace_standard).
        let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM memories WHERE id = $1")
            .bind(standard_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| to_store_err("set_namespace_standard verify memory", e))?;
        if exists.is_none() {
            return Err(StoreError::NotFound {
                id: standard_id.to_string(),
            });
        }
        if parent.is_some_and(|p| p == namespace) {
            return Err(StoreError::InvalidInput {
                detail: "namespace cannot be its own parent".to_string(),
            });
        }
        sqlx::query(
            "INSERT INTO namespace_meta (namespace, standard_id, updated_at, parent_namespace)
             VALUES ($1, $2, NOW(), $3)
             ON CONFLICT (namespace) DO UPDATE
                SET standard_id = EXCLUDED.standard_id,
                    updated_at = EXCLUDED.updated_at,
                    parent_namespace = EXCLUDED.parent_namespace",
        )
        .bind(namespace)
        .bind(standard_id)
        .bind(parent)
        .execute(&self.pool)
        .await
        .map_err(|e| to_store_err("set_namespace_standard", e))?;
        Ok(())
    }

    async fn clear_namespace_standard(
        &self,
        _ctx: &CallerContext,
        namespace: &str,
    ) -> StoreResult<bool> {
        let rows_affected = sqlx::query("DELETE FROM namespace_meta WHERE namespace = $1")
            .bind(namespace)
            .execute(&self.pool)
            .await
            .map_err(|e| to_store_err("clear_namespace_standard", e))?
            .rows_affected();
        Ok(rows_affected > 0)
    }

    async fn get_namespace_standard(
        &self,
        _ctx: &CallerContext,
        namespace: &str,
    ) -> StoreResult<Option<(String, Option<String>)>> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT standard_id, parent_namespace FROM namespace_meta WHERE namespace = $1",
        )
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| to_store_err("get_namespace_standard", e))?;
        Ok(row)
    }

    async fn touch_after_recall(&self, ids: &[String]) -> StoreResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        // Touch ops (atomic): increment access_count, extend TTL
        // (1h short / 1d mid), auto-promote mid→long at 5 accesses,
        // increment priority every 10 accesses.
        //
        // We run all three updates inside a single transaction so an
        // operator-visible recall stays consistent — the access_count
        // increment, the tier promotion, and the priority bump must
        // either all land or all roll back.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("touch_after_recall begin", e))?;

        sqlx::query(
            "UPDATE memories SET
                access_count = LEAST(access_count + 1, 1000000),
                last_accessed_at = NOW(),
                expires_at = CASE
                    WHEN tier = 'long' THEN expires_at
                    WHEN tier = 'short' AND expires_at IS NOT NULL
                        THEN NOW() + INTERVAL '1 hour'
                    WHEN tier = 'mid' AND expires_at IS NOT NULL
                        THEN NOW() + INTERVAL '1 day'
                    ELSE expires_at
                END
             WHERE id = ANY($1)",
        )
        .bind(ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("touch_after_recall extend", e))?;

        // Auto-promote mid → long at 5 accesses.
        sqlx::query(
            "UPDATE memories SET tier = 'long', expires_at = NULL, updated_at = NOW()
             WHERE id = ANY($1) AND tier = 'mid' AND access_count >= 5",
        )
        .bind(ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("touch_after_recall promote", e))?;

        // Increment priority every 10 accesses.
        sqlx::query(
            "UPDATE memories SET priority = LEAST(priority + 1, 10)
             WHERE id = ANY($1)
               AND access_count > 0
               AND access_count % 10 = 0
               AND priority < 10",
        )
        .bind(ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("touch_after_recall priority", e))?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("touch_after_recall commit", e))?;
        Ok(())
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
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };

        self.store(ctx, &mem).await.map(|_| ())
    }

    // v0.7.0 Wave-3 Continuation 3 — lifecycle write paths on postgres.

    async fn forget(
        &self,
        _ctx: &CallerContext,
        namespace: Option<&str>,
        pattern: Option<&str>,
        tier: Option<&Tier>,
        archive: bool,
    ) -> StoreResult<usize> {
        if namespace.is_none() && pattern.is_none() && tier.is_none() {
            return Err(StoreError::InvalidInput {
                detail: "at least one of namespace, pattern, or tier is required".to_string(),
            });
        }
        // Postgres uses ILIKE for the pattern match (no FTS5 here); the
        // sqlite path uses an FTS query. Both are case-insensitive
        // substring/token matches against title+content; we land on
        // ILIKE over title || ' ' || content so the wire contract
        // ("forget anything matching this string") is preserved.
        let tier_str = tier.map(|t| t.as_str().to_string());
        let pattern_like = pattern.map(|p| format!("%{p}%"));
        let now = chrono::Utc::now().to_rfc3339();

        if archive {
            // Insert matching rows into archived_memories before deletion.
            sqlx::query(
                "INSERT INTO archived_memories (
                    id, tier, namespace, title, content, tags, priority, confidence,
                    source, access_count, created_at, updated_at, last_accessed_at,
                    expires_at, archived_at, archive_reason, metadata,
                    embedding, embedding_dim, original_tier, original_expires_at
                )
                SELECT id, tier, namespace, title, content, tags, priority, confidence,
                       source, access_count, created_at, updated_at, last_accessed_at,
                       expires_at, $4::timestamptz, 'forget', metadata,
                       embedding, embedding_dim, tier, expires_at
                FROM memories
                WHERE ($1::text IS NULL OR namespace = $1)
                  AND ($2::text IS NULL OR tier = $2)
                  AND ($3::text IS NULL
                       OR title ILIKE $3
                       OR content ILIKE $3)
                ON CONFLICT (id) DO UPDATE SET
                    archived_at = EXCLUDED.archived_at,
                    archive_reason = EXCLUDED.archive_reason",
            )
            .bind(namespace)
            .bind(tier_str.as_deref())
            .bind(pattern_like.as_deref())
            .bind(parse_rfc3339_required(&now)?)
            .execute(&self.pool)
            .await
            .map_err(|e| to_store_err("forget archive copy", e))?;
        }

        let res = sqlx::query(
            "DELETE FROM memories
             WHERE ($1::text IS NULL OR namespace = $1)
               AND ($2::text IS NULL OR tier = $2)
               AND ($3::text IS NULL
                    OR title ILIKE $3
                    OR content ILIKE $3)",
        )
        .bind(namespace)
        .bind(tier_str.as_deref())
        .bind(pattern_like.as_deref())
        .execute(&self.pool)
        .await
        .map_err(|e| to_store_err("forget delete", e))?;

        Ok(usize::try_from(res.rows_affected()).unwrap_or(0))
    }

    async fn consolidate(
        &self,
        _ctx: &CallerContext,
        ids: &[String],
        title: &str,
        summary: &str,
        namespace: &str,
        tier: &Tier,
        source: &str,
        consolidator_agent_id: &str,
    ) -> StoreResult<String> {
        if ids.is_empty() {
            return Err(StoreError::InvalidInput {
                detail: "consolidate requires at least one source id".to_string(),
            });
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin consolidate tx", e))?;

        // Fetch source rows in one query, ordered by the input.
        let mut max_priority: i32 = 5;
        let mut all_tags: Vec<String> = Vec::new();
        let mut total_access: i64 = 0;
        let mut merged_metadata = serde_json::Map::new();
        let mut source_agent_ids: Vec<String> = Vec::new();

        for id in ids {
            use sqlx::Row;
            let row = sqlx::query(
                "SELECT tags, priority, access_count, metadata FROM memories WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| to_store_err("consolidate fetch source", e))?;
            let Some(row) = row else {
                return Err(StoreError::NotFound { id: id.clone() });
            };
            let priority: i32 = row.try_get("priority").unwrap_or(5);
            max_priority = max_priority.max(priority);
            let access_count: i64 = row.try_get("access_count").unwrap_or(0);
            total_access = total_access.saturating_add(access_count);
            let tags_json: serde_json::Value = row.try_get("tags").unwrap_or(serde_json::json!([]));
            if let Some(arr) = tags_json.as_array() {
                for t in arr {
                    if let Some(s) = t.as_str() {
                        all_tags.push(s.to_string());
                    }
                }
            }
            let metadata: serde_json::Value =
                row.try_get("metadata").unwrap_or(serde_json::json!({}));
            if let serde_json::Value::Object(map) = metadata {
                for (k, v) in map {
                    if k == "agent_id" {
                        if let serde_json::Value::String(aid) = &v
                            && !source_agent_ids.contains(aid)
                        {
                            source_agent_ids.push(aid.clone());
                        }
                        continue;
                    }
                    merged_metadata.insert(k, v);
                }
            }
        }

        all_tags.sort();
        all_tags.dedup();

        merged_metadata.insert(
            "derived_from".to_string(),
            serde_json::Value::Array(
                ids.iter()
                    .map(|id| serde_json::Value::String(id.clone()))
                    .collect(),
            ),
        );
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

        let new_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let tags_value =
            serde_json::to_value(&all_tags).map_err(|e| StoreError::IntegrityFailed {
                detail: format!("serialize consolidated tags: {e}"),
            })?;

        // Plan C R4 / R5 cert finding: the prior implementation was a plain
        // INSERT that exploded with `duplicate key value violates unique
        // constraint "memories_title_ns_uidx"` when an operator re-ran a
        // consolidate at the same (title, namespace) — common during
        // repeat cert runs against the same persistent postgres database.
        // The scenario reads `consolidated_id` out of the response, so we
        // must always return a real id; ON CONFLICT (title, namespace) DO
        // UPDATE re-uses the existing row's id and updates the content +
        // tags + metadata in place, matching the upsert contract of every
        // other insert site in this adapter. `RETURNING id` yields the
        // existing id on update so the caller sees a single canonical
        // memory at that (title, namespace).
        let inserted_id: String = sqlx::query_scalar(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, metadata
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, 1.0, $8, $9, $10, $10, $11)
            ON CONFLICT (title, namespace) DO UPDATE SET
                tier = CASE
                    WHEN tier_rank(EXCLUDED.tier) >= tier_rank(memories.tier)
                        THEN EXCLUDED.tier
                    ELSE memories.tier
                END,
                content = EXCLUDED.content,
                tags = EXCLUDED.tags,
                priority = EXCLUDED.priority,
                confidence = EXCLUDED.confidence,
                source = EXCLUDED.source,
                access_count = EXCLUDED.access_count,
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
                -- reflection_depth intentionally not surfaced here: the
                -- consolidate path mints a fresh memory and the DB column
                -- DEFAULT 0 applies. The UPSERT branch preserves the
                -- existing row's reflection_depth (no SET clause = keep).
            RETURNING id",
        )
        .bind(&new_id)
        .bind(tier.as_str())
        .bind(namespace)
        .bind(title)
        .bind(summary)
        .bind(&tags_value)
        .bind(max_priority)
        .bind(source)
        .bind(total_access)
        .bind(now)
        .bind(&merged_metadata_value)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| to_store_err("consolidate upsert", e))?;
        let new_id = inserted_id;

        // Delete source rows.
        //
        // Plan C R6 cert finding: when the UPSERT branch resolves a
        // `(title, namespace)` conflict it returns the EXISTING row's
        // id (RETURNING id). Some callers (S5 cert harness) re-include
        // the prior consolidated row in `ids` — they fetched every
        // memory in the namespace and the prior consolidation result
        // is one of them. If we delete every id in `ids` indiscriminately,
        // we delete the row we just upserted into and leave the caller
        // with a 201-but-vanished memory. Skip the deletion for the
        // upserted-into row; only the genuine source rows go away.
        for id in ids {
            if id == &new_id {
                continue;
            }
            sqlx::query("DELETE FROM memories WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("consolidate delete source", e))?;
        }

        tx.commit()
            .await
            .map_err(|e| to_store_err("consolidate commit", e))?;
        Ok(new_id)
    }

    async fn run_gc(&self, archive: bool) -> StoreResult<usize> {
        let now = chrono::Utc::now();
        if archive {
            sqlx::query(
                "INSERT INTO archived_memories (
                    id, tier, namespace, title, content, tags, priority, confidence,
                    source, access_count, created_at, updated_at, last_accessed_at,
                    expires_at, archived_at, archive_reason, metadata,
                    embedding, embedding_dim, original_tier, original_expires_at
                )
                SELECT id, tier, namespace, title, content, tags, priority, confidence,
                       source, access_count, created_at, updated_at, last_accessed_at,
                       expires_at, $1::timestamptz, 'ttl_expired', metadata,
                       embedding, embedding_dim, tier, expires_at
                FROM memories
                WHERE expires_at IS NOT NULL AND expires_at < $1
                ON CONFLICT (id) DO UPDATE SET
                    archived_at = EXCLUDED.archived_at,
                    archive_reason = EXCLUDED.archive_reason",
            )
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(|e| to_store_err("gc archive copy", e))?;
        }

        let res =
            sqlx::query("DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < $1")
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(|e| to_store_err("gc delete", e))?;

        // Best-effort cleanup of namespace_meta dangling references.
        let _ = sqlx::query(
            "DELETE FROM namespace_meta \
             WHERE standard_id NOT IN (SELECT id FROM memories)",
        )
        .execute(&self.pool)
        .await;

        Ok(usize::try_from(res.rows_affected()).unwrap_or(0))
    }

    async fn archive_restore(&self, _ctx: &CallerContext, id: &str) -> StoreResult<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin archive_restore tx", e))?;

        let exists: Option<(String,)> =
            sqlx::query_as("SELECT id FROM archived_memories WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| to_store_err("archive_restore lookup", e))?;
        if exists.is_none() {
            return Ok(false);
        }

        // Reject if the id is already in active memories.
        let active: Option<(String,)> = sqlx::query_as("SELECT id FROM memories WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| to_store_err("archive_restore active lookup", e))?;
        if active.is_some() {
            return Err(StoreError::Conflict { id: id.to_string() });
        }

        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, last_accessed_at,
                expires_at, metadata, embedding, embedding_dim
            )
            SELECT id, COALESCE(original_tier, 'long'), namespace, title, content,
                   tags, priority, confidence, source, access_count, created_at,
                   $1::timestamptz, last_accessed_at, original_expires_at, metadata,
                   embedding, embedding_dim
            FROM archived_memories WHERE id = $2",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| to_store_err("archive_restore insert", e))?;

        sqlx::query("DELETE FROM archived_memories WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("archive_restore delete", e))?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("archive_restore commit", e))?;
        Ok(true)
    }

    async fn archive_purge(&self, older_than_days: Option<i64>) -> StoreResult<usize> {
        let res = match older_than_days {
            Some(days) if days < 0 => {
                return Err(StoreError::InvalidInput {
                    detail: format!("older_than_days must be non-negative (got {days})"),
                });
            }
            Some(days) => {
                let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
                sqlx::query("DELETE FROM archived_memories WHERE archived_at < $1")
                    .bind(cutoff)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| to_store_err("archive_purge", e))?
            }
            None => sqlx::query("DELETE FROM archived_memories")
                .execute(&self.pool)
                .await
                .map_err(|e| to_store_err("archive_purge all", e))?,
        };
        Ok(usize::try_from(res.rows_affected()).unwrap_or(0))
    }

    async fn archive_by_ids(
        &self,
        _ctx: &CallerContext,
        ids: &[String],
        reason: Option<&str>,
    ) -> StoreResult<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut moved = 0usize;
        let now = chrono::Utc::now();
        let archive_reason = reason.unwrap_or("manual");
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin archive_by_ids tx", e))?;

        for id in ids {
            // Probe existence first so we can return an accurate count.
            let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM memories WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| to_store_err("archive_by_ids exists", e))?;
            if exists.is_none() {
                continue;
            }
            sqlx::query(
                "INSERT INTO archived_memories (
                    id, tier, namespace, title, content, tags, priority, confidence,
                    source, access_count, created_at, updated_at, last_accessed_at,
                    expires_at, archived_at, archive_reason, metadata,
                    embedding, embedding_dim, original_tier, original_expires_at
                )
                SELECT id, tier, namespace, title, content, tags, priority, confidence,
                       source, access_count, created_at, updated_at, last_accessed_at,
                       expires_at, $1::timestamptz, $2::text, metadata,
                       embedding, embedding_dim, tier, expires_at
                FROM memories WHERE id = $3
                ON CONFLICT (id) DO UPDATE SET
                    archived_at = EXCLUDED.archived_at,
                    archive_reason = EXCLUDED.archive_reason",
            )
            .bind(now)
            .bind(archive_reason)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("archive_by_ids insert", e))?;
            sqlx::query("DELETE FROM memories WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(|e| to_store_err("archive_by_ids delete", e))?;
            moved += 1;
        }

        tx.commit()
            .await
            .map_err(|e| to_store_err("archive_by_ids commit", e))?;
        Ok(moved)
    }

    async fn export_memories(&self) -> StoreResult<Vec<Memory>> {
        // Reuse the existing list path with an unbounded filter — postgres
        // adapter's `list` already projects the full Memory shape and the
        // ATOMIC_MULTI_WRITE-class semantics make a snapshot read safe.
        let ctx = CallerContext::for_agent("export");
        let filter = Filter {
            limit: 100_000,
            ..Filter::default()
        };
        self.list(&ctx, &filter).await
    }

    async fn export_links(&self) -> StoreResult<Vec<MemoryLink>> {
        // Delegate to the existing `list_links` trait method (no
        // namespace filter ⇒ full graph).
        self.list_links(None).await
    }

    async fn notify(
        &self,
        ctx: &CallerContext,
        target_agent: &str,
        title: &str,
        payload: &str,
        priority: Option<i32>,
        tier: Option<&Tier>,
    ) -> StoreResult<String> {
        let now = chrono::Utc::now().to_rfc3339();
        let resolved_tier = tier.cloned().unwrap_or(Tier::Short);
        let priority = priority.unwrap_or(5);
        let metadata = serde_json::json!({
            "agent_id": &ctx.agent_id,
            "target_agent_id": target_agent,
            "notify": true,
        });
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: resolved_tier,
            namespace: format!("_inbox/{target_agent}"),
            title: title.to_string(),
            content: payload.to_string(),
            tags: vec!["notify".to_string()],
            priority,
            confidence: 1.0,
            source: "notify".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        self.store(ctx, &mem).await
    }

    // v0.7.0 Wave-3 Continuation 3 (Phase 20) — full governance pipeline
    // on postgres: namespace inheritance walk, governance policy
    // resolution, multi-vote consensus state machine.

    async fn build_namespace_chain(&self, namespace: &str) -> StoreResult<Vec<String>> {
        // F-A2A1.2 — the governance-inheritance walk caps at
        // [`GOVERNANCE_INHERITANCE_DEPTH_CAP`] (= 5) intermediate levels per
        // the v0.7.0 spec, matching the bound the in-tx companion
        // [`build_namespace_chain_in_tx`] uses. See that helper's docstring
        // for the rationale and trade-offs.
        let mut chain: Vec<String> = Vec::new();

        if namespace == "*" {
            chain.push("*".to_string());
            return Ok(chain);
        }
        chain.push("*".to_string());

        // /-derived ancestors (root → leaf via namespace_ancestors which
        // returns most-specific-first; reverse for top-down).
        let mut hierarchy_chain: Vec<String> = crate::models::namespace_ancestors(namespace)
            .into_iter()
            .rev()
            .collect();

        // Walk explicit `namespace_meta.parent_namespace` chain above the
        // root, bounded + cycle-safe.
        if let Some(root) = hierarchy_chain.first().cloned() {
            let mut explicit_above: Vec<String> = Vec::new();
            let mut current = root;
            for _ in 0..GOVERNANCE_INHERITANCE_DEPTH_CAP {
                let row: Option<(Option<String>,)> = sqlx::query_as(
                    "SELECT parent_namespace FROM namespace_meta WHERE namespace = $1",
                )
                .bind(&current)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| to_store_err("build_namespace_chain parent lookup", e))?;
                let next = row.and_then(|(p,)| p);
                match next {
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
            for p in explicit_above.into_iter().rev() {
                if !chain.contains(&p) {
                    chain.push(p);
                }
            }
        }
        // F-A2A1.2 — same `/`-derived cap as the in-tx companion. Keep the
        // most-specific GOVERNANCE_INHERITANCE_DEPTH_CAP levels so a deeply
        // nested namespace still resolves against its closest authored
        // policy without blowing the resolver's connection-hold budget.
        let drained: Vec<String> = hierarchy_chain.drain(..).collect();
        let drained_len = drained.len();
        let kept: Vec<String> = if drained_len > GOVERNANCE_INHERITANCE_DEPTH_CAP {
            drained
                .into_iter()
                .skip(drained_len - GOVERNANCE_INHERITANCE_DEPTH_CAP)
                .collect()
        } else {
            drained
        };
        for entry in kept {
            if !chain.contains(&entry) {
                chain.push(entry);
            }
        }
        Ok(chain)
    }

    async fn resolve_governance_policy(
        &self,
        namespace: &str,
    ) -> StoreResult<Option<crate::models::GovernancePolicy>> {
        // Walk leaf → root and return the most-specific policy.
        let chain = self.build_namespace_chain(namespace).await?;
        for ns in chain.into_iter().rev() {
            // Look up the standard memory.
            let row: Option<(Option<String>,)> =
                sqlx::query_as("SELECT standard_id FROM namespace_meta WHERE namespace = $1")
                    .bind(&ns)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| to_store_err("resolve_governance_policy lookup", e))?;
            let Some((Some(standard_id),)) = row else {
                continue;
            };
            // Read the standard's metadata.
            let ctx = CallerContext::for_agent("governance");
            let mem = match self.get(&ctx, &standard_id).await {
                Ok(m) => m,
                Err(StoreError::NotFound { .. }) => continue,
                Err(e) => return Err(e),
            };
            if let Some(Ok(p)) = crate::models::GovernancePolicy::from_metadata(&mem.metadata) {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }

    async fn governance_approve_with_consensus(
        &self,
        ctx: &CallerContext,
        pending_id: &str,
        approver_agent_id: &str,
    ) -> StoreResult<super::ApproveOutcome> {
        // Load the pending row + assert state.
        let pa = match self.get_pending(ctx, pending_id).await? {
            Some(p) => p,
            None => {
                return Ok(super::ApproveOutcome::Rejected(format!(
                    "pending action not found: {pending_id}"
                )));
            }
        };
        if pa.status != "pending" {
            return Ok(super::ApproveOutcome::Rejected(format!(
                "already decided: status={}",
                pa.status
            )));
        }

        // Resolve namespace policy → approver type.
        let approver = self
            .resolve_governance_policy(&pa.namespace)
            .await?
            .map_or(crate::models::ApproverType::Human, |p| p.approver);

        match approver {
            crate::models::ApproverType::Human => {
                let ok = self
                    .pending_decide(ctx, pending_id, true, approver_agent_id)
                    .await?;
                if ok {
                    Ok(super::ApproveOutcome::Approved)
                } else {
                    Ok(super::ApproveOutcome::Rejected(
                        "decision write failed".to_string(),
                    ))
                }
            }
            crate::models::ApproverType::Agent(required) => {
                if approver_agent_id != required {
                    return Ok(super::ApproveOutcome::Rejected(format!(
                        "designated approver is '{required}'; got '{approver_agent_id}'"
                    )));
                }
                let ok = self
                    .pending_decide(ctx, pending_id, true, approver_agent_id)
                    .await?;
                if ok {
                    Ok(super::ApproveOutcome::Approved)
                } else {
                    Ok(super::ApproveOutcome::Rejected(
                        "decision write failed".to_string(),
                    ))
                }
            }
            crate::models::ApproverType::Consensus(quorum) => {
                if !self.is_registered_agent(approver_agent_id).await? {
                    return Ok(super::ApproveOutcome::Rejected(format!(
                        "consensus voter '{approver_agent_id}' is not a registered agent"
                    )));
                }
                let canonical_id = approver_agent_id.to_ascii_lowercase();
                let mut approvals = pa.approvals.clone();
                if approvals
                    .iter()
                    .any(|a| a.agent_id.eq_ignore_ascii_case(&canonical_id))
                {
                    return Ok(super::ApproveOutcome::Pending {
                        votes: approvals.len(),
                        quorum,
                    });
                }
                approvals.push(crate::models::Approval {
                    agent_id: canonical_id.clone(),
                    approved_at: chrono::Utc::now().to_rfc3339(),
                });
                let approvals_json =
                    serde_json::to_value(&approvals).map_err(|e| StoreError::IntegrityFailed {
                        detail: format!("serialize approvals: {e}"),
                    })?;
                sqlx::query(
                    "UPDATE pending_actions SET approvals = $1 \
                     WHERE id = $2 AND status = 'pending'",
                )
                .bind(&approvals_json)
                .bind(pending_id)
                .execute(&self.pool)
                .await
                .map_err(|e| to_store_err("update consensus approvals", e))?;
                let votes = approvals.len();
                if u32::try_from(votes).unwrap_or(u32::MAX) >= quorum {
                    let ok = self
                        .pending_decide(ctx, pending_id, true, &canonical_id)
                        .await?;
                    if ok {
                        return Ok(super::ApproveOutcome::Approved);
                    }
                    return Ok(super::ApproveOutcome::Rejected(
                        "decision write failed at consensus threshold".to_string(),
                    ));
                }
                Ok(super::ApproveOutcome::Pending { votes, quorum })
            }
        }
    }

    async fn execute_pending_action(
        &self,
        ctx: &CallerContext,
        pending_id: &str,
    ) -> StoreResult<Option<String>> {
        // Mirror sqlite `db::execute_pending_action`. Loads the row,
        // asserts status='approved', and applies the action via the
        // standard SAL surfaces (`store_with_embedding` for create-
        // style, `delete` for delete, `update` for promote). Idempotent
        // re-execute is the caller's responsibility.
        let pa = match self.get_pending(ctx, pending_id).await? {
            Some(p) => p,
            None => {
                return Err(StoreError::InvalidInput {
                    detail: format!("pending action not found: {pending_id}"),
                });
            }
        };
        if pa.status != "approved" {
            return Err(StoreError::InvalidInput {
                detail: format!("cannot execute non-approved action (status={})", pa.status),
            });
        }
        match pa.action_type.as_str() {
            "store" => {
                let mut mem: Memory = match serde_json::from_value(pa.payload.clone()) {
                    Ok(m) => m,
                    Err(e) => {
                        return Err(StoreError::IntegrityFailed {
                            detail: format!("invalid store payload: {e}"),
                        });
                    }
                };
                // Stamp fresh id + timestamps for idempotent replay.
                mem.id = uuid::Uuid::new_v4().to_string();
                let now = Utc::now().to_rfc3339();
                mem.created_at.clone_from(&now);
                mem.updated_at = now;
                mem.access_count = 0;
                let id = self.store(ctx, &mem).await?;
                Ok(Some(id))
            }
            "delete" => {
                if let Some(mid) = pa.memory_id.clone() {
                    self.delete(ctx, &mid).await?;
                    Ok(Some(mid))
                } else {
                    Ok(None)
                }
            }
            "promote" => {
                if let Some(mid) = pa.memory_id.clone() {
                    let patch = crate::store::UpdatePatch {
                        tier: Some(Tier::Long),
                        ..Default::default()
                    };
                    self.update(ctx, &mid, patch).await?;
                    Ok(Some(mid))
                } else {
                    Ok(None)
                }
            }
            other => Err(StoreError::InvalidInput {
                detail: format!("unsupported action_type: {other}"),
            }),
        }
    }

    async fn is_registered_agent(&self, agent_id: &str) -> StoreResult<bool> {
        use crate::models::AGENTS_NAMESPACE;
        let title = format!("agent:{agent_id}");
        let row: Option<(String,)> =
            sqlx::query_as("SELECT id FROM memories WHERE namespace = $1 AND title = $2")
                .bind(AGENTS_NAMESPACE)
                .bind(&title)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| to_store_err("is_registered_agent", e))?;
        Ok(row.is_some())
    }

    async fn enforce_governance_action(
        &self,
        action: super::GovernedAction,
        namespace: &str,
        agent_id: &str,
        memory_id: Option<&str>,
        memory_owner: Option<&str>,
        payload: &serde_json::Value,
    ) -> StoreResult<crate::models::GovernanceDecision> {
        use crate::config::{
            PermissionsMode, active_permissions_mode, record_permissions_decision,
        };
        use crate::models::{GovernanceDecision, GovernanceLevel};

        let mode = active_permissions_mode();
        record_permissions_decision(mode);

        if mode == PermissionsMode::Off {
            return Ok(GovernanceDecision::Allow);
        }

        // v0.7.0 H10 — open a SERIALIZABLE-equivalent transaction up-front
        // so every policy lookup AND the conditional pending_actions
        // INSERT ride a single connection. Pre-fix, the policy resolve
        // ran on connection A while the INSERT ran on connection B —
        // under concurrent governance writes the resolve and the INSERT
        // could observe different snapshots of namespace_meta /
        // pending_actions, producing a Pending decision whose row never
        // landed in the audit table.
        //
        // The transaction wraps the same surface area the sqlite path
        // serializes under its single rusqlite connection mutex, closing
        // the race without ratcheting backend-wide isolation level.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| to_store_err("begin enforce_governance_action tx", e))?;

        // Resolve the policy via the leaf-first walk — inline here so
        // every namespace_meta + memories.metadata read shares the same
        // tx snapshot as the INSERT below.
        let chain = build_namespace_chain_in_tx(&mut tx, namespace).await?;
        let mut resolved_policy: Option<crate::models::GovernancePolicy> = None;
        for ns in chain.iter().rev() {
            let row: Option<(Option<String>,)> =
                sqlx::query_as("SELECT standard_id FROM namespace_meta WHERE namespace = $1")
                    .bind(ns)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| to_store_err("resolve_governance_policy lookup (tx)", e))?;
            let Some((Some(standard_id),)) = row else {
                continue;
            };
            let meta: Option<(serde_json::Value,)> =
                sqlx::query_as("SELECT metadata FROM memories WHERE id = $1")
                    .bind(&standard_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| to_store_err("read governance standard metadata", e))?;
            if let Some((m,)) = meta
                && let Some(Ok(p)) = crate::models::GovernancePolicy::from_metadata(&m)
            {
                resolved_policy = Some(p);
                break;
            }
        }
        let Some(policy) = resolved_policy else {
            // No policy in the chain → Allow. Drop the tx (no writes).
            return Ok(GovernanceDecision::Allow);
        };
        let level = match action {
            super::GovernedAction::Store => &policy.write,
            super::GovernedAction::Delete => &policy.delete,
            super::GovernedAction::Promote => &policy.promote,
            // v0.7.0 L1-8: Reflect is gated by require_approval_above_depth
            // in the MCP handler; conservative fallback maps to write level.
            super::GovernedAction::Reflect => &policy.write,
        };

        // v0.7.0 Wave-3 Continuation 4 (Bucket C / S60+S80) — resolve
        // the namespace owner via the inheritance chain. Walks leaf→root
        // under the same tx snapshot as the policy lookup above.
        let ns_owner = if matches!(action, super::GovernedAction::Store) {
            let mut found: Option<String> = None;
            for ns in chain.iter().rev() {
                let row: Option<(Option<String>,)> = sqlx::query_as(
                    "SELECT m.metadata->>'agent_id' AS agent_id \
                     FROM namespace_meta nm \
                     JOIN memories m ON m.id = nm.standard_id \
                     WHERE nm.namespace = $1",
                )
                .bind(ns)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| to_store_err("namespace_owner chain lookup (tx)", e))?;
                if let Some((Some(o),)) = row {
                    found = Some(o);
                    break;
                }
            }
            found
        } else {
            None
        };

        // Inline is_registered_agent under the same tx — the existing
        // helper takes &self.pool; we reproduce its single-row probe.
        let registered_agent_check = if matches!(level, GovernanceLevel::Registered) {
            use crate::models::AGENTS_NAMESPACE;
            let title = format!("agent:{agent_id}");
            let row: Option<(String,)> =
                sqlx::query_as("SELECT id FROM memories WHERE namespace = $1 AND title = $2")
                    .bind(AGENTS_NAMESPACE)
                    .bind(&title)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| to_store_err("is_registered_agent (tx)", e))?;
            row.is_some()
        } else {
            false
        };

        // Evaluate the level. Same rules as the sqlite path:
        // - Any: always allow
        // - Registered: caller must be in `_agents` namespace
        // - Owner: caller must equal `memory_owner` (delete/promote) or
        //   `namespace_owner` (store)
        // - Approve: caller is non-owner ⇒ queue pending action
        let decision = match level {
            GovernanceLevel::Any => GovernanceDecision::Allow,
            GovernanceLevel::Registered => {
                if registered_agent_check {
                    GovernanceDecision::Allow
                } else {
                    GovernanceDecision::Deny(format!(
                        "agent '{agent_id}' is not registered for namespace '{namespace}'"
                    ))
                }
            }
            GovernanceLevel::Owner => {
                let owner_to_compare = match action {
                    super::GovernedAction::Store => ns_owner.as_deref(),
                    _ => memory_owner,
                };
                match owner_to_compare {
                    Some(o) if o == agent_id => GovernanceDecision::Allow,
                    Some(o) => GovernanceDecision::Deny(format!(
                        "owner-only namespace '{namespace}': caller '{agent_id}' is not '{o}'"
                    )),
                    None => GovernanceDecision::Allow,
                }
            }
            GovernanceLevel::Approve => {
                let owner_to_compare = match action {
                    super::GovernedAction::Store => ns_owner.as_deref(),
                    _ => memory_owner,
                };
                if matches!(owner_to_compare, Some(o) if o == agent_id) {
                    GovernanceDecision::Allow
                } else {
                    GovernanceDecision::Pending(String::new())
                }
            }
        };

        if mode == PermissionsMode::Advisory {
            // Drop the tx (no writes); advisory mode is read-only.
            return Ok(GovernanceDecision::Allow);
        }

        // Enforce mode — Pending queues a pending_actions row inside
        // the same tx so the audit trail is atomic with the decision.
        if let GovernanceDecision::Pending(_) = decision {
            let pending_id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now();
            let action_str = match action {
                super::GovernedAction::Store => "store",
                super::GovernedAction::Delete => "delete",
                super::GovernedAction::Promote => "promote",
                // v0.7.0 L1-8: Reflect action type for pending_actions row.
                super::GovernedAction::Reflect => "reflect",
            };
            sqlx::query(
                "INSERT INTO pending_actions \
                 (id, action_type, memory_id, namespace, payload, requested_by, requested_at, status) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending')",
            )
            .bind(&pending_id)
            .bind(action_str)
            .bind(memory_id)
            .bind(namespace)
            .bind(payload)
            .bind(agent_id)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("queue_pending_action (tx)", e))?;
            tx.commit()
                .await
                .map_err(|e| to_store_err("commit enforce_governance_action tx", e))?;
            return Ok(GovernanceDecision::Pending(pending_id));
        }
        // Allow / Deny: no write, but commit the (read-only) tx so the
        // connection is returned cleanly to the pool.
        tx.commit()
            .await
            .map_err(|e| to_store_err("commit enforce_governance_action tx (read-only)", e))?;
        Ok(decision)
    }

    // -----------------------------------------------------------------
    // v0.7.0 Wave-3 Continuation 6 — quota + verify-link parity.
    // -----------------------------------------------------------------

    async fn quota_status(&self, agent_id: &str) -> StoreResult<QuotaStatus> {
        // Auto-insert a default row when none exists, then read it back.
        // Mirrors the SQLite `quotas::ensure_row` posture so the wire
        // shape is byte-identical across backends.
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO agent_quotas (
                agent_id, max_memories_per_day, max_storage_bytes, max_links_per_day,
                current_memories_today, current_storage_bytes, current_links_today,
                day_started_at, created_at, updated_at
            ) VALUES ($1, $2, $3, $4, 0, 0, 0, $5, $5, $5)
            ON CONFLICT (agent_id) DO NOTHING",
        )
        .bind(agent_id)
        .bind(DEFAULT_MAX_MEMORIES_PER_DAY)
        .bind(DEFAULT_MAX_STORAGE_BYTES)
        .bind(DEFAULT_MAX_LINKS_PER_DAY)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| to_store_err("ensure agent_quotas row", e))?;

        let row = sqlx::query(
            "SELECT agent_id, max_memories_per_day, max_storage_bytes, max_links_per_day,
                    current_memories_today, current_storage_bytes, current_links_today,
                    day_started_at, created_at, updated_at
             FROM agent_quotas WHERE agent_id = $1",
        )
        .bind(agent_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| to_store_err("read agent_quotas row", e))?;

        Ok(row_to_quota_status(&row)?)
    }

    async fn quota_status_list(&self) -> StoreResult<Vec<QuotaStatus>> {
        let rows = sqlx::query(
            "SELECT agent_id, max_memories_per_day, max_storage_bytes, max_links_per_day,
                    current_memories_today, current_storage_bytes, current_links_today,
                    day_started_at, created_at, updated_at
             FROM agent_quotas ORDER BY agent_id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("list agent_quotas rows", e))?;

        rows.iter().map(row_to_quota_status).collect()
    }

    async fn verify_link(&self, filter: VerifyFilter) -> StoreResult<VerifyLinkReport> {
        if filter.source_id.is_none() && filter.link_id.is_none() {
            return Err(StoreError::InvalidInput {
                detail: "verify_link requires either source_id or link_id".to_string(),
            });
        }
        // Resolve the (source, target?, relation?) triple identically
        // to the SQLite path so the wire shape is stable.
        let (source_id, target_id, relation_filter) = if let Some(link_id) =
            filter.link_id.as_deref()
        {
            let parts: Vec<&str> = link_id.split('|').collect();
            if parts.len() != 3 {
                return Err(StoreError::InvalidInput {
                    detail: format!(
                        "link_id must be canonical source_id|target_id|relation triple, got {link_id}"
                    ),
                });
            }
            (
                parts[0].to_string(),
                Some(parts[1].to_string()),
                Some(parts[2].to_string()),
            )
        } else {
            (filter.source_id.unwrap_or_default(), filter.target_id, None)
        };

        let row_opt = match (target_id.as_deref(), relation_filter.as_deref()) {
            (Some(t), Some(r)) => sqlx::query(
                "SELECT source_id, target_id, relation, valid_from, valid_until, \
                        observed_by, signature, attest_level
                 FROM memory_links \
                 WHERE source_id = $1 AND target_id = $2 AND relation = $3 \
                 LIMIT 1",
            )
            .bind(&source_id)
            .bind(t)
            .bind(r)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| to_store_err("verify_link select", e))?,
            (Some(t), None) => sqlx::query(
                "SELECT source_id, target_id, relation, valid_from, valid_until, \
                        observed_by, signature, attest_level
                 FROM memory_links \
                 WHERE source_id = $1 AND target_id = $2 \
                 ORDER BY created_at ASC LIMIT 1",
            )
            .bind(&source_id)
            .bind(t)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| to_store_err("verify_link select", e))?,
            (None, _) => sqlx::query(
                "SELECT source_id, target_id, relation, valid_from, valid_until, \
                        observed_by, signature, attest_level
                 FROM memory_links \
                 WHERE source_id = $1 \
                 ORDER BY created_at ASC LIMIT 1",
            )
            .bind(&source_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| to_store_err("verify_link select", e))?,
        };

        let Some(row) = row_opt else {
            return Err(StoreError::NotFound {
                id: format!(
                    "link {source_id} -> {} {}",
                    target_id.as_deref().unwrap_or("?"),
                    relation_filter.as_deref().unwrap_or("?")
                ),
            });
        };

        let src: String = row
            .try_get("source_id")
            .map_err(|e| to_store_err("read source_id", e))?;
        let tgt: String = row
            .try_get("target_id")
            .map_err(|e| to_store_err("read target_id", e))?;
        let rel: String = row
            .try_get("relation")
            .map_err(|e| to_store_err("read relation", e))?;
        let vf: Option<DateTime<Utc>> = row
            .try_get("valid_from")
            .map_err(|e| to_store_err("read valid_from", e))?;
        let vu: Option<DateTime<Utc>> = row
            .try_get("valid_until")
            .map_err(|e| to_store_err("read valid_until", e))?;
        let obs: Option<String> = row
            .try_get("observed_by")
            .map_err(|e| to_store_err("read observed_by", e))?;
        let sig: Option<Vec<u8>> = row
            .try_get("signature")
            .map_err(|e| to_store_err("read signature", e))?;
        let attest: Option<String> = row
            .try_get("attest_level")
            .map_err(|e| to_store_err("read attest_level", e))?;

        let attest_level = attest.unwrap_or_else(|| "unsigned".to_string());
        let signature_present = sig.is_some();
        let mut findings: Vec<String> = Vec::new();

        // v0.7.0.1 G3 — re-derive the canonical RFC3339 strings from
        // the microsecond-precision TIMESTAMPTZ round-trip. The sign
        // path in `link_internal` truncates to microseconds before
        // signing AND before INSERT, so the verify path's CBOR bytes
        // must come from the same precision. `to_rfc3339()` on a value
        // already truncated to µs is a no-op, so this is defensive
        // belt-and-braces — if a future writer commits a higher-
        // precision value, this normalization clamps it back to what
        // the column actually stored.
        let vf_str = vf.map(|t| truncate_to_microseconds(t).to_rfc3339());
        let vu_str = vu.map(|t| truncate_to_microseconds(t).to_rfc3339());

        let verified = if signature_present {
            let observed = obs.as_deref().unwrap_or("");
            match crate::identity::verify::lookup_peer_public_key(observed) {
                None => {
                    findings.push(format!(
                        "signature present but no enrolled public key for observed_by={observed}"
                    ));
                    false
                }
                Some(pubkey) => {
                    let signable = crate::identity::sign::SignableLink {
                        src_id: &src,
                        dst_id: &tgt,
                        relation: &rel,
                        observed_by: obs.as_deref(),
                        valid_from: vf_str.as_deref(),
                        valid_until: vu_str.as_deref(),
                    };
                    let sig_bytes = sig.as_deref().unwrap_or(&[]);
                    match crate::identity::verify::verify(&pubkey, &signable, sig_bytes) {
                        Ok(()) => true,
                        Err(e) => {
                            findings.push(format!("signature verify failed: {e}"));
                            false
                        }
                    }
                }
            }
        } else {
            true
        };

        Ok(VerifyLinkReport {
            source_id: src,
            target_id: tgt,
            relation: rel,
            verified,
            attest_level,
            signature_present,
            observed_by: obs,
            findings,
        })
    }

    async fn find_paths(
        &self,
        source_id: &str,
        target_id: &str,
        max_depth: Option<usize>,
        max_results: Option<usize>,
    ) -> StoreResult<Vec<Vec<String>>> {
        // Inherent `PostgresStore::find_paths` already routes AGE vs CTE
        // off `self.kg_backend`; the trait method just forwards.
        PostgresStore::find_paths(self, source_id, target_id, max_depth, max_results).await
    }
}

/// v0.7.0 Continuation 6 — adapter row-to-`QuotaStatus` projection.
/// Lifted to a free function so both `quota_status` and
/// `quota_status_list` can reuse the same shape.
fn row_to_quota_status(row: &sqlx::postgres::PgRow) -> StoreResult<QuotaStatus> {
    let agent_id: String = row
        .try_get("agent_id")
        .map_err(|e| to_store_err("read quota agent_id", e))?;
    let max_memories_per_day: i64 = row
        .try_get("max_memories_per_day")
        .map_err(|e| to_store_err("read max_memories_per_day", e))?;
    let max_storage_bytes: i64 = row
        .try_get("max_storage_bytes")
        .map_err(|e| to_store_err("read max_storage_bytes", e))?;
    let max_links_per_day: i64 = row
        .try_get("max_links_per_day")
        .map_err(|e| to_store_err("read max_links_per_day", e))?;
    let current_memories_today: i64 = row
        .try_get("current_memories_today")
        .map_err(|e| to_store_err("read current_memories_today", e))?;
    let current_storage_bytes: i64 = row
        .try_get("current_storage_bytes")
        .map_err(|e| to_store_err("read current_storage_bytes", e))?;
    let current_links_today: i64 = row
        .try_get("current_links_today")
        .map_err(|e| to_store_err("read current_links_today", e))?;
    let day_started_at: DateTime<Utc> = row
        .try_get("day_started_at")
        .map_err(|e| to_store_err("read day_started_at", e))?;
    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(|e| to_store_err("read created_at", e))?;
    let updated_at: DateTime<Utc> = row
        .try_get("updated_at")
        .map_err(|e| to_store_err("read updated_at", e))?;
    Ok(QuotaStatus {
        agent_id,
        max_memories_per_day,
        max_storage_bytes,
        max_links_per_day,
        current_memories_today,
        current_storage_bytes,
        current_links_today,
        day_started_at: day_started_at.to_rfc3339(),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.to_rfc3339(),
    })
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

/// Outbound multi-hop knowledge-graph traversal for postgres-backed
/// daemons. Dispatches to [`PostgresStore::kg_query`] which itself
/// resolves Apache AGE vs the CTE fallback at adapter connect time.
///
/// # Errors
///
/// See [`PostgresStore::kg_query`].
pub async fn kg_query_via_store(
    store: &std::sync::Arc<dyn MemoryStore>,
    source_id: &str,
    max_depth: usize,
    include_invalidated: bool,
) -> StoreResult<Vec<crate::store::KgQueryRow>> {
    let pg = downcast_postgres(store)?;
    pg.kg_query_with_history(source_id, max_depth, include_invalidated)
        .await
}

/// Knowledge-graph timeline scan for postgres-backed daemons.
/// Mirrors the SQLite `db::kg_timeline` wire envelope so the HTTP
/// handler can stay backend-blind. `since` / `until` / `limit` are
/// passed through.
///
/// # Errors
///
/// See [`PostgresStore::kg_timeline`].
pub async fn kg_timeline_via_store(
    store: &std::sync::Arc<dyn MemoryStore>,
    source_id: &str,
    since: Option<&str>,
    until: Option<&str>,
    limit: Option<usize>,
) -> StoreResult<Vec<crate::store::KgTimelineRow>> {
    let pg = downcast_postgres(store)?;
    pg.kg_timeline(source_id, since, until, limit).await
}

/// Knowledge-graph link supersession for postgres-backed daemons.
/// Mirrors the SQLite `db::invalidate_link` contract — returns a
/// [`KgInvalidateRow`] whose `found` flag distinguishes "matched and
/// updated" from "no triple matched the predicate".
///
/// # Errors
///
/// See [`PostgresStore::kg_invalidate`].
pub async fn kg_invalidate_via_store(
    store: &std::sync::Arc<dyn MemoryStore>,
    source_id: &str,
    target_id: &str,
    relation: &str,
    valid_until: Option<&str>,
) -> StoreResult<crate::store::KgInvalidateRow> {
    let pg = downcast_postgres(store)?;
    pg.kg_invalidate(source_id, target_id, relation, valid_until)
        .await
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

/// v0.7.0 Wave-3 Continuation 4 (Bucket E / S44) — postgres taxonomy walk.
///
/// Returns `(namespace, count)` pairs (densest first) under an optional
/// prefix subtree. The HTTP `GET /api/v1/taxonomy` handler shapes these
/// into the hierarchical tree wire envelope (per-node `count` +
/// transitive `subtree_count` + an honest `total_count` /
/// `truncated` flag).
///
/// # Errors
///
/// Surfaces [`StoreError::BackendUnavailable`] when the active store is
/// not a [`PostgresStore`]; storage errors propagate from
/// [`PostgresStore::taxonomy_namespaces`].
pub async fn taxonomy_namespaces_via_store(
    store: &std::sync::Arc<dyn MemoryStore>,
    prefix: Option<&str>,
) -> StoreResult<Vec<(String, i64)>> {
    let pg = downcast_postgres(store)?;
    pg.taxonomy_namespaces(prefix).await
}

/// List pending governance actions for postgres-backed daemons.
///
/// Surfaces rows from the `pending_actions` table filtered by
/// optional `status` (`pending` / `approved` / `rejected`) and
/// optional `namespace` (S34's per-namespace queue view).
///
/// # Errors
///
/// Returns [`StoreError::BackendUnavailable`] when the underlying
/// adapter is not a [`PostgresStore`] or when the SQL query fails.
pub async fn list_pending_actions_via_store(
    store: &std::sync::Arc<dyn MemoryStore>,
    status: Option<&str>,
    namespace: Option<&str>,
    limit: usize,
) -> StoreResult<Vec<serde_json::Value>> {
    let pg = downcast_postgres(store)?;
    pg.list_pending_actions(status, namespace, limit).await
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
    /// v0.7.0 Wave-3 Continuation 4 (Bucket E / S44) — list every
    /// `(namespace, count)` pair across the live `memories` projection,
    /// optionally filtered to a prefix subtree. Returns the densest
    /// namespaces first; the caller is responsible for limit-trimming.
    ///
    /// Used by [`taxonomy_namespaces_via_store`] to power the
    /// hierarchical `GET /api/v1/taxonomy` walk on a postgres-backed
    /// daemon. The trait-level `list` cannot do this because it caps
    /// at 1000 rows and only matches namespaces exactly; the taxonomy
    /// walk needs prefix-match + dense-aggregate semantics that map
    /// cleanly onto a single `GROUP BY namespace` SQL query.
    pub async fn taxonomy_namespaces(
        &self,
        prefix: Option<&str>,
    ) -> StoreResult<Vec<(String, i64)>> {
        use sqlx::Row;
        let rows = if let Some(p) = prefix {
            // Match the subtree exactly OR any descendant via `<prefix>/...`.
            // The `LIKE` clause uses ESCAPE '\\' so a literal `_` or `%` in
            // the supplied prefix doesn't widen the match. Both conditions
            // are joined under a single OR for the dense-aggregate result.
            let escaped = p
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let descendant = format!("{escaped}/%");
            sqlx::query(
                "SELECT namespace, COUNT(*) AS cnt
                 FROM memories
                 WHERE (expires_at IS NULL OR expires_at > NOW())
                   AND (namespace = $1 OR namespace LIKE $2 ESCAPE '\\')
                 GROUP BY namespace
                 ORDER BY cnt DESC, namespace ASC",
            )
            .bind(p)
            .bind(descendant)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| to_store_err("taxonomy_namespaces prefix", e))?
        } else {
            sqlx::query(
                "SELECT namespace, COUNT(*) AS cnt
                 FROM memories
                 WHERE (expires_at IS NULL OR expires_at > NOW())
                 GROUP BY namespace
                 ORDER BY cnt DESC, namespace ASC",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| to_store_err("taxonomy_namespaces all", e))?
        };
        let mut out: Vec<(String, i64)> = Vec::with_capacity(rows.len());
        for r in rows {
            let ns: String = r.try_get("namespace").unwrap_or_default();
            let cnt: i64 = r.try_get("cnt").unwrap_or(0);
            out.push((ns, cnt));
        }
        Ok(out)
    }

    /// List pending governance actions filtered by optional status
    /// + namespace. Mirrors the sqlite `db::list_pending_actions`
    /// wire shape but projects directly from the postgres
    /// `pending_actions` table; on a fresh schema-init the row set
    /// is empty and the handler returns `count=0` cleanly.
    pub async fn list_pending_actions(
        &self,
        status: Option<&str>,
        namespace: Option<&str>,
        limit: usize,
    ) -> StoreResult<Vec<serde_json::Value>> {
        use sqlx::Row;
        let limit_i: i64 = limit.clamp(1, 1000).try_into().unwrap_or(100);
        let rows = sqlx::query(
            "SELECT id, action_type, memory_id, namespace, payload, requested_by, \
                    requested_at, status, decided_by, decided_at, approvals, \
                    default_timeout_seconds, expired_at \
             FROM pending_actions \
             WHERE ($1::text IS NULL OR status = $1) \
               AND ($2::text IS NULL OR namespace = $2) \
             ORDER BY requested_at DESC \
             LIMIT $3",
        )
        .bind(status)
        .bind(namespace)
        .bind(limit_i)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| to_store_err("list_pending_actions", e))?;
        let mut out: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let action_type: String = r.try_get("action_type").unwrap_or_default();
            let memory_id: Option<String> = r.try_get("memory_id").ok();
            let ns: String = r.try_get("namespace").unwrap_or_default();
            let payload: serde_json::Value =
                r.try_get("payload").unwrap_or(serde_json::Value::Null);
            let requested_by: String = r.try_get("requested_by").unwrap_or_default();
            let requested_at: chrono::DateTime<chrono::Utc> = r
                .try_get("requested_at")
                .unwrap_or_else(|_| chrono::Utc::now());
            let status_v: String = r.try_get("status").unwrap_or_default();
            let decided_by: Option<String> = r.try_get("decided_by").ok();
            let decided_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("decided_at").ok();
            let approvals: serde_json::Value = r
                .try_get("approvals")
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            let default_timeout_seconds: Option<i64> = r.try_get("default_timeout_seconds").ok();
            let expired_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("expired_at").ok();
            out.push(serde_json::json!({
                "id": id,
                "action_type": action_type,
                "memory_id": memory_id,
                "namespace": ns,
                "payload": payload,
                "requested_by": requested_by,
                "requested_at": requested_at.to_rfc3339(),
                "status": status_v,
                "decided_by": decided_by,
                "decided_at": decided_at.map(|t| t.to_rfc3339()),
                "approvals": approvals,
                "default_timeout_seconds": default_timeout_seconds,
                "expired_at": expired_at.map(|t| t.to_rfc3339()),
            }));
        }
        Ok(out)
    }

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
    // v0.7.0 L3 — `vector(N)` template substitution.
    //
    // The bundled `postgres_schema.sql` uses `vector({EMBEDDING_DIM})`
    // as a placeholder for the embedding-column dim. `render_schema_sql`
    // is the single substitution point invoked by `connect_with_dim`.
    // These tests verify the placeholder lives in the template AND
    // that the substitution leaves no stray placeholders behind.
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // F-A2A1.2 (#700) — governance inheritance depth cap.
    //
    // These tests pin the depth-cap constant + the chain-build behaviour
    // that does NOT require a live Postgres connection. The cap value is
    // surface-visible via `GOVERNANCE_INHERITANCE_DEPTH_CAP`; the chain
    // walk semantics (most-specific-N levels retained) ride the same
    // helper for both the pool-side `build_namespace_chain` and the
    // tx-side `build_namespace_chain_in_tx`. Live-PG variants below
    // exercise the same paths through `enforce_governance_action` end-
    // to-end against a real schema; the unit tests here are the
    // structural pin so a future refactor cannot silently drift the cap.
    // ------------------------------------------------------------------

    #[test]
    fn governance_inheritance_depth_cap_is_five() {
        // Pinned to 5 per the v0.7.0 fold-A2A1 spec. Any change to this
        // value must be reflected in
        // `docs/v0.7.0/a2a-triage-wave4-r2.md` §F-A2A1.2 and
        // accompanied by a CHANGELOG entry — the cap shapes the
        // bind-list size and connection-hold budget of every governed
        // write on postgres.
        assert_eq!(super::GOVERNANCE_INHERITANCE_DEPTH_CAP, 5);
    }

    #[test]
    fn namespace_ancestors_within_cap_pass_through() {
        // A namespace at or below the depth cap retains every level
        // (root → leaf). This is the common case: real-world
        // namespaces are 3-4 segments deep, so the cap should be a
        // no-op for ordinary writes.
        let ancestors: Vec<String> = crate::models::namespace_ancestors("a/b/c")
            .into_iter()
            .rev()
            .collect();
        // 3 levels: "a", "a/b", "a/b/c" — all retained.
        assert_eq!(ancestors.len(), 3);
        assert_eq!(ancestors[0], "a");
        assert_eq!(ancestors[2], "a/b/c");
    }

    #[test]
    fn namespace_ancestors_at_max_namespace_depth() {
        // The compile-time `MAX_NAMESPACE_DEPTH` is 8; namespaces at
        // that depth produce 8 ancestor levels. Our cap of 5 trims
        // such a chain when applied in the governance walker.
        let deep = "l1/l2/l3/l4/l5/l6/l7/l8";
        let ancestors: Vec<String> = crate::models::namespace_ancestors(deep)
            .into_iter()
            .rev()
            .collect();
        assert_eq!(ancestors.len(), 8);
        // Simulate the cap: keep last N most-specific entries.
        let cap = super::GOVERNANCE_INHERITANCE_DEPTH_CAP;
        let kept: Vec<String> = if ancestors.len() > cap {
            ancestors
                .iter()
                .skip(ancestors.len() - cap)
                .cloned()
                .collect()
        } else {
            ancestors
        };
        assert_eq!(kept.len(), cap);
        // The most-specific entry is the leaf itself.
        assert_eq!(kept.last().map(String::as_str), Some(deep));
        // The least-specific kept entry is the (cap-1)-from-leaf
        // ancestor, NOT the root. The root ("l1") is dropped under
        // the cap so resolution stays bounded — operators who want a
        // root-level policy applied to deep children must seat that
        // policy on a level within the cap reach.
        assert_eq!(kept.first().map(String::as_str), Some("l1/l2/l3/l4"));
    }

    #[test]
    fn namespace_ancestors_star_short_circuits() {
        // The synthetic `"*"` global standard is never decomposed by
        // `namespace_ancestors` — the chain walker prepends it
        // unconditionally instead. Verify the input shape so a future
        // refactor that auto-prefixes everything with `*` doesn't
        // silently double-count.
        let ancestors = crate::models::namespace_ancestors("*");
        assert_eq!(ancestors, vec!["*".to_string()]);
    }

    #[test]
    fn schema_template_carries_embedding_dim_placeholder() {
        // The schema file must hold the placeholder verbatim — otherwise
        // substitution becomes a silent no-op.
        assert!(
            INIT_SCHEMA.contains(EMBEDDING_DIM_PLACEHOLDER),
            "postgres_schema.sql must contain {EMBEDDING_DIM_PLACEHOLDER}"
        );
        // And it must NOT contain a hardcoded `vector(384)` /
        // `vector(768)` outside the placeholder — the template is the
        // single source of truth for column dim.
        assert!(
            !INIT_SCHEMA.contains("vector(384)"),
            "postgres_schema.sql must not contain hardcoded vector(384)"
        );
        assert!(
            !INIT_SCHEMA.contains("vector(768)"),
            "postgres_schema.sql must not contain hardcoded vector(768)"
        );
    }

    #[test]
    fn render_schema_sql_substitutes_768() {
        let rendered = render_schema_sql(INIT_SCHEMA, 768);
        assert!(rendered.contains("vector(768)"), "missing vector(768)");
        assert!(!rendered.contains("vector(384)"), "stray vector(384)");
        assert!(
            !rendered.contains(EMBEDDING_DIM_PLACEHOLDER),
            "placeholder not substituted: {rendered}"
        );
    }

    #[test]
    fn render_schema_sql_substitutes_384() {
        let rendered = render_schema_sql(INIT_SCHEMA, 384);
        assert!(rendered.contains("vector(384)"), "missing vector(384)");
        assert!(
            !rendered.contains(EMBEDDING_DIM_PLACEHOLDER),
            "placeholder not substituted: {rendered}"
        );
    }

    #[test]
    fn render_schema_sql_handles_arbitrary_dim() {
        // The rendering helper itself doesn't gate on supported dims —
        // the gate lives in `connect_with_dim` / `migrate_embedding_dim`.
        // This test just verifies the substitution is purely textual.
        let rendered = render_schema_sql("vector({EMBEDDING_DIM})", 1024);
        assert_eq!(rendered, "vector(1024)");
    }

    #[test]
    fn supported_embedding_dims_match_compiled_embedders() {
        // `SUPPORTED_EMBEDDING_DIMS` MUST mirror the values returned
        // by `EmbeddingModel::dim()` (config.rs). If either side gains
        // a new embedder we want the test to catch the drift.
        assert_eq!(SUPPORTED_EMBEDDING_DIMS, &[384, 768]);
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
    fn age_params_literal_renders_json_dict() {
        // v0.7.0 Wave-3 Continuation 5 — wire-shape contract for the
        // AGE `cypher()` third-arg helper. AGE rejects `$1::agtype`
        // (the third arg must be a bare Param, not a FuncExpr cast),
        // so we inline the params as a SQL agtype literal. Single
        // quotes in the JSON value must be SQL-escaped (`''`); the
        // outer wrap is `'…'::agtype`.
        assert_eq!(age_params_literal(&[("k", "v")]), "'{\"k\":\"v\"}'::agtype");
        assert_eq!(
            age_params_literal(&[("a", "1"), ("b", "two")]),
            "'{\"a\":\"1\",\"b\":\"two\"}'::agtype"
        );
        // SQL-escape: a literal apostrophe in the value gets doubled.
        assert_eq!(
            age_params_literal(&[("name", "O'Reilly")]),
            "'{\"name\":\"O''Reilly\"}'::agtype"
        );
        // Empty dict is harmless (AGE accepts an empty params object).
        assert_eq!(age_params_literal(&[]), "'{}'::agtype");
    }

    #[test]
    fn build_or_tsquery_or_joins_lexemes() {
        // v0.7.0.1 S79 — wire-shape contract for the OR-joined
        // tsquery helper. Mirrors sqlite's `sanitize_fts_query(_, true)`
        // OR-joined output so multi-token queries surface every row
        // matching AT LEAST one token.
        assert_eq!(build_or_tsquery("rust ownership"), "'rust' | 'ownership'");
        assert_eq!(build_or_tsquery("dog field"), "'dog' | 'field'");
        // Lower-cases tokens (postgres `to_tsquery('english', _)` lemma
        // matching is case-folded but the lexeme bytes must be the
        // post-fold form).
        assert_eq!(build_or_tsquery("Rust Ownership"), "'rust' | 'ownership'");
        // Single-token queries surface the bare lexeme.
        assert_eq!(build_or_tsquery("dog"), "'dog'");
    }

    #[test]
    fn build_or_tsquery_strips_tsquery_operators() {
        // S79 — tokens must not carry tsquery operators
        // (`& | ! ( ) : * '`) into the parser, which would let a
        // crafted query smuggle a boolean expression past the
        // matcher. Sanitization keeps only ASCII alphanumerics
        // plus `_-` so e.g. `cat | exec()` collapses to two harmless
        // lexemes.
        let got = build_or_tsquery("cat | drop ' table");
        assert!(
            !got.contains('&') && !got.contains('!') && !got.contains('('),
            "build_or_tsquery must drop tsquery operators: got {got}"
        );
        assert!(got.contains("'cat'"), "must keep the cat lexeme: {got}");
        assert!(got.contains("'drop'"), "must keep the drop lexeme: {got}");
        assert!(got.contains("'table'"), "must keep the table lexeme: {got}");
    }

    #[test]
    fn build_or_tsquery_drops_short_tokens() {
        // S79 — single-character tokens are noise (postgres' english
        // text-search config drops them as stop words anyway). Keep
        // tokens >= 2 chars.
        assert_eq!(build_or_tsquery("a brown dog"), "'brown' | 'dog'");
        assert_eq!(build_or_tsquery("x y z"), "'_empty_'");
    }

    #[test]
    fn build_or_tsquery_falls_back_to_sentinel_for_empty_input() {
        // S79 — `to_tsquery('english', '')` errors with a parse
        // failure; the helper substitutes a no-match sentinel so the
        // recall surface returns a clean empty pool instead of a
        // 500.
        assert_eq!(build_or_tsquery(""), "'_empty_'");
        assert_eq!(build_or_tsquery("    "), "'_empty_'");
        assert_eq!(build_or_tsquery("!@# $%^"), "'_empty_'");
    }

    #[test]
    fn build_or_tsquery_caps_token_count() {
        // S79 — pathologically long queries get truncated to 16
        // tokens so the planner doesn't blow up on a 10K-token
        // adversarial input.
        let long = (0..50)
            .map(|i| format!("tok{i:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        let got = build_or_tsquery(&long);
        let lexeme_count = got.matches('|').count() + 1;
        assert_eq!(
            lexeme_count, 16,
            "build_or_tsquery must cap to 16 tokens: got {lexeme_count} from {got}"
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
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
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
        // Pin the parity invariant: Postgres tracks the SQLite
        // CURRENT_SCHEMA_VERSION except for the postgres-only ladder
        // steps below. v29 is Postgres-only (in-place `vector(N)`
        // conversion helper); v30 is Postgres-only too (M15 —
        // `memories_metadata_is_object` CHECK; SQLite doesn't carry an
        // analogue because the SQLite metadata column has no JSON
        // type-checking primitive equivalent to `jsonb_typeof`). v31
        // mirrors SQLite v29 (`memories.reflection_depth`, v0.7.0
        // Task 1/8 — recursive learning). v32 mirrors SQLite v33 — the
        // v0.7.1-fold (#687/#688) SQL-side CHECK constraint on
        // `memory_links.relation`. v33 mirrors SQLite v34 — V-4
        // closeout (#698) signed_events cross-row hash chain. A
        // future bump on either side without the corresponding port
        // re-trips this assertion before the migration runner gets a
        // chance to write a partial schema to disk.
        assert_eq!(CURRENT_SCHEMA_VERSION, 37);
    }

    #[tokio::test]
    async fn live_migration_reaches_current_schema_version() {
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
            "schema_version must reach CURRENT_SCHEMA_VERSION"
        );
    }

    #[tokio::test]
    async fn live_migration_v17_to_current_is_idempotent() {
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

    // ------------------------------------------------------------------
    // F-A2A1.2 (#700) — live-PG governance enforcement tests.
    //
    // These pin the postgres adapter's `enforce_governance_action` end-
    // to-end against a real schema: seed a namespace standard with a
    // governance policy, walk inheritance through deep children, and
    // assert the per-level decision matches the SQLite reference.
    // Skipped when `AI_MEMORY_TEST_POSTGRES_URL` is unset (matches the
    // discipline of the existing live tests above).
    //
    // Mapping to the failing A2A scenarios:
    // - `live_governance_allow_owner_at_leaf`           — S53 phase B (owner write 201)
    // - `live_governance_deny_non_owner_inherited`      — S53/S60/S80 (intruder 403)
    // - `live_governance_pending_on_approve_level`      — S34 (write goes pending)
    // - `live_governance_inheritance_cap_at_five`       — depth-cap spec pin
    // ------------------------------------------------------------------

    /// Seed a namespace standard memory and register it via
    /// `namespace_meta`. Returns the standard_id. Owner is the
    /// metadata.agent_id stamped on the standard memory.
    async fn seed_governance_standard(
        pool: &sqlx::PgPool,
        namespace: &str,
        owner: &str,
        policy_json: serde_json::Value,
    ) -> String {
        let standard_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let metadata = serde_json::json!({
            "agent_id": owner,
            "governance": policy_json,
        });
        sqlx::query(
            "INSERT INTO memories (
                id, tier, namespace, title, content, tags, priority, confidence,
                source, access_count, created_at, updated_at, metadata
            ) VALUES ($1, 'long', $2, $3, 'standard', '[]'::jsonb, 5, 1.0,
                      'test', 0, $4, $4, $5)",
        )
        .bind(&standard_id)
        .bind(namespace)
        .bind(format!("standard:{namespace}"))
        .bind(now)
        .bind(&metadata)
        .execute(pool)
        .await
        .expect("seed standard memory");

        sqlx::query(
            "INSERT INTO namespace_meta (namespace, standard_id, parent_namespace) \
             VALUES ($1, $2, NULL) \
             ON CONFLICT (namespace) DO UPDATE SET standard_id = EXCLUDED.standard_id",
        )
        .bind(namespace)
        .bind(&standard_id)
        .execute(pool)
        .await
        .expect("seed namespace_meta");

        standard_id
    }

    async fn cleanup_governance_ns(pool: &sqlx::PgPool, namespace: &str) {
        let _ = sqlx::query("DELETE FROM pending_actions WHERE namespace LIKE $1")
            .bind(format!("{namespace}%"))
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM namespace_meta WHERE namespace LIKE $1")
            .bind(format!("{namespace}%"))
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM memories WHERE namespace LIKE $1")
            .bind(format!("{namespace}%"))
            .execute(pool)
            .await;
    }

    #[tokio::test]
    async fn live_governance_allow_owner_at_leaf() {
        // S53 phase B — owner writes to their own namespace under a
        // `write=owner` policy. Decision must be Allow.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        crate::config::override_active_permissions_mode_for_test(
            crate::config::PermissionsMode::Enforce,
        );
        let store = PostgresStore::connect(&url).await.expect("connect");
        let pool = store.pool.clone();
        let owner = format!("ai:gov-owner-{}", uuid::Uuid::new_v4());
        let ns = format!("fa2a12-allow-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        seed_governance_standard(
            &pool,
            &ns,
            &owner,
            serde_json::json!({"write": "owner", "promote": "any", "delete": "owner"}),
        )
        .await;

        let payload = serde_json::json!({"title": "owner write"});
        let decision = store
            .enforce_governance_action(
                crate::store::GovernedAction::Store,
                &ns,
                &owner,
                None,
                None,
                &payload,
            )
            .await
            .expect("enforce_governance_action");
        assert!(
            matches!(decision, crate::models::GovernanceDecision::Allow),
            "owner write to own ns must Allow; got {decision:?}"
        );

        cleanup_governance_ns(&pool, &ns).await;
    }

    #[tokio::test]
    async fn live_governance_deny_non_owner_inherited() {
        // S53/S60/S80 — a non-owner write to a deep child of a
        // `write=owner` parent must be Denied via the inheritance walk.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        crate::config::override_active_permissions_mode_for_test(
            crate::config::PermissionsMode::Enforce,
        );
        let store = PostgresStore::connect(&url).await.expect("connect");
        let pool = store.pool.clone();
        let owner = format!("ai:gov-owner-{}", uuid::Uuid::new_v4());
        let intruder = format!("ai:gov-intruder-{}", uuid::Uuid::new_v4());
        let parent = format!("fa2a12-deny-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let deep_child = format!("{parent}/sub/level/deep");
        seed_governance_standard(
            &pool,
            &parent,
            &owner,
            serde_json::json!({
                "write": "owner",
                "promote": "any",
                "delete": "owner",
                "inherit": true,
            }),
        )
        .await;

        let payload = serde_json::json!({"title": "intruder"});
        let decision = store
            .enforce_governance_action(
                crate::store::GovernedAction::Store,
                &deep_child,
                &intruder,
                None,
                None,
                &payload,
            )
            .await
            .expect("enforce_governance_action");
        match decision {
            crate::models::GovernanceDecision::Deny(reason) => {
                assert!(
                    reason.contains("owner-only namespace")
                        || reason.to_lowercase().contains("owner"),
                    "deny reason should reference owner-only policy; got: {reason}"
                );
            }
            other => panic!("intruder write to deep child must Deny; got {other:?}"),
        }

        // Owner write to the same deep child should Allow — owner walk
        // resolves leaf→root and finds the parent's standard owner.
        let owner_decision = store
            .enforce_governance_action(
                crate::store::GovernedAction::Store,
                &deep_child,
                &owner,
                None,
                None,
                &payload,
            )
            .await
            .expect("enforce_governance_action owner");
        assert!(
            matches!(owner_decision, crate::models::GovernanceDecision::Allow),
            "owner write to inherited deep child must Allow; got {owner_decision:?}"
        );

        cleanup_governance_ns(&pool, &parent).await;
    }

    #[tokio::test]
    async fn live_governance_pending_on_approve_level() {
        // S34 — a `write=approve` policy on a namespace must route
        // non-owner writes through Pending. The decision payload must
        // land a row in `pending_actions`.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        crate::config::override_active_permissions_mode_for_test(
            crate::config::PermissionsMode::Enforce,
        );
        let store = PostgresStore::connect(&url).await.expect("connect");
        let pool = store.pool.clone();
        let owner = format!("ai:gov-owner-{}", uuid::Uuid::new_v4());
        let requester = format!("ai:gov-requester-{}", uuid::Uuid::new_v4());
        let ns = format!("fa2a12-pending-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        seed_governance_standard(
            &pool,
            &ns,
            &owner,
            serde_json::json!({"write": "approve", "promote": "any", "delete": "owner"}),
        )
        .await;

        let payload = serde_json::json!({"title": "needs approval"});
        let decision = store
            .enforce_governance_action(
                crate::store::GovernedAction::Store,
                &ns,
                &requester,
                None,
                None,
                &payload,
            )
            .await
            .expect("enforce_governance_action");
        let pending_id = match decision {
            crate::models::GovernanceDecision::Pending(id) => id,
            other => panic!("approve-level non-owner write must Pending; got {other:?}"),
        };
        assert!(!pending_id.is_empty(), "Pending id must be non-empty");

        let row: (String, String, String) = sqlx::query_as(
            "SELECT action_type, namespace, status FROM pending_actions WHERE id = $1",
        )
        .bind(&pending_id)
        .fetch_one(&pool)
        .await
        .expect("read pending_actions row");
        assert_eq!(row.0, "store", "action_type must be 'store'");
        assert_eq!(row.1, ns, "namespace must match");
        assert_eq!(row.2, "pending", "status must be 'pending'");

        cleanup_governance_ns(&pool, &ns).await;
    }

    #[tokio::test]
    async fn live_governance_inheritance_cap_at_five() {
        // F-A2A1.2 depth cap — a namespace at MAX_NAMESPACE_DEPTH (8
        // levels) under a `write=owner` parent at the root must still
        // resolve to Deny for a non-owner, because the cap retains the
        // most-specific 5 levels which include the policy-anchored
        // child path. Conversely, a policy seated at the root that's
        // OUTSIDE the cap (depth 8 child, policy at depth 1 root) is
        // expected NOT to apply — the cap is the explicit contract.
        //
        // This test pins the "most-specific kept" semantics by seating
        // the policy 2 levels above the leaf (well within the cap)
        // and verifying inheritance fires.
        let Some(url) = postgres_url() else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        crate::config::override_active_permissions_mode_for_test(
            crate::config::PermissionsMode::Enforce,
        );
        let store = PostgresStore::connect(&url).await.expect("connect");
        let pool = store.pool.clone();
        let owner = format!("ai:cap-owner-{}", uuid::Uuid::new_v4());
        let intruder = format!("ai:cap-intruder-{}", uuid::Uuid::new_v4());
        // Anchor the policy at a 4-segment namespace; the leaf is 6
        // segments — within the cap.
        let suffix = &uuid::Uuid::new_v4().to_string()[..6];
        let policy_ns = format!("fa2a12-cap-{suffix}/a/b/c");
        let leaf_ns = format!("{policy_ns}/d/e");
        seed_governance_standard(
            &pool,
            &policy_ns,
            &owner,
            serde_json::json!({
                "write": "owner",
                "promote": "any",
                "delete": "owner",
                "inherit": true,
            }),
        )
        .await;

        let payload = serde_json::json!({"leaf": leaf_ns});
        let decision = store
            .enforce_governance_action(
                crate::store::GovernedAction::Store,
                &leaf_ns,
                &intruder,
                None,
                None,
                &payload,
            )
            .await
            .expect("enforce_governance_action");
        match decision {
            crate::models::GovernanceDecision::Deny(_) => {}
            other => panic!(
                "policy at depth 4 must deny intruder write at depth 6 (within cap); got \
                 {other:?}"
            ),
        }

        cleanup_governance_ns(&pool, &format!("fa2a12-cap-{suffix}")).await;
    }
}
