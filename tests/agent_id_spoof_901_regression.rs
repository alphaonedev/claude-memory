// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #901 — agent_id-spoof regression pins for the three handler
//! surfaces closed in commit b4ba16c8c.
//!
//! Pre-#901 the following three handlers trusted caller-supplied
//! `agent_id` (body or query) as authenticated identity:
//!
//! - `src/handlers/subscriptions.rs::notify` — `body.agent_id`
//! - `src/handlers/subscriptions.rs::subscribe` — `body.agent_id`
//! - `src/handlers/hook_subscribers.rs::get_inbox` — `?agent_id=` query
//!
//! Post-#901 the header `X-Agent-Id` is the only trusted source; the
//! body / query value (if present) MUST match the header else 403.
//! These tests pin the FORBIDDEN branch on each of the three surfaces
//! — they would FAIL on pre-#901 code (which would route through to
//! the storage layer and stamp the spoofed identity) and PASS on the
//! current HEAD.
//!
//! Happy-path coverage for the same surfaces lives in
//! `tests/handler_postgres_branches_fake_pg.rs::{pg_notify_happy_path,
//! pg_subscribe_namespace_form_synthesizes_url_pg, pg_inbox_returns_envelope}`.

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

fn build_router_fixture() -> (axum::Router, NamedTempFile) {
    let f = NamedTempFile::new().expect("tempfile");
    let db_path = f.path().to_path_buf();
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
        replay_cache: Arc::new(ai_memory::identity::replay::ReplayCache::default()),
        verify_require_nonce: false,
        federation_nonce_cache: std::sync::Arc::new(
            ai_memory::identity::replay::FederationNonceCache::default(),
        ),
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    };
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, f)
}

async fn post_json_as(
    router: &axum::Router,
    uri: &str,
    body: Value,
    caller: &str,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-agent-id", caller)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn get_as(router: &axum::Router, uri: &str, caller: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-agent-id", caller)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn notify_rejects_spoofed_body_agent_id_901() {
    let (router, _f) = build_router_fixture();
    let (status, body) = post_json_as(
        &router,
        "/api/v1/notify",
        json!({
            "target_agent_id": "recipient",
            "title": "spoof-notify",
            "payload": "spoof attempt",
            "agent_id": "alice",
            "priority": 5,
        }),
        "bob",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "#901: notify body.agent_id spoof must 403, got {status} body={body}"
    );
}

#[tokio::test]
async fn subscribe_rejects_spoofed_body_agent_id_901() {
    let (router, _f) = build_router_fixture();
    let (status, body) = post_json_as(
        &router,
        "/api/v1/subscriptions",
        json!({
            "agent_id": "alice",
            "url": "https://example.com/hook",
            "events": "*",
            "secret": "hex-secret",
        }),
        "bob",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "#901: subscribe body.agent_id spoof must 403, got {status} body={body}"
    );
}

#[tokio::test]
async fn get_inbox_rejects_spoofed_query_agent_id_901() {
    let (router, _f) = build_router_fixture();
    let (status, body) = get_as(&router, "/api/v1/inbox?agent_id=alice", "bob").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "#901: get_inbox ?agent_id= spoof must 403, got {status} body={body}"
    );
}
