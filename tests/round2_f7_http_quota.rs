// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Round-2 F7 — HTTP `POST /api/v1/memories` increments
//! `agent_quotas` counters.
//!
//! Round-2 evidence: 500 stores from `agent-d-quota:alpha-01` + 5 from
//! `:beta-01` via the HTTP API succeeded but `memory_quota_status`
//! showed zero new rows. The MCP store path (src/mcp.rs:1691) calls
//! `quotas::check_and_record` ahead of `db::insert`; the HTTP store
//! path was missing that wiring entirely. This test pumps 50 HTTP
//! stores from a single `agent_id` and asserts the per-agent
//! counter advances by 50.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rusqlite::Connection;
use serde_json::json;
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};

fn build_test_router() -> (axum::Router, std::path::PathBuf, NamedTempFile) {
    let f = NamedTempFile::new().expect("tempfile");
    let db_path = f.path().to_path_buf();
    // Open + run all migrations (including 0022_v07_agent_quotas).
    let _ = ai_memory::db::open(&db_path).expect("db::open");

    let conn = ai_memory::db::open(&db_path).expect("reopen for AppState");
    let db: Db = Arc::new(Mutex::new((
        conn,
        db_path.clone(),
        ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: Arc<dyn ai_memory::store::MemoryStore> =
        Arc::new(ai_memory::store::sqlite::SqliteStore::open(&db_path).expect("open SqliteStore"));
    let app_state = AppState {
        db,
        embedder: Arc::new(None),
        vector_index: Arc::new(Mutex::new(None)),
        federation: Arc::new(None),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(None),
        family_embeddings: Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
    };
    let api_key_state = ApiKeyState { key: None };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, db_path, f)
}

async fn http_store(
    router: &axum::Router,
    agent_id: &str,
    namespace: &str,
    title: &str,
) -> StatusCode {
    let body = json!({
        "tier": "long",
        "namespace": namespace,
        "title": title,
        "content": "round-2 F7 quota test body",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "agent_id": agent_id,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    resp.status()
}

fn read_quota_count(db_path: &Path, agent_id: &str) -> i64 {
    let conn = Connection::open(db_path).expect("open for quota read");
    ai_memory::quotas::get_status(&conn, agent_id)
        .expect("quota row")
        .current_memories_today
}

#[tokio::test]
async fn http_post_memories_increments_agent_quota_counter() {
    let (router, db_path, _keep) = build_test_router();
    let agent_id = "round2-f7-http-agent";
    let namespace = "round2-f7";

    // Pump 50 stores via the HTTP store path under a single agent_id.
    // Pre-F7, the counter stayed at zero; post-F7 it must equal 50.
    for i in 0..50 {
        let title = format!("round2-f7 memory #{i}");
        let status = http_store(&router, agent_id, namespace, &title).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "HTTP store #{i} must succeed (got {status})"
        );
    }

    let count = read_quota_count(&db_path, agent_id);
    assert_eq!(
        count, 50,
        "F7: HTTP store path must increment `agent_quotas.current_memories_today` to 50 \
         (got {count}); the MCP path already does, the HTTP path was the bypass surface"
    );
}

#[tokio::test]
async fn http_post_memories_partitions_quota_by_agent_id() {
    // The Round-2 evidence pumped through `:alpha-01` AND `:beta-01`;
    // pin that the per-agent counter is partitioned correctly so an
    // alpha-write never lands on beta's row.
    let (router, db_path, _keep) = build_test_router();
    let alpha = "round2-f7-agent:alpha";
    let beta = "round2-f7-agent:beta";

    for i in 0..7 {
        let title = format!("alpha #{i}");
        let s = http_store(&router, alpha, "round2-f7", &title).await;
        assert_eq!(s, StatusCode::CREATED);
    }
    for i in 0..3 {
        let title = format!("beta #{i}");
        let s = http_store(&router, beta, "round2-f7", &title).await;
        assert_eq!(s, StatusCode::CREATED);
    }

    let alpha_count = read_quota_count(&db_path, alpha);
    let beta_count = read_quota_count(&db_path, beta);
    assert_eq!(alpha_count, 7, "alpha agent must see exactly 7 increments");
    assert_eq!(beta_count, 3, "beta agent must see exactly 3 increments");
}

#[tokio::test]
async fn http_post_memories_quota_storage_bytes_advance() {
    // Beyond the count counter, F7 also wires the storage_bytes
    // accumulator — same shape as the MCP path so cross-path totals
    // remain coherent.
    let (router, db_path, _keep) = build_test_router();
    let agent_id = "round2-f7-storage";

    let s = http_store(&router, agent_id, "round2-f7", "storage probe").await;
    assert_eq!(s, StatusCode::CREATED);

    let conn = Connection::open(&db_path).unwrap();
    let status = ai_memory::quotas::get_status(&conn, agent_id).unwrap();
    assert_eq!(status.current_memories_today, 1);
    assert!(
        status.current_storage_bytes > 0,
        "F7: storage_bytes counter must advance on HTTP store (got {})",
        status.current_storage_bytes
    );
}
