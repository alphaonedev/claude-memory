// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::items_after_statements
)]
//! v0.7.0.1 G1 — postgres `store_memory` must increment
//! `agent_quotas.current_memories_today`.
//!
//! Reproducer for HALT finding G1 from
//! `runs/v0.7.0-a2a-cont6-cert-r1b-20260509-2148/findings/HALT.md`.
//!
//! ## What this test asserts
//!
//! 1. Boot an in-process HTTP daemon backed by [`PostgresStore`] (the
//!    same adapter the cert harness exercises through `--store-url
//!    postgres://`).
//! 2. POST 50 distinct memories under a fresh `agent_id` through
//!    `POST /api/v1/memories`.
//! 3. POST `{}` to `/api/v1/quota/status` carrying the same `X-Agent-Id`
//!    and assert `current_memories_today == 50`.
//!
//! ## Pre-fix behaviour (the failure shape from S61)
//!
//! All 50 stores succeed (200/201). The `agent_quotas` row for the
//! agent_id auto-inserts via `quota_status`, but
//! `current_memories_today` stays at zero because the postgres
//! `store_memory` path does not run the increment UPDATE. SQLite
//! parity through `quotas::check_and_record` runs as part of the
//! `db::insert` SQL transaction; the postgres adapter's `store` and
//! `store_with_embedding` paths skip it entirely.
//!
//! ## Gating
//!
//! Same gates as `serve_postgres_smoke.rs` — `feature = "sal-postgres"`
//! plus `AI_MEMORY_TEST_POSTGRES_URL` set at run time. Without either,
//! every test prints a skip line and returns cleanly.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::store::MemoryStore;
use ai_memory::store::postgres::PostgresStore;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock};

fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

async fn build_postgres_app_state(url: &str) -> AppState {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).expect("scratch sqlite");
    let path = std::path::PathBuf::from(":memory:");
    let db: Db = Arc::new(Mutex::new((conn, path, ResolvedTtl::default(), true)));
    let store: Arc<dyn MemoryStore> = Arc::new(
        PostgresStore::connect(url)
            .await
            .expect("connect postgres adapter"),
    );
    AppState {
        db,
        embedder: Arc::new(None),
        vector_index: Arc::new(Mutex::new(None)),
        federation: Arc::new(None),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(None),
        family_embeddings: Arc::new(RwLock::new(Some(Vec::new()))),
        storage_backend: StorageBackend::Postgres,
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    }
}

async fn spawn_daemon(
    url: &str,
) -> (
    String,
    Arc<Notify>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let app_state = build_postgres_app_state(url).await;
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_daemon = shutdown.clone();
    let addr_for_daemon = addr.clone();
    let handle = tokio::spawn(async move {
        ai_memory::daemon_runtime::serve_http_with_shutdown(
            &addr_for_daemon,
            api_key_state,
            app_state,
            shutdown_for_daemon,
        )
        .await
    });
    let mut ready = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(resp) = reqwest::get(format!("http://{addr}/api/v1/health")).await
            && resp.status() == reqwest::StatusCode::OK
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "postgres-backed serve never became ready");
    (format!("http://{addr}"), shutdown, handle)
}

/// G1 reproducer — `current_memories_today` must advance by N after N
/// successful `POST /api/v1/memories` calls under the same `agent_id`.
#[tokio::test(flavor = "multi_thread")]
async fn g1_postgres_store_memory_increments_quota_counter() {
    let Some(url) = postgres_url() else {
        eprintln!(
            "skipping g1_postgres_store_memory_increments_quota_counter: \
             AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    // Per-test agent + namespace partitioning so concurrent runs against
    // a shared scratch DB don't collide.
    let suffix = uuid::Uuid::new_v4();
    let agent_id = format!("g1-quota-{suffix}");
    let namespace = format!("g1-quota-ns-{suffix}");

    const N: usize = 50;

    // Pump N stores under the same agent_id.
    for i in 0..N {
        let body = json!({
            "tier": "long",
            "namespace": namespace,
            "title": format!("g1 memory #{i}"),
            "content": "G1 reproducer body — quota increment must fire on PG path",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "system",
            "metadata": { "agent_id": agent_id },
        });
        let resp = client
            .post(format!("{base}/api/v1/memories"))
            .header("x-agent-id", &agent_id)
            .json(&body)
            .send()
            .await
            .expect("store POST");
        assert!(
            resp.status().is_success(),
            "store #{i} should succeed: status={} body={:?}",
            resp.status(),
            resp.text().await.ok()
        );
    }

    // Read the quota row via the documented HTTP surface. The handler
    // requires `agent_id` in the BODY for the single-agent projection;
    // an empty body returns the full operator-facing list.
    let resp = client
        .post(format!("{base}/api/v1/quota/status"))
        .header("x-agent-id", &agent_id)
        .json(&json!({ "agent_id": &agent_id }))
        .send()
        .await
        .expect("quota/status POST");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "quota/status should return 200 for a known agent_id"
    );
    let v: Value = resp.json().await.expect("quota/status body");

    // Wire shape: {agent_id, max_memories_per_day, current_memories_today, ...}.
    let count = v["current_memories_today"]
        .as_i64()
        .expect("current_memories_today must be an integer");

    assert_eq!(
        count, N as i64,
        "G1: postgres `store_memory` must increment \
         `agent_quotas.current_memories_today` to {N} after {N} successful HTTP \
         stores under agent_id={agent_id}; got {count}. Wire body: {v}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}
