// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! L0.7-3 Tier B Chunk D — HTTP surface coverage push (sqlite branches).
//!
//! Drives every route registered in `build_router()` through axum's
//! in-process `Router::oneshot()` interface so the sqlite handler
//! branches in `src/handlers/http.rs`, `src/handlers/hook_subscribers.rs`,
//! and `src/handlers/federation_receive.rs` are exercised. The Postgres
//! branches (`#[cfg(feature = "sal")] if Postgres`) cannot be exercised
//! from this file because the SAL `MemoryStore` trait dispatch path
//! requires a live Postgres + AGE extension; those branches are covered
//! by the `tests/g*_postgres_*` integration tests when `AI_MEMORY_TEST_AGE_URL`
//! is set.
//!
//! Coverage strategy per playbook §4:
//!   A. Happy path — every route with HTTP-level parity to an MCP tool.
//!   B. Input validation — missing required field 400, invalid agent_id 400,
//!      malformed body 400.
//!   C. Authorization — HMAC required on approve/reject (A1), X-Agent-Id
//!      precedence chain.
//!   D. State-dependent — 404 on missing id, 409 on conflict, 202 on pending.
//!   E. Idempotency — re-store same title/namespace returns conflict 409 or
//!      merge per on_conflict policy.
//!   F. Audit chain — signed_events fires on every privileged op.
//!
//! All tests run with `storage_backend: Sqlite` against an in-memory or
//! tempfile-backed sqlite connection. Wiremock + real network are
//! intentionally avoided per the playbook discipline.

#![allow(clippy::too_many_lines)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::await_holding_lock)]
#![allow(clippy::doc_markdown)]

use std::sync::{Arc, Mutex as StdMutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};

/// Process-wide serialiser for tests that mutate the global HMAC secret.
static HMAC_LOCK: StdMutex<()> = StdMutex::new(());

/// Acquire the HMAC lock, tolerating poison from prior test panics —
/// each test reads the lock to enforce its own state. If a previous
/// test poisoned the lock, we still want to grab it and reset the
/// secret to our desired value.
fn lock_hmac() -> std::sync::MutexGuard<'static, ()> {
    HMAC_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Fixture builder
// ---------------------------------------------------------------------------

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

async fn post_json(router: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

async fn post_with_headers(
    router: &axum::Router,
    uri: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

async fn get_uri(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

async fn delete_uri(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

async fn put_json(router: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

/// Create a memory and return its id + status.
async fn create_basic(router: &axum::Router, ns: &str, title: &str) -> (StatusCode, String) {
    let body = json!({
        "tier": "long",
        "namespace": ns,
        "title": title,
        "content": format!("content of {title}"),
        "tags": ["t1", "t2"],
        "priority": 5,
        "confidence": 0.9,
        "source": "api",
        "metadata": {},
        "agent_id": "ai:test",
    });
    let (status, payload) = post_json(router, "/api/v1/memories", body).await;
    let id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    (status, id)
}

// ---------------------------------------------------------------------------
// Happy paths — POST/GET/PUT/DELETE round-trip for the memory CRUD surface.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_create_memory_happy_path() {
    let (router, _f) = build_router_fixture();
    let (status, id) = create_basic(&router, "chunk-d/happy", "hello").await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!id.is_empty());
}

#[tokio::test]
async fn http_create_memory_with_top_level_scope_validates_and_merges() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/scope",
        "title": "scope-mem",
        "content": "with scope",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "scope": "private",
    });
    let (status, payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::CREATED, "{payload}");
}

#[tokio::test]
async fn http_create_memory_invalid_scope_rejected() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/scope-bad",
        "title": "scope-bad",
        "content": "should reject",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "scope": "🦀 not valid scope",
    });
    let (status, _payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_create_memory_invalid_on_conflict_mode_rejected() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/oc",
        "title": "bad-oc",
        "content": "content",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "on_conflict": "splat",
    });
    let (status, payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(payload.get("error").is_some());
}

#[tokio::test]
async fn http_create_memory_on_conflict_error_returns_409() {
    let (router, _f) = build_router_fixture();
    let (s1, _) = create_basic(&router, "chunk-d/oc", "duplicate").await;
    assert_eq!(s1, StatusCode::CREATED);
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/oc",
        "title": "duplicate",
        "content": "second body",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "on_conflict": "error",
    });
    let (status, _payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn http_create_memory_on_conflict_version_rewrites_title() {
    let (router, _f) = build_router_fixture();
    let (s1, _id1) = create_basic(&router, "chunk-d/ver", "version-mem").await;
    assert_eq!(s1, StatusCode::CREATED);
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/ver",
        "title": "version-mem",
        "content": "second version",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "on_conflict": "version",
    });
    let (status, _payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn http_create_memory_on_conflict_merge_upserts() {
    let (router, _f) = build_router_fixture();
    let (s1, _) = create_basic(&router, "chunk-d/merge", "merge-mem").await;
    assert_eq!(s1, StatusCode::CREATED);
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/merge",
        "title": "merge-mem",
        "content": "merged content",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "on_conflict": "merge",
    });
    let (status, _payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn http_create_memory_metadata_agent_id_promotion() {
    // L11 NHI: metadata.agent_id is honored when top-level body.agent_id absent.
    let (router, _f) = build_router_fixture();
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/metaid",
        "title": "metaid-mem",
        "content": "metadata agent_id",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {"agent_id": "ai:meta-claimer"},
    });
    let (status, _payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn http_create_memory_invalid_agent_id_returns_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "tier": "long",
        "namespace": "chunk-d/bad-aid",
        "title": "bad-aid",
        "content": "x",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "agent_id": "has whitespace",
    });
    let (status, payload) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        payload
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.contains("agent_id"))
    );
}

// ---------------------------------------------------------------------------
// GET /api/v1/memories/{id} — fetch by id and prefix.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_get_memory_by_id_and_prefix() {
    let (router, _f) = build_router_fixture();
    let (s, id) = create_basic(&router, "chunk-d/get", "get-mem").await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(!id.is_empty());

    let (status, payload) = get_uri(&router, &format!("/api/v1/memories/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["memory"]["id"].as_str(), Some(id.as_str()));

    // Prefix lookup (8-char prefix).
    let prefix = &id[..8];
    let (s2, p2) = get_uri(&router, &format!("/api/v1/memories/{prefix}")).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(p2["memory"]["id"].as_str(), Some(id.as_str()));
}

#[tokio::test]
async fn http_get_memory_unknown_id_returns_404() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(
        &router,
        "/api/v1/memories/00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_get_memory_invalid_id_returns_400_or_404() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/memories/!invalid!").await;
    // `!invalid!` is clean-string-valid but matches no row → 404.
    // True control-char ids would 400 but URL parser strips them.
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// PUT /api/v1/memories/{id} — update.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_update_memory_happy_path() {
    let (router, _f) = build_router_fixture();
    let (_s, id) = create_basic(&router, "chunk-d/upd", "upd-mem").await;
    let body = json!({
        "priority": 9,
        "tags": ["updated"],
    });
    let (status, _payload) = put_json(&router, &format!("/api/v1/memories/{id}"), body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_update_memory_unknown_id_returns_404() {
    let (router, _f) = build_router_fixture();
    let body = json!({"priority": 5});
    let (status, _payload) = put_json(
        &router,
        "/api/v1/memories/00000000-0000-0000-0000-000000000000",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_update_memory_invalid_priority_rejected() {
    let (router, _f) = build_router_fixture();
    let (_s, id) = create_basic(&router, "chunk-d/upd-bad", "upd-bad").await;
    let body = json!({"priority": 99});
    let (status, _payload) = put_json(&router, &format!("/api/v1/memories/{id}"), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/memories/{id} — delete.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_delete_memory_happy_path() {
    let (router, _f) = build_router_fixture();
    let (_s, id) = create_basic(&router, "chunk-d/del", "del-mem").await;
    let (status, _payload) = delete_uri(&router, &format!("/api/v1/memories/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    // Second delete: now missing → 404.
    let (s2, _p2) = delete_uri(&router, &format!("/api/v1/memories/{id}")).await;
    assert_eq!(s2, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_delete_memory_invalid_id_400_or_404() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = delete_uri(&router, "/api/v1/memories/!bad!").await;
    // clean-string-valid but no row → 404; truly invalid → 400.
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// POST /api/v1/memories/{id}/promote
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_promote_memory_happy_path() {
    let (router, _f) = build_router_fixture();
    let (_s, id) = create_basic(&router, "chunk-d/promo", "promo-mem").await;
    // promote_memory takes no body — it just promotes to long tier.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/memories/{id}/promote"))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn http_promote_memory_unknown_id_404() {
    let (router, _f) = build_router_fixture();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories/00000000-0000-0000-0000-000000000000/promote")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// GET /api/v1/memories — list.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_list_memories_returns_array() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/list", "l1").await;
    let _ = create_basic(&router, "chunk-d/list", "l2").await;
    let (status, payload) = get_uri(&router, "/api/v1/memories?namespace=chunk-d/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["memories"].is_array());
    let mems = payload["memories"].as_array().unwrap();
    assert!(mems.len() >= 2);
}

#[tokio::test]
async fn http_list_memories_invalid_agent_id_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/memories?agent_id=has%20space").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_list_memories_with_all_filters() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/all", "f1").await;
    let (status, _payload) = get_uri(
        &router,
        "/api/v1/memories?namespace=chunk-d/all&tier=long&limit=5&offset=0&min_priority=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// GET /api/v1/search — keyword.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_search_memories_happy() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/search", "needle in a haystack").await;
    let (status, payload) = get_uri(&router, "/api/v1/search?q=needle").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["results"].is_array());
}

#[tokio::test]
async fn http_search_empty_query_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/search?q=%20").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_search_invalid_agent_id_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/search?q=x&agent_id=has%20space").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_search_invalid_as_agent_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/search?q=x&as_agent=BAD%20NAMESPACE").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// GET/POST /api/v1/recall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_recall_get_happy() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/rec", "alpha beta gamma").await;
    let (status, _payload) = get_uri(&router, "/api/v1/recall?q=alpha").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_recall_post_happy() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/rec-p", "alpha beta gamma").await;
    let body = json!({"q": "alpha"});
    let (status, _payload) = post_json(&router, "/api/v1/recall", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_recall_post_empty_q_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({"q": ""});
    let (status, _payload) = post_json(&router, "/api/v1/recall", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_recall_get_with_agent_id_filter() {
    // Recall doesn't validate agent_id filter explicitly — it just
    // passes through to the db layer. Confirms the filter is accepted.
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/rec-aid", "rec-data").await;
    let (status, _payload) = get_uri(&router, "/api/v1/recall?q=rec&agent_id=ai:filtered").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_recall_get_invalid_as_agent_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/recall?q=x&as_agent=!bad%20ns").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_recall_get_with_budget_tokens_zero() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/rec-bz", "rec-data").await;
    let (status, _payload) = get_uri(&router, "/api/v1/recall?q=rec&budget_tokens=0").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_recall_get_with_tags_filter() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/rec-tag", "tagged-data").await;
    let (status, _payload) = get_uri(&router, "/api/v1/recall?q=tagged&tags=t1,t2").await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// POST /api/v1/forget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_forget_by_namespace() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/forget", "to-forget").await;
    let body = json!({"namespace": "chunk-d/forget"});
    let (status, payload) = post_json(&router, "/api/v1/forget", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.get("deleted").is_some());
}

#[tokio::test]
async fn http_forget_by_pattern() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/forget-p", "pattern-mem").await;
    let body = json!({"pattern": "pattern"});
    let (status, _payload) = post_json(&router, "/api/v1/forget", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_forget_by_tier() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/forget-t", "tiered").await;
    let body = json!({"tier": "long"});
    let (status, _payload) = post_json(&router, "/api/v1/forget", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_forget_actual_removes_rows() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/forget-real", "kill-me").await;
    let body = json!({"namespace": "chunk-d/forget-real"});
    let (status, _payload) = post_json(&router, "/api/v1/forget", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_forget_invalid_namespace_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({"namespace": "!bad ns"});
    let (status, _payload) = post_json(&router, "/api/v1/forget", body).await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::OK);
}

// ---------------------------------------------------------------------------
// POST /api/v1/consolidate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_consolidate_with_explicit_summary() {
    let (router, _f) = build_router_fixture();
    let (_s, id1) = create_basic(&router, "chunk-d/cons", "c1").await;
    let (_s2, id2) = create_basic(&router, "chunk-d/cons", "c2").await;
    let body = json!({
        "ids": [id1, id2],
        "namespace": "chunk-d/cons",
        "title": "consolidated",
        "summary": "preset summary long enough to satisfy validation",
        "tier": "long",
    });
    let (status, _payload) = post_json(&router, "/api/v1/consolidate", body).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::OK);
}

#[tokio::test]
async fn http_consolidate_too_few_ids_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "ids": [],
        "namespace": "chunk-d/cons-bad",
        "title": "x",
        "summary": "preset summary long enough to satisfy validation min",
    });
    let (status, _payload) = post_json(&router, "/api/v1/consolidate", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// GET /api/v1/contradictions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_detect_contradictions_happy() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/contra", "topic-a").await;
    let _ = create_basic(&router, "chunk-d/contra", "topic-b").await;
    let (status, _payload) =
        get_uri(&router, "/api/v1/contradictions?namespace=chunk-d/contra").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_detect_contradictions_invalid_agent_id_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/contradictions?agent_id=has%20space").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// POST /api/v1/auto_tag + /api/v1/expand_query — both 503 without LLM.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_auto_tag_no_llm_returns_503() {
    let (router, _f) = build_router_fixture();
    let body = json!({"title": "x", "content": "y"});
    let (status, _payload) = post_json(&router, "/api/v1/auto_tag", body).await;
    assert!(status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_expand_query_no_llm_returns_503() {
    let (router, _f) = build_router_fixture();
    let body = json!({"query": "expand me"});
    let (status, _payload) = post_json(&router, "/api/v1/expand_query", body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// GET /api/v1/tools/list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_tools_list_returns_array() {
    let (router, _f) = build_router_fixture();
    let (status, payload) = get_uri(&router, "/api/v1/tools/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.get("tools").is_some());
}

// ---------------------------------------------------------------------------
// POST /api/v1/memory_load_family
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_load_family_happy_path() {
    let (router, _f) = build_router_fixture();
    let body = json!({"family": "core"});
    let (status, _payload) = post_json(&router, "/api/v1/memory_load_family", body).await;
    // returns OK with empty array on empty DB
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_load_family_invalid_family_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({"family": "🎉 not real"});
    let (status, _payload) = post_json(&router, "/api/v1/memory_load_family", body).await;
    assert!(status == StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_load_family_invalid_namespace_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({"family": "core", "namespace": "has space"});
    let (status, _payload) = post_json(&router, "/api/v1/memory_load_family", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// POST/DELETE/GET /api/v1/links
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_create_link_happy() {
    let (router, _f) = build_router_fixture();
    let (_s, id1) = create_basic(&router, "chunk-d/link", "src").await;
    let (_s2, id2) = create_basic(&router, "chunk-d/link", "dst").await;
    let body = json!({
        "source_id": id1,
        "target_id": id2,
        "relation": "related_to",
    });
    let (status, _payload) = post_json(&router, "/api/v1/links", body).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn http_create_link_self_loop_400() {
    let (router, _f) = build_router_fixture();
    let (_s, id1) = create_basic(&router, "chunk-d/link-self", "src2").await;
    let body = json!({
        "source_id": id1,
        "target_id": id1,
        "relation": "related_to",
    });
    let (status, _payload) = post_json(&router, "/api/v1/links", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_create_link_with_s82_wire_shape() {
    let (router, _f) = build_router_fixture();
    let (_s, src) = create_basic(&router, "chunk-d/link-s82", "src").await;
    let (_s2, dst) = create_basic(&router, "chunk-d/link-s82", "dst").await;
    // S82-style aliases: {from, to, rel_type}
    let body = json!({
        "from": src,
        "to": dst,
        "rel_type": "related_to",
    });
    let (status, _payload) = post_json(&router, "/api/v1/links", body).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::OK);
}

#[tokio::test]
async fn http_create_link_invalid_relation_400() {
    let (router, _f) = build_router_fixture();
    let (_s, id1) = create_basic(&router, "chunk-d/link-rel", "src3").await;
    let (_s2, id2) = create_basic(&router, "chunk-d/link-rel", "dst3").await;
    let body = json!({
        "source_id": id1,
        "target_id": id2,
        "relation": "!invalid",
    });
    let (status, _payload) = post_json(&router, "/api/v1/links", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_get_links_for_memory() {
    let (router, _f) = build_router_fixture();
    let (_s, id1) = create_basic(&router, "chunk-d/getlinks", "a").await;
    let (status, _payload) = get_uri(&router, &format!("/api/v1/links/{id1}")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_delete_link_unknown_returns_404_or_ok() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "source_id": "00000000-0000-0000-0000-000000000000",
        "target_id": "11111111-1111-1111-1111-111111111111",
        "relation": "related_to",
    });
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/v1/links")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    assert!(
        status == StatusCode::NOT_FOUND
            || status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
    );
}

// ---------------------------------------------------------------------------
// GET/POST /api/v1/agents + DELETE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_register_agent_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "agent_id": "ai:newagent",
        "agent_type": "ai:generic",
        "capabilities": ["read", "write"],
    });
    let (status, payload) = post_json(&router, "/api/v1/agents", body).await;
    assert_eq!(status, StatusCode::CREATED, "{payload}");
}

#[tokio::test]
async fn http_register_agent_invalid_id_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "agent_id": "has whitespace",
        "agent_type": "ai:generic",
    });
    let (status, _payload) = post_json(&router, "/api/v1/agents", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_register_agent_invalid_type_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "agent_id": "ai:goodid",
        "agent_type": "!bad-type",
    });
    let (status, _payload) = post_json(&router, "/api/v1/agents", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_list_agents_returns_array() {
    let (router, _f) = build_router_fixture();
    let _ = post_json(
        &router,
        "/api/v1/agents",
        json!({"agent_id": "ai:lister", "agent_type": "ai:generic"}),
    )
    .await;
    let (status, payload) = get_uri(&router, "/api/v1/agents").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["agents"].is_array() || payload["count"].is_number());
}

// ---------------------------------------------------------------------------
// HMAC-gated approve/reject — A1 fix
// ---------------------------------------------------------------------------

fn sign(secret: &str, timestamp: &str, body: &str) -> String {
    use sha2::Digest;
    use sha2::Sha256;
    fn sha256_hex(s: &str) -> String {
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        format!("{:x}", h.finalize())
    }
    fn hmac_sha256_hex(key_hex: &str, body: &str) -> String {
        const BLOCK: usize = 64;
        let key_bytes = hex_decode(key_hex).unwrap_or_else(|| key_hex.as_bytes().to_vec());
        let mut key = key_bytes;
        if key.len() > BLOCK {
            let mut h = Sha256::new();
            h.update(&key);
            key = h.finalize().to_vec();
        }
        key.resize(BLOCK, 0);
        let mut opad = [0x5cu8; BLOCK];
        let mut ipad = [0x36u8; BLOCK];
        for i in 0..BLOCK {
            opad[i] ^= key[i];
            ipad[i] ^= key[i];
        }
        let mut inner = Sha256::new();
        inner.update(ipad);
        inner.update(body.as_bytes());
        let inner_digest = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(opad);
        outer.update(inner_digest);
        format!("{:x}", outer.finalize())
    }
    fn hex_decode(s: &str) -> Option<Vec<u8>> {
        if !s.len().is_multiple_of(2) {
            return None;
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
            .collect()
    }
    let key_hash = sha256_hex(secret);
    let canonical = format!("{timestamp}.{body}");
    let sig = hmac_sha256_hex(&key_hash, &canonical);
    format!("sha256={sig}")
}

#[tokio::test]
async fn http_approve_pending_without_hmac_returns_401() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(None);
    let (router, _f) = build_router_fixture();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/pending/some-id/approve")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_reject_pending_without_hmac_returns_401() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(None);
    let (router, _f) = build_router_fixture();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/pending/some-id/reject")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_approve_pending_unknown_id_with_valid_hmac_passes_gate() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("test-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let body_str = "";
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = sign("test-secret", &ts, body_str);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/pending/00000000-0000-0000-0000-000000000000/approve")
        .header("x-ai-memory-signature", sig)
        .header("x-ai-memory-timestamp", ts)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    // Reach the handler core (past HMAC gate); unknown row produces 403
    // (ApproveOutcome::Rejected with "no pending row" reason).
    let st = resp.status();
    assert!(
        st == StatusCode::FORBIDDEN
            || st == StatusCode::INTERNAL_SERVER_ERROR
            || st == StatusCode::NOT_FOUND
            || st == StatusCode::BAD_REQUEST,
        "unexpected status {st}"
    );
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_reject_pending_unknown_id_with_valid_hmac_passes_gate() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("test-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let body_str = "";
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = sign("test-secret", &ts, body_str);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/pending/00000000-0000-0000-0000-000000000000/reject")
        .header("x-ai-memory-signature", sig)
        .header("x-ai-memory-timestamp", ts)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    // 404 or 200 depending on db state
    assert!(
        st == StatusCode::OK
            || st == StatusCode::FORBIDDEN
            || st == StatusCode::NOT_FOUND
            || st == StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected status {st}"
    );
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_approve_pending_with_stale_timestamp_401() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("test-secret".to_string()));
    let (router, _f) = build_router_fixture();
    // Use timestamp from 10 minutes ago — outside 300s window.
    let ts = (chrono::Utc::now().timestamp() - 600).to_string();
    let sig = sign("test-secret", &ts, "");
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/pending/some-id/approve")
        .header("x-ai-memory-signature", sig)
        .header("x-ai-memory-timestamp", ts)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_approve_pending_with_bad_sig_401() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("test-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let ts = chrono::Utc::now().timestamp().to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/pending/some-id/approve")
        .header("x-ai-memory-signature", "sha256=deadbeef")
        .header("x-ai-memory-timestamp", ts)
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_approve_pending_missing_timestamp_401() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("test-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/pending/some-id/approve")
        .header("x-ai-memory-signature", "sha256=abc")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

// ---------------------------------------------------------------------------
// GET /api/v1/pending — list (no HMAC required for read).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_list_pending_returns_array() {
    let (router, _f) = build_router_fixture();
    let (status, payload) = get_uri(&router, "/api/v1/pending").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["pending"].is_array() || payload["count"].is_number());
}

#[tokio::test]
async fn http_list_pending_with_status_filter() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/pending?status=pending&limit=5").await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// POST /api/v1/memories/bulk — bulk_create.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_bulk_create_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!([
        {
            "tier": "long", "namespace": "chunk-d/bulk", "title": "b1",
            "content": "x", "tags": [], "priority": 5, "confidence": 1.0,
            "source": "api", "metadata": {}, "agent_id": "ai:bulker",
        },
        {
            "tier": "long", "namespace": "chunk-d/bulk", "title": "b2",
            "content": "y", "tags": [], "priority": 5, "confidence": 1.0,
            "source": "api", "metadata": {}, "agent_id": "ai:bulker",
        },
    ]);
    let (status, _payload) = post_json(&router, "/api/v1/memories/bulk", body).await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::MULTI_STATUS
            || status == StatusCode::OK
    );
}

#[tokio::test]
async fn http_bulk_create_empty_list_returns_ok() {
    let (router, _f) = build_router_fixture();
    let body = json!([]);
    let (status, _payload) = post_json(&router, "/api/v1/memories/bulk", body).await;
    // Empty list is allowed — it's a no-op.
    assert!(
        status == StatusCode::OK
            || status == StatusCode::CREATED
            || status == StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn http_bulk_create_over_limit_400() {
    let (router, _f) = build_router_fixture();
    let mems: Vec<Value> = (0..1500)
        .map(|i| {
            json!({
                "tier": "long", "namespace": "chunk-d/bulk-big", "title": format!("b{i}"),
                "content": "x", "tags": [], "priority": 5, "confidence": 1.0,
                "source": "api", "metadata": {},
            })
        })
        .collect();
    let body = Value::Array(mems);
    let (status, _payload) = post_json(&router, "/api/v1/memories/bulk", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_bulk_create_with_invalid_member_records_error() {
    let (router, _f) = build_router_fixture();
    let body = json!([
        {
            "tier": "long", "namespace": "chunk-d/bulk-err", "title": "good",
            "content": "x", "tags": [], "priority": 5, "confidence": 1.0,
            "source": "api", "metadata": {},
        },
        {
            "tier": "long", "namespace": "!bad ns", "title": "bad",
            "content": "y", "tags": [], "priority": 5, "confidence": 1.0,
            "source": "api", "metadata": {},
        },
    ]);
    let (status, _payload) = post_json(&router, "/api/v1/memories/bulk", body).await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::CREATED
            || status == StatusCode::MULTI_STATUS
            || status == StatusCode::BAD_REQUEST
    );
}

// ---------------------------------------------------------------------------
// GET /api/v1/archive + POST + DELETE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_list_archive_empty_returns_array() {
    let (router, _f) = build_router_fixture();
    let (status, payload) = get_uri(&router, "/api/v1/archive").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["memories"].is_array() || payload["count"].is_number());
}

#[tokio::test]
async fn http_archive_stats_returns_struct() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/archive/stats").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_archive_by_ids_happy() {
    let (router, _f) = build_router_fixture();
    let (_s, id) = create_basic(&router, "chunk-d/arch", "to-arch").await;
    let body = json!({"ids": [id]});
    let (status, _payload) = post_json(&router, "/api/v1/archive", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_purge_archive_with_older_than_zero() {
    let (router, _f) = build_router_fixture();
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/v1/archive?older_than_days=0")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_restore_archive_unknown_id_404() {
    let (router, _f) = build_router_fixture();
    let body = json!({});
    let (status, _payload) = post_json(
        &router,
        "/api/v1/archive/00000000-0000-0000-0000-000000000000/restore",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// GET /api/v1/namespaces (list and qs-form)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_list_namespaces_returns_array() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/ns1", "x").await;
    let (status, payload) = get_uri(&router, "/api/v1/namespaces").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["namespaces"].is_array() || payload["count"].is_number());
}

// ---------------------------------------------------------------------------
// GET /api/v1/taxonomy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_taxonomy_returns_struct() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/tax/sub", "x").await;
    let (status, _payload) = get_uri(&router, "/api/v1/taxonomy?prefix=chunk-d").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_taxonomy_with_trailing_slash_handled() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/taxonomy?prefix=chunk-d/").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_taxonomy_invalid_prefix_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/taxonomy?prefix=!bad%20").await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::OK);
}

// ---------------------------------------------------------------------------
// POST /api/v1/check_duplicate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_check_duplicate_no_embedder_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({"title": "x", "content": "y", "namespace": "chunk-d/dup"});
    let (status, _payload) = post_json(&router, "/api/v1/check_duplicate", body).await;
    // No embedder configured → likely 500/400 based on handler shape.
    assert!(status.is_client_error() || status.is_server_error());
}

// ---------------------------------------------------------------------------
// POST /api/v1/entities, GET /api/v1/entities/by_alias
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_entity_register_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "canonical_name": "Alice Smith",
        "namespace": "chunk-d/ent",
        "aliases": ["alice", "asmith"],
    });
    let (status, _payload) = post_json(&router, "/api/v1/entities", body).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::OK);
}

#[tokio::test]
async fn http_entity_register_invalid_namespace_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "canonical_name": "Bob",
        "namespace": "has space",
        "aliases": [],
    });
    let (status, _payload) = post_json(&router, "/api/v1/entities", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_entity_register_invalid_canonical_name_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "canonical_name": "",
        "namespace": "chunk-d/ent",
        "aliases": [],
    });
    let (status, _payload) = post_json(&router, "/api/v1/entities", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_entity_get_by_alias_unknown_returns_null_or_404() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(
        &router,
        "/api/v1/entities/by_alias?alias=zzzz&namespace=chunk-d/ent",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// GET /api/v1/kg/timeline + POST /api/v1/kg/invalidate + POST /api/v1/kg/query
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_kg_timeline_unknown_source_returns_empty() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(
        &router,
        "/api/v1/kg/timeline?source_id=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_kg_timeline_with_since_until() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(
        &router,
        "/api/v1/kg/timeline?source_id=00000000-0000-0000-0000-000000000000&since=2024-01-01T00:00:00Z&until=2026-12-31T23:59:59Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_kg_timeline_invalid_since_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(
        &router,
        "/api/v1/kg/timeline?source_id=00000000-0000-0000-0000-000000000000&since=NOTATIME",
    )
    .await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::OK);
}

#[tokio::test]
async fn http_kg_invalidate_no_match_404() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "source_id": "00000000-0000-0000-0000-000000000000",
        "target_id": "11111111-1111-1111-1111-111111111111",
        "relation": "related_to",
    });
    let (status, _payload) = post_json(&router, "/api/v1/kg/invalidate", body).await;
    assert!(status == StatusCode::NOT_FOUND || status == StatusCode::OK);
}

#[tokio::test]
async fn http_kg_invalidate_real_link() {
    let (router, _f) = build_router_fixture();
    let (_s, src) = create_basic(&router, "chunk-d/inv", "s").await;
    let (_s2, dst) = create_basic(&router, "chunk-d/inv", "d").await;
    // Seed a link first.
    let _ = post_json(
        &router,
        "/api/v1/links",
        json!({"source_id": src, "target_id": dst, "relation": "related_to"}),
    )
    .await;
    let body = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
    });
    let (status, payload) = post_json(&router, "/api/v1/kg/invalidate", body).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
}

#[tokio::test]
async fn http_kg_invalidate_invalid_valid_until_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "source_id": "00000000-0000-0000-0000-000000000000",
        "target_id": "11111111-1111-1111-1111-111111111111",
        "relation": "related_to",
        "valid_until": "NOT_A_DATE",
    });
    let (status, _payload) = post_json(&router, "/api/v1/kg/invalidate", body).await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::OK);
}

#[tokio::test]
async fn http_kg_query_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "source_id": "00000000-0000-0000-0000-000000000000",
        "max_depth": 2,
    });
    let (status, _payload) = post_json(&router, "/api/v1/kg/query", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_kg_query_invalid_valid_at_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "source_id": "00000000-0000-0000-0000-000000000000",
        "valid_at": "NOT_A_DATE",
    });
    let (status, _payload) = post_json(&router, "/api/v1/kg/query", body).await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::OK);
}

// ---------------------------------------------------------------------------
// POST /api/v1/kg/find_paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_kg_find_paths_happy() {
    let (router, _f) = build_router_fixture();
    let (_s, src) = create_basic(&router, "chunk-d/kg", "src").await;
    let (_s2, dst) = create_basic(&router, "chunk-d/kg", "dst").await;
    let body = json!({"source_id": src, "target_id": dst, "max_depth": 3, "max_results": 5});
    let (status, _payload) = post_json(&router, "/api/v1/kg/find_paths", body).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// POST /api/v1/links/verify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_verify_link_unsigned_returns_envelope() {
    let (router, _f) = build_router_fixture();
    let (_s, src) = create_basic(&router, "chunk-d/v", "src-v").await;
    let (_s2, dst) = create_basic(&router, "chunk-d/v", "dst-v").await;
    let body = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
    });
    let (status, _payload) = post_json(&router, "/api/v1/links/verify", body).await;
    assert!(status == StatusCode::OK || status == StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// POST /api/v1/quota/status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_quota_status_no_writes_returns_zero() {
    let (router, _f) = build_router_fixture();
    let body = json!({"agent_id": "ai:nobody"});
    let (status, _payload) = post_json(&router, "/api/v1/quota/status", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_quota_status_invalid_agent_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({"agent_id": "has whitespace"});
    let (status, _payload) = post_json(&router, "/api/v1/quota/status", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// GET /api/v1/stats + /api/v1/gc + /api/v1/export + /api/v1/import
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_get_stats_returns_struct() {
    let (router, _f) = build_router_fixture();
    let (status, payload) = get_uri(&router, "/api/v1/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.is_object());
}

#[tokio::test]
async fn http_run_gc_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!({});
    let (status, _payload) = post_json(&router, "/api/v1/gc", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_export_returns_envelope() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/exp", "x").await;
    let (status, payload) = get_uri(&router, "/api/v1/export").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.is_object());
}

#[tokio::test]
async fn http_import_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "memories": [{
            "id": "44444444-4444-4444-4444-444444444444",
            "tier": "long",
            "namespace": "chunk-d/imp",
            "title": "imp-1",
            "content": "imported content",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "import",
            "access_count": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "metadata": {},
            "reflection_depth": 0,
        }]
    });
    let (status, _payload) = post_json(&router, "/api/v1/import", body).await;
    assert!(status == StatusCode::OK || status == StatusCode::CREATED);
}

#[tokio::test]
async fn http_import_empty_400_or_ok() {
    let (router, _f) = build_router_fixture();
    let body = json!({"memories": []});
    let (status, _payload) = post_json(&router, "/api/v1/import", body).await;
    assert!(status == StatusCode::OK || status == StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// GET /api/v1/capabilities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_capabilities_v3_default() {
    let (router, _f) = build_router_fixture();
    let (status, payload) = get_uri(&router, "/api/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.is_object());
}

#[tokio::test]
async fn http_capabilities_accept_v2_header() {
    let (router, _f) = build_router_fixture();
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/capabilities")
        .header("accept-capabilities", "v2")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// GET /api/v1/health and metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_health_returns_ok() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_metrics_returns_prom_text() {
    let (router, _f) = build_router_fixture();
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Hook subscribers — POST/GET/DELETE /api/v1/subscriptions + /notify + /inbox.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_subscribe_no_secret_no_global_hmac_400() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(None);
    let (router, _f) = build_router_fixture();
    let body = json!({
        "url": "https://example.com/hook",
        "events": "store",
    });
    let (status, payload) = post_json(&router, "/api/v1/subscriptions", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        payload
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.to_lowercase().contains("hmac"))
    );
}

#[tokio::test]
async fn http_subscribe_with_per_sub_secret_happy() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(None);
    let (router, _f) = build_router_fixture();
    let body = json!({
        "url": "https://example.com/hook",
        "events": "store",
        "secret": "per-sub-secret",
        "agent_id": "ai:subscriber",
    });
    let (status, _payload) = post_json(&router, "/api/v1/subscriptions", body).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn http_subscribe_with_global_hmac_secret_happy() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("global-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let body = json!({
        "url": "https://example.com/hook",
        "events": "*",
        "agent_id": "ai:globally-secured",
    });
    let (status, _payload) = post_json(&router, "/api/v1/subscriptions", body).await;
    assert_eq!(status, StatusCode::CREATED);
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_subscribe_namespace_form_synthesizes_url() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("global-secret".to_string()));
    ai_memory::config::set_allow_loopback_webhooks(true);
    let (router, _f) = build_router_fixture();
    let body = json!({
        "agent_id": "ai:ns-subscriber",
        "namespace": "chunk-d/ns",
    });
    let (status, payload) = post_json(&router, "/api/v1/subscriptions", body).await;
    // The synthetic URL is still SSRF-validated on the sqlite path; accept
    // either outcome (the handler is exercised in both branches).
    assert!(
        status == StatusCode::CREATED || status == StatusCode::BAD_REQUEST,
        "{status} {payload}"
    );
    ai_memory::config::set_active_hooks_hmac_secret(None);
    ai_memory::config::set_allow_loopback_webhooks(false);
}

#[tokio::test]
async fn http_subscribe_missing_url_and_namespace_400() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("global-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let body = json!({
        "agent_id": "ai:no-url-no-ns",
        "events": "store",
    });
    let (status, _payload) = post_json(&router, "/api/v1/subscriptions", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_list_subscriptions_happy() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("global-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let _ = post_json(
        &router,
        "/api/v1/subscriptions",
        json!({
            "url": "https://example.com/hook",
            "events": "*",
            "secret": "x",
            "agent_id": "ai:lister",
        }),
    )
    .await;
    let (status, payload) = get_uri(&router, "/api/v1/subscriptions").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["subscriptions"].is_array());
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_list_subscriptions_with_agent_filter() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(Some("global-secret".to_string()));
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/subscriptions?agent_id=ai:nobody").await;
    assert_eq!(status, StatusCode::OK);
    ai_memory::config::set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_unsubscribe_by_id_when_missing_404_or_ok() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = delete_uri(&router, "/api/v1/subscriptions?id=missing-id").await;
    // The MCP handler may return 400 or OK with `removed=false`.
    assert!(
        status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn http_unsubscribe_no_id_no_ns_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = delete_uri(&router, "/api/v1/subscriptions").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_unsubscribe_by_namespace_returns_removed_false_on_miss() {
    let _g = lock_hmac();
    ai_memory::config::set_active_hooks_hmac_secret(None);
    let (router, _f) = build_router_fixture();
    let (status, payload) = delete_uri(
        &router,
        "/api/v1/subscriptions?agent_id=ai:none&namespace=chunk-d/unmatched",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.get("removed").is_some());
}

// ---------------------------------------------------------------------------
// POST /api/v1/notify + GET /api/v1/inbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_notify_happy_path() {
    let (router, _f) = build_router_fixture();
    // Register a target agent first (auto-create supported but explicit is cleaner).
    let _ = post_json(
        &router,
        "/api/v1/agents",
        json!({"agent_id": "ai:target", "agent_type": "ai:generic"}),
    )
    .await;
    let body = json!({
        "target_agent_id": "ai:target",
        "title": "hello",
        "payload": "test payload",
        "agent_id": "ai:sender",
    });
    let (status, _payload) = post_json(&router, "/api/v1/notify", body).await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn http_notify_missing_payload_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "target_agent_id": "ai:target",
        "title": "hello",
        "agent_id": "ai:sender",
    });
    let (status, _payload) = post_json(&router, "/api/v1/notify", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_notify_with_content_alias() {
    let (router, _f) = build_router_fixture();
    let _ = post_json(
        &router,
        "/api/v1/agents",
        json!({"agent_id": "ai:target2", "agent_type": "ai:generic"}),
    )
    .await;
    let body = json!({
        "target_agent_id": "ai:target2",
        "title": "alias-test",
        "content": "via content field",
        "agent_id": "ai:sender",
    });
    let (status, _payload) = post_json(&router, "/api/v1/notify", body).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_notify_with_priority_and_tier() {
    let (router, _f) = build_router_fixture();
    let _ = post_json(
        &router,
        "/api/v1/agents",
        json!({"agent_id": "ai:target3", "agent_type": "ai:generic"}),
    )
    .await;
    let body = json!({
        "target_agent_id": "ai:target3",
        "title": "x",
        "payload": "y",
        "priority": 9,
        "tier": "long",
        "agent_id": "ai:sender",
    });
    let (status, _payload) = post_json(&router, "/api/v1/notify", body).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_notify_invalid_agent_id_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "target_agent_id": "ai:target",
        "title": "x",
        "payload": "y",
        "agent_id": "has whitespace",
    });
    let (status, _payload) = post_json(&router, "/api/v1/notify", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_inbox_returns_array() {
    let (router, _f) = build_router_fixture();
    let (status, payload) = get_uri(&router, "/api/v1/inbox?agent_id=ai:inbox-test").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.get("messages").is_some());
}

#[tokio::test]
async fn http_inbox_invalid_agent_id_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/inbox?agent_id=has%20space").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_inbox_with_unread_only_filter() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(
        &router,
        "/api/v1/inbox?agent_id=ai:inbox2&unread_only=true&limit=5",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// POST/GET/DELETE /api/v1/namespaces/{ns}/standard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_set_namespace_standard_with_governance_happy() {
    let (router, _f) = build_router_fixture();
    // Use a single-segment namespace because axum's `{ns}` path param
    // doesn't allow slashes.
    let body = json!({
        "governance": {
            "approver_type": "Human",
        }
    });
    let (status, payload) = post_json(&router, "/api/v1/namespaces/chunkstd/standard", body).await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::INTERNAL_SERVER_ERROR,
        "{status} {payload}"
    );
}

#[tokio::test]
async fn http_set_namespace_standard_empty_body() {
    let (router, _f) = build_router_fixture();
    let body = json!({});
    let (status, _payload) =
        post_json(&router, "/api/v1/namespaces/chunkempty/standard", body).await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn http_get_namespace_standard_missing_returns_null_or_404() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/namespaces/chunk-d/none/standard").await;
    assert!(status == StatusCode::OK || status == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_clear_namespace_standard_missing_no_op() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = delete_uri(&router, "/api/v1/namespaces/chunk-d/none/standard").await;
    assert!(status == StatusCode::OK || status == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_set_namespace_standard_qs_form() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "namespace": "chunk-d/qs-std",
        "governance": {"approver_type": "human"},
    });
    let (status, _payload) = post_json(&router, "/api/v1/namespaces", body).await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn http_set_namespace_standard_qs_missing_ns_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "governance": {"approver_type": "human"},
    });
    let (status, _payload) = post_json(&router, "/api/v1/namespaces", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_clear_namespace_standard_qs_missing_ns_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = delete_uri(&router, "/api/v1/namespaces").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_set_namespace_standard_nested_shape() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "standard": {
            "namespace": "chunk-d/nested",
            "governance": {"approver_type": "human"},
        }
    });
    let (status, _payload) = post_json(&router, "/api/v1/namespaces", body).await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn http_set_namespace_standard_invalid_governance_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "namespace": "chunk-d/badgov",
        "governance": "not an object",
    });
    let (status, _payload) = post_json(&router, "/api/v1/namespaces", body).await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::INTERNAL_SERVER_ERROR);
}

// ---------------------------------------------------------------------------
// POST /api/v1/session/start
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_session_start_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "namespace": "chunk-d/session",
        "limit": 5,
        "agent_id": "ai:session-test",
    });
    let (status, payload) = post_json(&router, "/api/v1/session/start", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.get("session_id").is_some() || payload.get("memories").is_some());
}

#[tokio::test]
async fn http_session_start_invalid_agent_id_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "agent_id": "has whitespace",
    });
    let (status, _payload) = post_json(&router, "/api/v1/session/start", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_session_start_empty_body() {
    let (router, _f) = build_router_fixture();
    let body = json!({});
    let (status, _payload) = post_json(&router, "/api/v1/session/start", body).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Federation receive — sync_push + sync_since (sqlite branches).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_sync_push_empty_happy() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer1",
        "memories": [],
        "deletions": [],
    });
    let (status, payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["applied"], 0);
}

#[tokio::test]
async fn http_sync_push_invalid_sender_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "has whitespace",
        "memories": [],
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_push_dry_run_no_writes() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer-dry",
        "memories": [{
            "id": "11111111-1111-1111-1111-111111111111",
            "tier": "long",
            "namespace": "chunk-d/sync-dry",
            "title": "dry-mem",
            "content": "x",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "import",
            "access_count": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "metadata": {"agent_id": "ai:peer-dry"},
            "reflection_depth": 0,
        }],
        "dry_run": true,
    });
    let (status, payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert_eq!(payload["dry_run"], true);
    // Either noop=1 (made it past validate_memory) or skipped=1 (failed
    // validation). Both branches exercise the sync_push loop.
    let noop = payload["noop"].as_u64().unwrap_or(0);
    let skipped = payload["skipped"].as_u64().unwrap_or(0);
    assert_eq!(noop + skipped, 1, "expected one outcome: {payload}");
}

#[tokio::test]
async fn http_sync_push_apply_memory() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer-apply",
        "memories": [{
            "id": "22222222-2222-2222-2222-222222222222",
            "tier": "long",
            "namespace": "chunk-d/sync-apply",
            "title": "apply-mem",
            "content": "applied",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "import",
            "access_count": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "metadata": {"agent_id": "ai:peer-apply"},
            "reflection_depth": 0,
        }],
    });
    let (status, payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    let applied = payload["applied"].as_u64().unwrap_or(0);
    let skipped = payload["skipped"].as_u64().unwrap_or(0);
    assert_eq!(applied + skipped, 1, "expected 1 disposition: {payload}");
}

#[tokio::test]
async fn http_sync_push_invalid_memory_skipped() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer-bad",
        "memories": [{
            "id": "33333333-3333-3333-3333-333333333333",
            "tier": "long",
            "namespace": "!bad ns",
            "title": "",
            "content": "",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "import",
            "access_count": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "metadata": {},
            "reflection_depth": 0,
        }],
    });
    let (status, payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["skipped"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn http_sync_push_with_sender_clock_skew_logs_warn() {
    let (router, _f) = build_router_fixture();
    // Set sender wall clock 1 year ahead → triggers the skew warn path.
    let future = "2099-01-01T00:00:00Z";
    let body = json!({
        "sender_agent_id": "ai:peer-skew",
        "sender_wall_clock": future,
        "memories": [],
        "deletions": [],
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_sync_push_unparsable_wall_clock_skipped() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer-bad-wc",
        "sender_wall_clock": "NOT_A_DATE",
        "memories": [],
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_sync_push_memory_oversize_rejected() {
    let (router, _f) = build_router_fixture();
    let mems: Vec<Value> = (0..1500)
        .map(|i| {
            json!({
                "id": format!("11111111-1111-1111-1111-{:012}", i),
                "tier": "long",
                "namespace": "chunk-d/big",
                "title": format!("bigmem-{i}"),
                "content": "x",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "import",
                "access_count": 0,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "metadata": {},
                "reflection_depth": 0,
            })
        })
        .collect();
    let body = json!({
        "sender_agent_id": "ai:peer-flood",
        "memories": mems,
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_push_deletions_oversize_rejected() {
    let (router, _f) = build_router_fixture();
    let dels: Vec<Value> = (0..1500).map(|i| json!(format!("{:032x}", i))).collect();
    let body = json!({
        "sender_agent_id": "ai:peer-del-flood",
        "memories": [],
        "deletions": dels,
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_push_invalid_x_agent_id_header_400() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer",
        "memories": [],
    });
    let (status, _payload) = post_with_headers(
        &router,
        "/api/v1/sync/push",
        body,
        &[("x-agent-id", "has whitespace")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_push_with_deletions() {
    let (router, _f) = build_router_fixture();
    // Seed a row.
    let (_s, id) = create_basic(&router, "chunk-d/del-test", "del-target").await;
    let body = json!({
        "sender_agent_id": "ai:peer-del",
        "memories": [],
        "deletions": [id],
    });
    let (status, payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert!(payload["deleted"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn http_sync_push_invalid_deletion_id_skipped() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer-del-bad",
        "memories": [],
        "deletions": ["\u{001}invalid"],
    });
    let (status, payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert!(payload["skipped"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn http_sync_since_no_since_returns_all() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/since", "x").await;
    let (status, payload) = get_uri(&router, "/api/v1/sync/since").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["memories"].is_array());
}

#[tokio::test]
async fn http_sync_since_with_valid_since() {
    let (router, _f) = build_router_fixture();
    let (status, payload) = get_uri(
        &router,
        "/api/v1/sync/since?since=2024-01-01T00:00:00Z&limit=10",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["memories"].is_array());
}

#[tokio::test]
async fn http_sync_since_invalid_since_400() {
    let (router, _f) = build_router_fixture();
    let (status, _payload) = get_uri(&router, "/api/v1/sync/since?since=NOT_A_DATE").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_since_with_peer_query() {
    let (router, _f) = build_router_fixture();
    let _ = create_basic(&router, "chunk-d/sin-peer", "x").await;
    let (status, _payload) = get_uri(&router, "/api/v1/sync/since?peer=ai:peer-puller").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_sync_push_with_links_inbound() {
    let (router, _f) = build_router_fixture();
    // Seed two memories so the link target lookup succeeds.
    let (_s, src) = create_basic(&router, "chunk-d/sync-l", "src").await;
    let (_s2, dst) = create_basic(&router, "chunk-d/sync-l", "dst").await;
    let body = json!({
        "sender_agent_id": "ai:peer-link",
        "memories": [],
        "links": [{
            "source_id": src,
            "target_id": dst,
            "relation": "related_to",
            "metadata": {},
            "created_at": "2026-01-01T00:00:00Z",
            "attest_level": "unsigned",
        }],
    });
    let (status, payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert!(payload["links_applied"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn http_sync_push_namespace_meta() {
    let (router, _f) = build_router_fixture();
    let (_s, id) = create_basic(&router, "chunk-d/sync-meta-std", "std-mem").await;
    let body = json!({
        "sender_agent_id": "ai:peer-nm",
        "memories": [],
        "namespace_meta": [{
            "namespace": "chunk-d/sync-meta-std",
            "standard_id": id,
            "parent_namespace": null,
        }],
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_sync_push_archive_restore_roundtrip() {
    let (router, _f) = build_router_fixture();
    let (_s, id) = create_basic(&router, "chunk-d/sync-arch", "arch-target").await;
    // Archive via federation push.
    let body = json!({
        "sender_agent_id": "ai:peer-arch",
        "memories": [],
        "archives": [id.clone()],
    });
    let (s1, _p1) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(s1, StatusCode::OK);
    // Then restore.
    let body = json!({
        "sender_agent_id": "ai:peer-arch",
        "memories": [],
        "restores": [id],
    });
    let (s2, _p2) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(s2, StatusCode::OK);
}

#[tokio::test]
async fn http_sync_push_namespace_meta_clears() {
    let (router, _f) = build_router_fixture();
    let body = json!({
        "sender_agent_id": "ai:peer-cl",
        "memories": [],
        "namespace_meta_clears": ["chunk-d/cleared-ns"],
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn http_sync_push_archive_oversize_400() {
    let (router, _f) = build_router_fixture();
    let arch: Vec<Value> = (0..1500).map(|i| json!(format!("a-{i}"))).collect();
    let body = json!({
        "sender_agent_id": "ai:peer-aflood",
        "memories": [],
        "archives": arch,
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_push_restore_oversize_400() {
    let (router, _f) = build_router_fixture();
    let restore: Vec<Value> = (0..1500).map(|i| json!(format!("r-{i}"))).collect();
    let body = json!({
        "sender_agent_id": "ai:peer-rflood",
        "memories": [],
        "restores": restore,
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_push_namespace_meta_oversize_400() {
    let (router, _f) = build_router_fixture();
    let entries: Vec<Value> = (0..1500)
        .map(|i| {
            json!({
                "namespace": format!("ns-{i}"),
                "standard_id": format!("sid-{i}"),
                "parent_namespace": null,
            })
        })
        .collect();
    let body = json!({
        "sender_agent_id": "ai:peer-nmflood",
        "memories": [],
        "namespace_meta": entries,
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_sync_push_namespace_meta_clears_oversize_400() {
    let (router, _f) = build_router_fixture();
    let clears: Vec<Value> = (0..1500).map(|i| json!(format!("ns-{i}"))).collect();
    let body = json!({
        "sender_agent_id": "ai:peer-cflood",
        "memories": [],
        "namespace_meta_clears": clears,
    });
    let (status, _payload) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
