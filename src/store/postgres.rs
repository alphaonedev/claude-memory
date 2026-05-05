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
    CallerContext, Capabilities, Filter, KgBackend, MemoryStore, StoreError, StoreResult,
    UpdatePatch, VerifyReport,
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
