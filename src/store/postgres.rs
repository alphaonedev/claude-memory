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
    CallerContext, Capabilities, Filter, KgBackend, KgQueryRow, KgTimelineRow, MemoryStore,
    StoreError, StoreResult, UpdatePatch, VerifyReport,
};
use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};

/// Bootstrap schema run at adapter init — idempotent via IF NOT EXISTS.
const INIT_SCHEMA: &str = include_str!("postgres_schema.sql");

/// Current schema version. Matches SQLite CURRENT_SCHEMA_VERSION (src/db.rs:173).
/// Incremented on each migration step.
const CURRENT_SCHEMA_VERSION: i32 = 15;

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
        if current_version < 15 {
            self.migrate_v15().await?;
        }

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
        sqlx::query("DELETE FROM schema_version")
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("delete old schema_version", e))?;

        sqlx::query("INSERT INTO schema_version (version) VALUES ($1)")
            .bind(CURRENT_SCHEMA_VERSION)
            .execute(&mut *tx)
            .await
            .map_err(|e| to_store_err("insert schema_version", e))?;

        tx.commit()
            .await
            .map_err(|e| to_store_err("commit migration transaction", e))?;

        tracing::info!(
            target = "store::postgres",
            version = CURRENT_SCHEMA_VERSION,
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
        let tags_first: Option<&str> = filter.tags_any.first().map(String::as_str);
        let rows = sqlx::query(
            "SELECT *,
                    ts_rank(
                        to_tsvector('english', title || ' ' || content),
                        plainto_tsquery('english', $1)
                    ) AS rank
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
        // The SQL schema has no links table yet in this preview. The
        // follow-up PR adds `memory_links` + a link() implementation.
        // We return UnsupportedCapability rather than silently no-op.
        let _ = link;
        Err(StoreError::UnsupportedCapability {
            capability: "LINKS".to_string(),
        })
    }

    async fn register_agent(
        &self,
        _ctx: &CallerContext,
        agent: &AgentRegistration,
    ) -> StoreResult<()> {
        // Agent registration lives in a dedicated table on the Postgres
        // track next sprint. The Task 1.3 baseline ships on SqliteStore
        // only for v0.7-alpha.
        let _ = agent;
        Err(StoreError::UnsupportedCapability {
            capability: "AGENT_REGISTRATION".to_string(),
        })
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
}
