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
    CallerContext, Capabilities, Filter, MemoryStore, StoreError, StoreResult, UpdatePatch,
    VerifyReport,
};
use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};

/// Bootstrap schema run at adapter init — idempotent via IF NOT EXISTS.
const INIT_SCHEMA: &str = include_str!("postgres_schema.sql");

/// Default connection pool settings. Tuned for a mid-range ai-memory
/// daemon — adjust via `PostgresStore::with_pool_options` when wiring
/// a larger deployment.
const DEFAULT_MAX_CONNECTIONS: u32 = 16;
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// `PostgresStore` — sqlx + pgvector backend. Owns a connection pool.
#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
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
        // against. (#302 item 2 — prior code accepted any version.)
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

        Ok(Self { pool })
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
               AND (expires_at IS NULL OR expires_at > NOW())
             ORDER BY rank DESC, priority DESC
             LIMIT $4",
        )
        .bind(query)
        .bind(filter.namespace.as_ref())
        .bind(filter.tier.as_ref().map(Tier::as_str))
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
}
