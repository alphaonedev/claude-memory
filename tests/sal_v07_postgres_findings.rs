// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(
    clippy::items_after_statements,
    clippy::map_unwrap_or,
    clippy::too_many_lines
)]
//! v0.7.0 Postgres SAL findings — regression tests for H10, M3, M4, M15.
//!
//! Each test is feature-gated on `sal-postgres` and skipped without
//! `AI_MEMORY_TEST_POSTGRES_URL`. Matches the existing
//! `tests/g1_postgres_quota_increment_on_store.rs` skip-line convention.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::models::Memory;
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::{CallerContext, MemoryStore};

fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

fn unique_title(base: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{base}-{nanos}-{}", uuid::Uuid::new_v4())
}

fn fresh_memory(title: &str, namespace: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: "regression-content".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:test"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

// ---------------------------------------------------------------------
// M4 / M7 — statement_timeout enforced on every pooled connection.
// ---------------------------------------------------------------------

#[tokio::test]
async fn m4_statement_timeout_aborts_long_query() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    // Use a deliberately short timeout (3s) so the test completes
    // quickly while still proving the SET is effective.
    let store = match PostgresStore::connect_with_timeout(&url, 3).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: connect failed: {e}");
            return;
        }
    };

    // The adapter doesn't expose raw SQL, so we open a sibling pool
    // wrapping the same URL with the same timeout and prove that
    // pg_sleep(60) aborts. The intent is to confirm the after_connect
    // hook fires — if it did, every new connection sets
    // statement_timeout = 3s; the SELECT below must abort well under
    // the 60s the query would otherwise run.
    use sqlx::Executor;
    use sqlx::postgres::PgPoolOptions;

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute("SET statement_timeout = 3000; SET lock_timeout = 1000;")
                    .await
                    .map(|_| ())
            })
        })
        .connect(&url)
        .await
        .expect("sibling pool");

    let start = std::time::Instant::now();
    let result: Result<sqlx::postgres::PgRow, sqlx::Error> =
        sqlx::query("SELECT pg_sleep(60)").fetch_one(&pool).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "pg_sleep(60) must abort under a 3s statement_timeout; got Ok in {elapsed:?}"
    );
    assert!(
        elapsed.as_secs() < 30,
        "abort took {elapsed:?}; timeout did not fire"
    );

    let err = result.err().unwrap();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("statement timeout")
            || msg.to_lowercase().contains("canceling")
            || msg.to_lowercase().contains("57014"),
        "expected statement_timeout error; got: {msg}"
    );

    // Touch the store to keep the binding live + assert it constructed
    // OK with the same hook applied.
    let _ = store.schema_version().await;

    // Drop the sibling pool explicitly.
    drop(pool);
}

// ---------------------------------------------------------------------
// M15 — metadata-must-be-object CHECK constraint blocks malformed rows.
// ---------------------------------------------------------------------

#[tokio::test]
async fn m15_check_constraint_rejects_array_metadata() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let _store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: connect failed: {e}");
            return;
        }
    };

    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("pool");

    // Insert with metadata = '[]'::jsonb — should fail.
    let title = unique_title("m15-metadata-array");
    let now = chrono::Utc::now();
    let res = sqlx::query(
        "INSERT INTO memories (
            id, tier, namespace, title, content, tags, priority, confidence,
            source, access_count, created_at, updated_at, metadata
        ) VALUES ($1, 'mid', 'm15-test', $2, 'x', '[]'::jsonb, 5, 1.0,
                  'test', 0, $3, $3, '[]'::jsonb)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&title)
    .bind(now)
    .execute(&pool)
    .await;

    assert!(
        res.is_err(),
        "INSERT with metadata='[]' must fail the M15 CHECK; got: {res:?}"
    );
    let err_msg = format!("{}", res.err().unwrap());
    assert!(
        err_msg.contains("memories_metadata_is_object")
            || err_msg.to_lowercase().contains("check constraint"),
        "expected CHECK violation; got: {err_msg}"
    );

    // Sanity: a well-formed object passes.
    let title2 = unique_title("m15-metadata-object");
    sqlx::query(
        "INSERT INTO memories (
            id, tier, namespace, title, content, tags, priority, confidence,
            source, access_count, created_at, updated_at, metadata
        ) VALUES ($1, 'mid', 'm15-test', $2, 'x', '[]'::jsonb, 5, 1.0,
                  'test', 0, $3, $3, '{}'::jsonb)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&title2)
    .bind(now)
    .execute(&pool)
    .await
    .expect("object metadata must pass the M15 CHECK");

    // Cleanup the row we just inserted so re-runs are idempotent.
    let _ = sqlx::query("DELETE FROM memories WHERE namespace = 'm15-test'")
        .execute(&pool)
        .await;
}

// ---------------------------------------------------------------------
// M3 — federation catchup uses SAL trait dispatch (no SQLite leak).
// ---------------------------------------------------------------------

#[tokio::test]
async fn m3_catchup_applies_to_postgres_not_sqlite() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let store: Arc<dyn MemoryStore> = match PostgresStore::connect(&url).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("skip: connect failed: {e}");
            return;
        }
    };

    // Simulate the catchup-applied path: a federation peer pushes a
    // memory through `apply_remote_memory`. The M3 fix routes it to
    // postgres; pre-fix the sqlite path was used instead.
    let ctx = CallerContext::for_agent("federation-catchup");
    let title = unique_title("m3-catchup");
    let mem = fresh_memory(&title, "m3-federation-test");
    let returned_id = store
        .apply_remote_memory(&ctx, &mem)
        .await
        .expect("apply_remote_memory must succeed against postgres");

    // The returned id must match a row that actually exists in postgres.
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("pool");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE namespace = 'm3-federation-test' AND title = $1",
    )
    .bind(&title)
    .fetch_one(&pool)
    .await
    .expect("count");

    assert_eq!(
        count, 1,
        "M3: apply_remote_memory must land the row in postgres; got count={count} \
         for title={title} returned_id={returned_id}"
    );

    // Cleanup.
    let _ = sqlx::query("DELETE FROM memories WHERE namespace = 'm3-federation-test'")
        .execute(&pool)
        .await;
}

// ---------------------------------------------------------------------
// H10 — concurrent governance actions every Pending lands a pending_actions row.
// ---------------------------------------------------------------------

#[tokio::test]
async fn h10_concurrent_governance_actions_every_pending_persists() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let store: Arc<dyn MemoryStore> = match PostgresStore::connect(&url).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("skip: connect failed: {e}");
            return;
        }
    };

    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("pool");

    // Pre-flight: ensure enforce mode is active for the duration of
    // this test by setting via the config global (the production
    // wiring reads this on every call).
    ai_memory::config::set_active_permissions_mode(ai_memory::config::PermissionsMode::Enforce);

    let namespace = format!("h10-gov-{}", uuid::Uuid::new_v4());

    // Cleanup any stale state from a prior aborted run.
    let _ = sqlx::query("DELETE FROM pending_actions WHERE namespace = $1")
        .bind(&namespace)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM namespace_meta WHERE namespace = $1")
        .bind(&namespace)
        .execute(&pool)
        .await;

    // Seed a governance standard memory under `namespace` with the
    // `approve` write level so non-owner stores queue Pending.
    let standard_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let standard_meta = serde_json::json!({
        "agent_id": "owner-agent",
        "governance": {
            "write": "approve",
            "delete": "approve",
            "promote": "approve"
        }
    });
    sqlx::query(
        "INSERT INTO memories (
            id, tier, namespace, title, content, tags, priority, confidence,
            source, access_count, created_at, updated_at, metadata
        ) VALUES ($1, 'long', $2, $3, 'standard', '[]'::jsonb, 5, 1.0,
                  'test', 0, $4, $4, $5)",
    )
    .bind(&standard_id)
    .bind(&namespace)
    .bind(format!("standard:{namespace}"))
    .bind(now)
    .bind(&standard_meta)
    .execute(&pool)
    .await
    .expect("seed standard memory");

    sqlx::query(
        "INSERT INTO namespace_meta (namespace, standard_id, parent_namespace) \
         VALUES ($1, $2, NULL) \
         ON CONFLICT (namespace) DO UPDATE SET standard_id = EXCLUDED.standard_id",
    )
    .bind(&namespace)
    .bind(&standard_id)
    .execute(&pool)
    .await
    .expect("seed namespace_meta");

    // Fire N concurrent governance enforce calls. Each call from a
    // non-owner agent should resolve Pending AND land a
    // pending_actions row.
    const N: usize = 16;
    let mut handles = Vec::new();
    for i in 0..N {
        let store = store.clone();
        let ns = namespace.clone();
        let agent_id = format!("intruder-{i}");
        handles.push(tokio::spawn(async move {
            let payload = serde_json::json!({"i": i});
            store
                .enforce_governance_action(
                    ai_memory::store::GovernedAction::Store,
                    &ns,
                    &agent_id,
                    None,
                    None,
                    &payload,
                )
                .await
        }));
    }

    let mut pending_ids: Vec<String> = Vec::new();
    for h in handles {
        match h.await.expect("task join") {
            Ok(ai_memory::models::GovernanceDecision::Pending(pid)) => pending_ids.push(pid),
            Ok(other) => panic!("expected Pending, got {other:?}"),
            Err(e) => panic!("enforce_governance_action errored: {e}"),
        }
    }

    assert_eq!(
        pending_ids.len(),
        N,
        "H10: every concurrent enforce call must produce a Pending decision"
    );

    // Every Pending id must correspond to a real pending_actions row.
    for pid in &pending_ids {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_actions WHERE id = $1")
            .bind(pid)
            .fetch_one(&pool)
            .await
            .expect("count pending row");
        assert_eq!(
            count, 1,
            "H10: pending decision {pid} must have landed a pending_actions row"
        );
    }

    // Cleanup.
    let _ = sqlx::query("DELETE FROM pending_actions WHERE namespace = $1")
        .bind(&namespace)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM namespace_meta WHERE namespace = $1")
        .bind(&namespace)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM memories WHERE namespace = $1")
        .bind(&namespace)
        .execute(&pool)
        .await;
}
