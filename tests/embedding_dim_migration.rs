// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #877 (HIGH-SEV) — embedding-dim auto-migrate regression.
//!
//! Plan-C re-test surfaced that a fresh container schema bootstraps
//! `memories.embedding` at `vector(384)` (the `DEFAULT_EMBEDDING_DIM`),
//! while an autonomous-tier daemon loads `nomic-embed-text-v1.5` (768).
//! Every HTTP POST `/api/v1/memories` then failed with `expected 384
//! dimensions, not 768` at the pgvector layer.
//!
//! Fix: the postgres adapter exposes
//! `connect_with_dim_and_timeout_auto_migrate(url, dim, secs)` — a new
//! daemon-bootstrap entry point that detects the dim mismatch and runs
//! the destructive `migrate_embedding_dim` in-place. The daemon
//! `bootstrap_serve` path resolves the configured embedder dim from the
//! same ladder `build_embedder` uses (`app_config` override > tier preset)
//! and threads it through `build_store_handle` → the new auto-migrate
//! entry point so a misaligned-dim schema is healed before the first
//! write hits the wire.
//!
//! # Gating
//!
//! Requires `feature = "sal-postgres"` + `AI_MEMORY_TEST_POSTGRES_URL`
//! pointing at a fresh, disposable database. Without either the test
//! `eprintln!`s a skip and returns Ok — matches the rest of the
//! postgres integration suite.
//!
//! # What the test pins
//!
//! 1. Bootstrap at dim=384, sanity-check the column declared at 384.
//! 2. Re-open with the auto-migrate entry point at dim=768 — verify the
//!    column flipped to `vector(768)`.
//! 3. Idempotence: a second auto-migrate call at dim=768 is a no-op.
//! 4. End-to-end write-path regression: after the auto-migrate, the
//!    postgres adapter accepts a 768-dim embedding insert end-to-end
//!    (the actual failure mode the Plan-C retest hit).

#![cfg(feature = "sal-postgres")]

use ai_memory::store::postgres::PostgresStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

mod common;
use common::postgres_url;

/// Drop ai-memory tables so each test gets a fresh schema. The postgres
/// fixture is shared across tests in the suite, so we tear down the
/// adapter-owned tables (CREATE TABLE IF NOT EXISTS is the bootstrap
/// idiom, so dropping = full reset on the next connect). We DO NOT
/// drop the `vector` extension itself — that's a database-level install
/// that other tests in the binary may still need.
async fn reset_schema(pool: &PgPool) {
    let stmts = [
        "DROP TABLE IF EXISTS archived_memories CASCADE",
        "DROP TABLE IF EXISTS memory_links CASCADE",
        "DROP TABLE IF EXISTS memories CASCADE",
        "DROP TABLE IF EXISTS namespace_meta CASCADE",
        "DROP TABLE IF EXISTS pending_actions CASCADE",
        "DROP TABLE IF EXISTS sync_state CASCADE",
        "DROP TABLE IF EXISTS subscriptions CASCADE",
        "DROP TABLE IF EXISTS subscription_events CASCADE",
        "DROP TABLE IF EXISTS subscription_dlq CASCADE",
        "DROP TABLE IF EXISTS signed_events CASCADE",
        "DROP TABLE IF EXISTS audit_log CASCADE",
        "DROP TABLE IF EXISTS entity_aliases CASCADE",
        "DROP TABLE IF EXISTS memory_transcripts CASCADE",
        "DROP TABLE IF EXISTS memory_transcript_links CASCADE",
        "DROP TABLE IF EXISTS agent_quotas CASCADE",
        "DROP TABLE IF EXISTS schema_version CASCADE",
        "DROP VIEW IF EXISTS kg_query_view CASCADE",
        "DROP VIEW IF EXISTS kg_timeline_view CASCADE",
    ];
    for sql in stmts {
        let _ = sqlx::query(sql).execute(pool).await;
    }
}

async fn inspection_pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await
        .expect("inspection pool connect")
}

async fn current_dim(pool: &PgPool) -> Option<i32> {
    sqlx::query_scalar::<_, i32>(
        "SELECT atttypmod FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         WHERE c.relname = 'memories' AND a.attname = 'embedding'",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Core regression: bootstrap at 384, then re-open with the
/// auto-migrate entry point at 768 — the column flips in-place.
#[tokio::test]
async fn auto_migrate_converts_384_schema_to_768_on_daemon_bootstrap() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let inspect = inspection_pool(&url).await;
    reset_schema(&inspect).await;

    // Step 1: bootstrap the schema at the legacy MiniLM dim (384). This
    // mirrors a fresh container that booted before the operator
    // configured an embedder, or that booted with the default
    // `MiniLmL6V2` preset.
    {
        let _store = PostgresStore::connect_with_dim(&url, 384)
            .await
            .expect("connect at dim=384");
    }
    assert_eq!(
        current_dim(&inspect).await,
        Some(384),
        "step-1: fresh bootstrap must land vector(384)"
    );

    // Step 2: re-open via the auto-migrate entry point at dim=768. This
    // is what the daemon does at `bootstrap_serve` time when the
    // configured tier is `autonomous` / `smart` (= NomicEmbedV15, 768).
    {
        let _store = PostgresStore::connect_with_dim_and_timeout_auto_migrate(&url, 768, 30)
            .await
            .expect("auto-migrate to dim=768");
    }
    assert_eq!(
        current_dim(&inspect).await,
        Some(768),
        "step-2: auto-migrate must convert the column to vector(768) in place"
    );

    // Step 3: idempotence — a second auto-migrate at 768 is a no-op
    // and leaves the column unchanged.
    {
        let _store = PostgresStore::connect_with_dim_and_timeout_auto_migrate(&url, 768, 30)
            .await
            .expect("idempotent auto-migrate");
    }
    assert_eq!(
        current_dim(&inspect).await,
        Some(768),
        "step-3: re-opening at the matching dim must be a no-op"
    );
}

/// Direct bootstrap at 768 via the auto-migrate entry point: the
/// fresh schema lands `vector(768)` so the no-op-after-bootstrap path
/// also passes (we don't need to do the conversion at all).
#[tokio::test]
async fn auto_migrate_no_op_when_fresh_schema_already_matches() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let inspect = inspection_pool(&url).await;
    reset_schema(&inspect).await;

    {
        let _store = PostgresStore::connect_with_dim_and_timeout_auto_migrate(&url, 768, 30)
            .await
            .expect("fresh bootstrap at dim=768 via auto-migrate path");
    }
    assert_eq!(
        current_dim(&inspect).await,
        Some(768),
        "fresh-bootstrap path must land vector(768) directly without a destructive migrate"
    );
}

/// HTTP-write-path regression closeout: after the auto-migrate runs,
/// the postgres adapter accepts a 768-dim embedding insert end-to-end.
/// This is the actual failure mode the Plan-C container retest hit
/// (`expected 384 dimensions, not 768`); the test pins the recovery.
#[tokio::test]
async fn http_write_path_accepts_768_after_auto_migrate() {
    use ai_memory::models::Memory;
    use ai_memory::store::{CallerContext, MemoryStore};
    use chrono::Utc;

    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let inspect = inspection_pool(&url).await;
    reset_schema(&inspect).await;

    // 1) Bootstrap the schema at 384 (the bug-trigger state).
    let _ = PostgresStore::connect_with_dim(&url, 384)
        .await
        .expect("seed bootstrap");

    // 2) Re-open with auto-migrate at 768 (simulates the fixed daemon
    //    bootstrap path with an autonomous-tier embedder).
    let store = PostgresStore::connect_with_dim_and_timeout_auto_migrate(&url, 768, 30)
        .await
        .expect("auto-migrate at bootstrap");

    // 3) Insert a memory with a 768-dim embedding via the SAL surface
    //    the HTTP create_memory handler uses. `store_with_embedding`
    //    is the postgres-aware fork — bypassing it (plain `store`)
    //    would never bind the vector and the test would degenerate to
    //    a schema-only check.
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: "issue-877-retest".to_string(),
        namespace: "ai-memory-mcp".to_string(),
        title: "issue #877 retest".to_string(),
        content: "auto-migrate must let a 768-dim insert succeed".to_string(),
        tags: vec!["issue-877".to_string()],
        source: "test".to_string(),
        created_at: now.clone(),
        updated_at: now,
        metadata: serde_json::json!({"agent_id":"issue-877-test"}),
        ..Default::default()
    };

    let ctx = CallerContext::for_agent("issue-877-test");
    let embedding: Vec<f32> = vec![0.0_f32; 768];
    store
        .store_with_embedding(&ctx, &mem, Some(&embedding))
        .await
        .expect("768-dim insert must succeed after auto-migrate");

    // 4) Read-back sanity: row landed.
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM memories WHERE id = 'issue-877-retest'")
            .fetch_optional(&inspect)
            .await
            .expect("inspect row");
    assert!(
        row.is_some(),
        "issue-877 row must persist through the postgres write path post-auto-migrate"
    );
}
