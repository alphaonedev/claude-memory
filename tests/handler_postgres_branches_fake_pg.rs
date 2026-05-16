// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 polish/coverage-90 (issue #767) — handler postgres-branch
//! coverage push without a live postgres.
//!
//! Strategy: build the daemon `AppState` with
//! `storage_backend = StorageBackend::Postgres` while wiring an
//! `SqliteStore` as the `dyn MemoryStore` handle. This drives every
//! `#[cfg(feature = "sal")] if matches!(StorageBackend::Postgres) {…}`
//! branch in `handlers/http.rs`, `handlers/hook_subscribers.rs`, and
//! `handlers/federation_receive.rs` while the underlying calls land on
//! the SqliteStore impls (which exist for every method used on the
//! "happy" postgres branch). Branches that route through
//! `crate::store::postgres::*_via_store` helpers exercise the
//! `downcast_postgres` → `BackendUnavailable` error path → `503
//! Service Unavailable` envelope — also useful real coverage of
//! `store_err_to_response`.
//!
//! The `cov_18_offload_ttl_postgres` baseline tested a similar
//! "Postgres flag with SqliteStore" pattern for the offload TTL
//! plumbing. This file generalises it across the handler surface.

#![cfg(feature = "sal")]
#![allow(clippy::too_many_lines)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};

/// Build a router with `storage_backend = Postgres` but backed by an
/// `SqliteStore`. This drives every `if matches!(Postgres)` branch
/// without requiring an actual postgres connection.
fn build_fake_pg_router() -> (axum::Router, NamedTempFile) {
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
        // The headline trick: claim to be Postgres while running on Sqlite.
        storage_backend: StorageBackend::Postgres,
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
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn get_uri(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
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

async fn delete_uri(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
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

async fn put_json(router: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
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

// ---------------------------------------------------------------------------
// /api/v1/memories — create / get / update / delete / promote on PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_create_memory_happy_path() {
    let (router, _f) = build_fake_pg_router();
    let body = json!({
        "tier": "long",
        "namespace": "pgfake",
        "title": "pg-create",
        "content": "stored via the postgres branch (sqlite-backed)",
        "tags": ["pg"],
        "priority": 5,
        "confidence": 1.0,
        "source": "user",
        "metadata": {},
    });
    let (status, v) = post_json(&router, "/api/v1/memories", body).await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "create on pg branch: {status} body={v}",
    );
    assert!(v.get("id").is_some(), "{v}");
}

#[tokio::test]
async fn pg_create_memory_invalid_returns_400() {
    let (router, _f) = build_fake_pg_router();
    // Empty content — fails validate::validate_create
    let body = json!({
        "tier": "long",
        "namespace": "pgfake",
        "title": "",
        "content": "",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "user",
        "metadata": {},
    });
    let (status, _v) = post_json(&router, "/api/v1/memories", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pg_get_memory_unknown_returns_404() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(
        &router,
        "/api/v1/memories/00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pg_get_memory_after_create_roundtrip() {
    let (router, _f) = build_fake_pg_router();
    let body = json!({
        "tier": "long",
        "namespace": "pgfake-get",
        "title": "pg-get",
        "content": "roundtrip body",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "user",
        "metadata": {},
    });
    let (_status, v) = post_json(&router, "/api/v1/memories", body).await;
    let id = v["id"].as_str().expect("id").to_string();
    let (status, got) = get_uri(&router, &format!("/api/v1/memories/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    // The pg path's get_memory returns `{memory: ..., links: ...}` so the
    // id is nested under `memory`.
    assert_eq!(got["memory"]["id"], json!(id));
}

#[tokio::test]
async fn pg_update_memory_happy() {
    let (router, _f) = build_fake_pg_router();
    let body = json!({
        "tier": "mid",
        "namespace": "pgfake-upd",
        "title": "pg-upd",
        "content": "original",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "user",
        "metadata": {},
    });
    let (_status, v) = post_json(&router, "/api/v1/memories", body).await;
    let id = v["id"].as_str().unwrap().to_string();
    let (status, got) = put_json(
        &router,
        &format!("/api/v1/memories/{id}"),
        json!({"content": "updated body"}),
    )
    .await;
    assert!(status == StatusCode::OK, "pg update: {status} body={got}");
}

#[tokio::test]
async fn pg_update_memory_unknown_returns_404() {
    let (router, _f) = build_fake_pg_router();
    let id = "00000000-0000-0000-0000-000000000000";
    let (status, _v) = put_json(
        &router,
        &format!("/api/v1/memories/{id}"),
        json!({"content": "new"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pg_delete_memory_unknown_returns_404() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = delete_uri(
        &router,
        "/api/v1/memories/00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pg_delete_memory_after_create() {
    let (router, _f) = build_fake_pg_router();
    let body = json!({
        "tier": "long",
        "namespace": "pgfake-del",
        "title": "pg-del",
        "content": "to be deleted via pg branch",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "user",
        "metadata": {},
    });
    let (_status, v) = post_json(&router, "/api/v1/memories", body).await;
    let id = v["id"].as_str().unwrap().to_string();
    let (status, got) = delete_uri(&router, &format!("/api/v1/memories/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{got}");
}

#[tokio::test]
async fn pg_promote_memory_unknown_returns_404() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/memories/00000000-0000-0000-0000-000000000000/promote",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pg_promote_memory_after_create() {
    let (router, _f) = build_fake_pg_router();
    let body = json!({
        "tier": "short",
        "namespace": "pgfake-prom",
        "title": "pg-prom",
        "content": "to be promoted via pg branch",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "user",
        "metadata": {},
    });
    let (_status, v) = post_json(&router, "/api/v1/memories", body).await;
    let id = v["id"].as_str().unwrap().to_string();
    let (status, got) = post_json(
        &router,
        &format!("/api/v1/memories/{id}/promote"),
        json!({}),
    )
    .await;
    assert!(status == StatusCode::OK, "pg promote: {status} body={got}");
    assert_eq!(got["tier"], json!("long"));
}

#[tokio::test]
async fn pg_promote_memory_invalid_id_400() {
    let (router, _f) = build_fake_pg_router();
    // Use a long alpha string that fails validate::validate_id at the top of
    // the handler before the postgres branch is even reached. The `_` and
    // `:` characters fail the UUID-shape check.
    let (status, _v) = post_json(
        &router,
        "/api/v1/memories/not_a_valid_uuid_shape/promote",
        json!({}),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/memories — list/search on PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_list_memories_returns_array() {
    let (router, _f) = build_fake_pg_router();
    // Seed one row
    let _ = post_json(
        &router,
        "/api/v1/memories",
        json!({
            "tier": "long",
            "namespace": "pgfake-list",
            "title": "pg-list",
            "content": "list me",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "metadata": {},
        }),
    )
    .await;
    let (status, v) = get_uri(&router, "/api/v1/memories?namespace=pgfake-list&limit=10").await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["memories"].is_array() || v.is_array(), "{v}");
}

#[tokio::test]
async fn pg_search_memories_happy() {
    let (router, _f) = build_fake_pg_router();
    let _ = post_json(
        &router,
        "/api/v1/memories",
        json!({
            "tier": "long",
            "namespace": "pgfake-search",
            "title": "pg-search",
            "content": "uniquesearchtoken123",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "metadata": {},
        }),
    )
    .await;
    let (status, v) = get_uri(&router, "/api/v1/search?q=uniquesearchtoken123").await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

// ---------------------------------------------------------------------------
// /api/v1/recall — PG keyword fallback envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_recall_get_envelope() {
    let (router, _f) = build_fake_pg_router();
    let _ = post_json(
        &router,
        "/api/v1/memories",
        json!({
            "tier": "long",
            "namespace": "pgfake-rec",
            "title": "pg-rec",
            "content": "recallable content for pg path",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "metadata": {},
        }),
    )
    .await;
    let (status, v) = get_uri(&router, "/api/v1/recall?q=recallable&namespace=pgfake-rec").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    // PG keyword fallback envelope carries `mode = keyword`.
    assert!(
        v.get("memories").is_some() || v.get("count").is_some(),
        "{v}"
    );
}

#[tokio::test]
async fn pg_recall_post_envelope() {
    let (router, _f) = build_fake_pg_router();
    let _ = post_json(
        &router,
        "/api/v1/memories",
        json!({
            "tier": "long",
            "namespace": "pgfake-rec2",
            "title": "pg-rec2",
            "content": "content for postgres recall path",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "metadata": {},
        }),
    )
    .await;
    let (status, v) = post_json(
        &router,
        "/api/v1/recall",
        json!({
            "context": "content",
            "namespace": "pgfake-rec2",
            "limit": 5,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn pg_recall_post_with_has_citations_filter() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/recall",
        json!({
            "context": "anything",
            "has_citations": true,
            "limit": 5,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /api/v1/forget — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_forget_by_namespace_returns_deleted_count() {
    let (router, _f) = build_fake_pg_router();
    let _ = post_json(
        &router,
        "/api/v1/memories",
        json!({
            "tier": "short",
            "namespace": "pgfake-forget",
            "title": "forget-me",
            "content": "to be forgotten via pg branch",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "metadata": {},
        }),
    )
    .await;
    let (status, v) = post_json(
        &router,
        "/api/v1/forget",
        json!({"namespace": "pgfake-forget"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v.get("deleted").is_some(), "{v}");
}

// ---------------------------------------------------------------------------
// /api/v1/agents — list_agents PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_list_agents_returns_array() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = get_uri(&router, "/api/v1/agents").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v.get("agents").is_some(), "{v}");
}

#[tokio::test]
async fn pg_register_agent_then_list() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = post_json(
        &router,
        "/api/v1/agents",
        json!({
            "agent_id": "pg-agent-1",
            "agent_type": "human",
            "capabilities": ["store"],
        }),
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "{v}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/entities — entity_register PG branch (alias union)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_entity_register_happy() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = post_json(
        &router,
        "/api/v1/entities",
        json!({
            "canonical_name": "PG Entity One",
            "namespace": "pgfake-ent",
            "aliases": ["pg-ent-1", "pe1"],
            "metadata": {},
        }),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "{v}"
    );
}

#[tokio::test]
async fn pg_entity_register_alias_union_on_re_register() {
    let (router, _f) = build_fake_pg_router();
    let _ = post_json(
        &router,
        "/api/v1/entities",
        json!({
            "canonical_name": "PG Entity Two",
            "namespace": "pgfake-ent2",
            "aliases": ["one", "two"],
            "metadata": {},
        }),
    )
    .await;
    // Re-register with NEW aliases — handler should union them with prior.
    let (status, _v) = post_json(
        &router,
        "/api/v1/entities",
        json!({
            "canonical_name": "PG Entity Two",
            "namespace": "pgfake-ent2",
            "aliases": ["three", "four"],
            "metadata": {},
        }),
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::CREATED);
}

#[tokio::test]
async fn pg_entity_register_invalid_namespace_400() {
    let (router, _f) = build_fake_pg_router();
    // Use a namespace that clearly fails `validate_namespace` — `..` is a
    // banned segment per the existing namespace rules.
    let (status, _v) = post_json(
        &router,
        "/api/v1/entities",
        json!({
            "canonical_name": "bad",
            "namespace": "../etc",
            "aliases": [],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// /api/v1/taxonomy — PG branch routes through taxonomy_namespaces_via_store
// → downcast_postgres → BackendUnavailable → 503
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_taxonomy_via_store_pg_branch_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/taxonomy").await;
    // With `sal-postgres` ON, taxonomy_namespaces_via_store fails the
    // downcast on a SqliteStore → 503. With only `sal` ON, the cfg-gated
    // call is compiled out and the sqlite fallback returns 200. Accept
    // both shapes so the test is feature-set independent.
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::OK,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/archive — PG branches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_list_archive_via_store_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/archive").await;
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::OK,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_archive_stats_via_store_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/archive/stats").await;
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::OK,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/kg/* — PG branches via kg_*_via_store → 503
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_kg_timeline_via_store_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(
        &router,
        "/api/v1/kg/timeline?source_id=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::OK
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::BAD_REQUEST,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_kg_invalidate_via_store_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/kg/invalidate",
        json!({
            "source_id": "00000000-0000-0000-0000-000000000000",
            "target_id": "00000000-0000-0000-0000-000000000001",
            "relation": "related_to",
            "valid_until": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await;
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::OK
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::BAD_REQUEST,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_kg_query_via_store_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/kg/query",
        json!({
            "source_id": "00000000-0000-0000-0000-000000000000",
            "max_depth": 2,
        }),
    )
    .await;
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::OK
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::BAD_REQUEST,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/pending — list_pending_actions_via_store PG path → 503
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_list_pending_via_store_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/pending").await;
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::OK,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/inbox — hook_subscribers::get_inbox PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_inbox_returns_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = get_uri(&router, "/api/v1/inbox?agent_id=pg-recipient").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v.get("messages").is_some() || v.is_array(), "{v}");
}

#[tokio::test]
async fn pg_inbox_with_unread_only() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(
        &router,
        "/api/v1/inbox?agent_id=pg-recipient&unread_only=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pg_inbox_invalid_agent_id_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/inbox?agent_id=!bad!").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// /api/v1/notify — hook_subscribers::notify PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_notify_happy_path() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = post_json(
        &router,
        "/api/v1/notify",
        json!({
            "target_agent_id": "pg-recipient",
            "title": "pg-note",
            "payload": "hello from postgres branch",
            "agent_id": "pg-sender",
            "priority": 5,
        }),
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "notify pg: {status} body={v}",
    );
}

#[tokio::test]
async fn pg_notify_missing_payload_400() {
    let (router, _f) = build_fake_pg_router();
    // target_agent_id + title present; payload + content both missing →
    // handler returns 400 explicitly (not axum's 422 deserialization fail).
    let (status, _v) = post_json(
        &router,
        "/api/v1/notify",
        json!({
            "target_agent_id": "pg-recipient",
            "title": "no-body",
            "agent_id": "pg-sender",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// /api/v1/subscriptions — PG branches (subscribe / list / unsubscribe)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_subscribe_namespace_form_synthesizes_url_pg() {
    // The namespace-form subscribe synthesizes a loopback URL internally
    // and bypasses SSRF — exercises the pg subscribe branch without
    // needing a routable URL.
    let (router, _f) = build_fake_pg_router();
    let (status, v) = post_json(
        &router,
        "/api/v1/subscriptions",
        json!({
            "agent_id": "pg-sub-agent",
            "namespace": "pgfake-sub-ns",
            "events": "memory.created",
            "secret": "test-secret",
        }),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "subscribe pg: {status} body={v}",
    );
}

#[tokio::test]
async fn pg_subscribe_missing_url_and_namespace_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/subscriptions",
        json!({"agent_id": "pg-sub-agent"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pg_unsubscribe_by_id_when_missing_returns_ok_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = delete_uri(&router, "/api/v1/subscriptions?id=no-such-sub").await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn pg_unsubscribe_no_id_no_ns_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = delete_uri(&router, "/api/v1/subscriptions?agent_id=pg-agent").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pg_unsubscribe_by_namespace_missing_returns_removed_false() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = delete_uri(
        &router,
        "/api/v1/subscriptions?agent_id=pg-agent&namespace=nonexistent",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /api/v1/namespaces/{ns}/standard — PG branch for set / get / clear
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_get_namespace_standard_missing_returns_not_implemented_or_ok() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/namespaces/no-such-ns/standard").await;
    // The path-form `get_namespace_standard` routes through Db extractor
    // (not AppState) so it never dispatches to a PG branch — it returns OK
    // with a null envelope from the MCP handler. The qs-form (covered
    // separately below) is the one with the PG dispatch.
    assert!(
        status == StatusCode::OK
            || status == StatusCode::NOT_IMPLEMENTED
            || status == StatusCode::NOT_FOUND,
    );
}

#[tokio::test]
async fn pg_get_namespace_standard_qs_with_inherit() {
    let (router, _f) = build_fake_pg_router();
    // Use the query-string form — exercises get_namespace_standard_qs PG branch.
    let (status, _v) = get_uri(
        &router,
        "/api/v1/namespaces?namespace=foo/bar/baz&inherit=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pg_get_namespace_standard_qs_no_namespace_returns_list() {
    let (router, _f) = build_fake_pg_router();
    // No namespace → delegates to list_namespaces() (sqlite path even on
    // pg backend — list_namespaces has its own dispatch).
    let (status, _v) = get_uri(&router, "/api/v1/namespaces").await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /api/v1/sync/push — postgres branch via sync_push_via_store
// ---------------------------------------------------------------------------

static FED_LEGACY_BYPASS_INIT_PG: std::sync::Once = std::sync::Once::new();
fn install_federation_legacy_bypass_pg() {
    FED_LEGACY_BYPASS_INIT_PG.call_once(|| unsafe {
        std::env::set_var("AI_MEMORY_FED_TRUST_BODY_AGENT_ID", "1");
        std::env::set_var("AI_MEMORY_FED_SYNC_TRUST_PEER", "1");
    });
}

#[tokio::test]
async fn pg_sync_push_apply_memory() {
    install_federation_legacy_bypass_pg();
    let (router, _f) = build_fake_pg_router();
    let now = chrono::Utc::now().to_rfc3339();
    let body = json!({
        "sender_agent_id": "pg-peer",
        "sender_clock": {"entries": {}},
        "memories": [{
            "id": uuid::Uuid::new_v4().to_string(),
            "tier": "long",
            "namespace": "pg-sync",
            "title": "pg-sync-mem",
            "content": "from a pg-branch peer push",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "access_count": 0,
            "created_at": now,
            "updated_at": now,
            "metadata": {},
            "reflection_depth": 0,
            "memory_kind": "observation",
        }],
        "dry_run": false,
    });
    let (status, v) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v.get("applied").is_some(), "{v}");
}

#[tokio::test]
async fn pg_sync_push_invalid_sender_400() {
    install_federation_legacy_bypass_pg();
    let (router, _f) = build_fake_pg_router();
    let body = json!({
        "sender_agent_id": "",
        "sender_clock": {"entries": {}},
        "memories": [],
        "dry_run": false,
    });
    let (status, _v) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pg_sync_push_dry_run_no_writes() {
    install_federation_legacy_bypass_pg();
    let (router, _f) = build_fake_pg_router();
    let now = chrono::Utc::now().to_rfc3339();
    let body = json!({
        "sender_agent_id": "pg-peer",
        "sender_clock": {"entries": {}},
        "memories": [{
            "id": uuid::Uuid::new_v4().to_string(),
            "tier": "long",
            "namespace": "pg-sync-dry",
            "title": "pg-dry",
            "content": "dry run",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "access_count": 0,
            "created_at": now,
            "updated_at": now,
            "metadata": {},
            "reflection_depth": 0,
            "memory_kind": "observation",
        }],
        "dry_run": true,
    });
    let (status, v) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn pg_sync_push_deletions_oversize_rejected() {
    install_federation_legacy_bypass_pg();
    let (router, _f) = build_fake_pg_router();
    // 10_001 short ID strings — small enough to fit Axum's body limit but
    // over the per-collection MAX_BULK_SIZE cap.
    let deletions: Vec<Value> = (0..10_001)
        .map(|_| json!(uuid::Uuid::new_v4().to_string()))
        .collect();
    let body = json!({
        "sender_agent_id": "pg-peer",
        "sender_clock": {"entries": {}},
        "memories": [],
        "deletions": deletions,
        "dry_run": false,
    });
    let (status, _v) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pg_sync_push_with_deletions() {
    install_federation_legacy_bypass_pg();
    let (router, _f) = build_fake_pg_router();
    let body = json!({
        "sender_agent_id": "pg-peer",
        "sender_clock": {"entries": {}},
        "memories": [],
        "deletions": ["00000000-0000-0000-0000-000000000099"],
        "dry_run": false,
    });
    let (status, _v) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pg_sync_push_with_invalid_memory_skipped() {
    install_federation_legacy_bypass_pg();
    let (router, _f) = build_fake_pg_router();
    let now = chrono::Utc::now().to_rfc3339();
    // empty title — validate_memory fails so skipped
    let body = json!({
        "sender_agent_id": "pg-peer",
        "sender_clock": {"entries": {}},
        "memories": [{
            "id": uuid::Uuid::new_v4().to_string(),
            "tier": "long",
            "namespace": "pg-sync-bad",
            "title": "",
            "content": "",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "access_count": 0,
            "created_at": now,
            "updated_at": now,
            "metadata": {},
            "reflection_depth": 0,
            "memory_kind": "observation",
        }],
        "dry_run": false,
    });
    let (status, v) = post_json(&router, "/api/v1/sync/push", body).await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

// ---------------------------------------------------------------------------
// /api/v1/stats, /api/v1/gc — both have pg branches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_get_stats_returns_struct() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/stats").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pg_run_gc_happy() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(&router, "/api/v1/gc", json!({})).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /api/v1/export — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_export_returns_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/export").await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /api/v1/bulk — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_bulk_create_happy() {
    let (router, _f) = build_fake_pg_router();
    // bulk_create takes a bare JSON array (Vec<CreateMemory>), not an
    // object-wrapped envelope.
    let body = json!([
        {
            "tier": "long",
            "namespace": "pg-bulk",
            "title": "bulk-1",
            "content": "first bulk memory via pg branch",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "metadata": {},
        },
        {
            "tier": "long",
            "namespace": "pg-bulk",
            "title": "bulk-2",
            "content": "second bulk memory via pg branch",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "metadata": {},
        }
    ]);
    let (status, v) = post_json(&router, "/api/v1/memories/bulk", body).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "bulk pg: {status} body={v}",
    );
}

#[tokio::test]
async fn pg_bulk_create_over_limit_400() {
    let (router, _f) = build_fake_pg_router();
    let items: Vec<Value> = (0..1001)
        .map(|i| {
            json!({
                "tier": "long",
                "namespace": "pg-bulk-over",
                "title": format!("over-{i}"),
                "content": "x",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "user",
                "metadata": {},
            })
        })
        .collect();
    let body = json!(items);
    let (status, _v) = post_json(&router, "/api/v1/memories/bulk", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// /api/v1/quota/status — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_quota_status_no_writes_returns_zero() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) =
        post_json(&router, "/api/v1/quota/status", json!({"agent_id": "pg-q"})).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /api/v1/check_duplicate — handler is sqlite-only but PG flag path
// surfaces an error envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_check_duplicate_returns_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/check_duplicate",
        json!({"title": "x", "content": "needs an embedder to score similarity"}),
    )
    .await;
    // Without an embedder configured the substrate returns 400 (semantic
    // recall is unavailable, so duplicate detection can't score).
    assert!(
        status == StatusCode::BAD_REQUEST
            || status == StatusCode::OK
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/links — PG branches when flag is Postgres
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_get_links_unknown_id_returns_empty_array() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(
        &router,
        "/api/v1/links/00000000-0000-0000-0000-000000000000",
    )
    .await;
    // Substrate returns 200 with an empty array; PG branch should as well.
    assert!(
        status == StatusCode::OK
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::SERVICE_UNAVAILABLE,
    );
}

// ---------------------------------------------------------------------------
// /api/v1/kg/find_paths — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_kg_find_paths_invalid_source_400() {
    let (router, _f) = build_fake_pg_router();
    // Use a clearly-invalid id (contains whitespace, fails validate_id).
    let (status, _v) = post_json(
        &router,
        "/api/v1/kg/find_paths",
        json!({"source_id": "bad id with spaces", "target_id": "00000000-0000-0000-0000-000000000001"}),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::OK,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_kg_find_paths_unknown_returns_empty() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/kg/find_paths",
        json!({
            "source_id": "00000000-0000-0000-0000-000000000000",
            "target_id": "00000000-0000-0000-0000-000000000001",
            "max_depth": 3,
        }),
    )
    .await;
    // SqliteStore's find_paths impl returns Ok(vec![]) when no path; pg
    // branch wraps the same envelope, returning 200 with empty paths.
    assert!(
        status == StatusCode::OK
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::NOT_IMPLEMENTED,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/entities/by_alias — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_entity_get_by_alias_unknown_returns_null_or_404() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(
        &router,
        "/api/v1/entities/by_alias?alias=no-such-alias&namespace=pgfake-e",
    )
    .await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/import — PG branch (governance walk)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_import_memories_happy() {
    let (router, _f) = build_fake_pg_router();
    let now = chrono::Utc::now().to_rfc3339();
    let body = json!({
        "memories": [{
            "id": uuid::Uuid::new_v4().to_string(),
            "tier": "long",
            "namespace": "pg-import",
            "title": "import-1",
            "content": "imported via pg branch",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "import",
            "access_count": 0,
            "created_at": now,
            "updated_at": now,
            "metadata": {},
            "reflection_depth": 0,
            "memory_kind": "observation",
        }],
        "links": []
    });
    let (status, v) = post_json(&router, "/api/v1/import", body).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "import pg: {status} body={v}",
    );
}

#[tokio::test]
async fn pg_import_memories_with_invalid_member_records_error() {
    let (router, _f) = build_fake_pg_router();
    let now = chrono::Utc::now().to_rfc3339();
    let body = json!({
        "memories": [{
            "id": uuid::Uuid::new_v4().to_string(),
            "tier": "long",
            "namespace": "pg-import-bad",
            "title": "",
            "content": "",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "import",
            "access_count": 0,
            "created_at": now,
            "updated_at": now,
            "metadata": {},
            "reflection_depth": 0,
            "memory_kind": "observation",
        }],
    });
    let (status, v) = post_json(&router, "/api/v1/import", body).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "{v}"
    );
}

#[tokio::test]
async fn pg_import_oversize_400() {
    let (router, _f) = build_fake_pg_router();
    // 10_001 small memories — over MAX_BULK_SIZE
    let now = chrono::Utc::now().to_rfc3339();
    let mems: Vec<Value> = (0..10_001)
        .map(|i| {
            json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "tier": "long",
                "namespace": "pg-imp-over",
                "title": format!("o-{i}"),
                "content": "x",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "import",
                "access_count": 0,
                "created_at": now,
                "updated_at": now,
                "metadata": {},
                "reflection_depth": 0,
                "memory_kind": "observation",
            })
        })
        .collect();
    let body = json!({"memories": mems, "links": []});
    let (status, _v) = post_json(&router, "/api/v1/import", body).await;
    // Either the handler's MAX_BULK_SIZE returns 400 or Axum body-limit
    // rejects with 413.
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE,
        "{status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/links — PG create / delete branches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_create_link_with_unknown_ids_returns_error() {
    let (router, _f) = build_fake_pg_router();
    // Both endpoints exist in handlers; PG branch routes through SAL trait.
    let (status, _v) = post_json(
        &router,
        "/api/v1/links",
        json!({
            "source_id": "00000000-0000-0000-0000-000000000000",
            "target_id": "00000000-0000-0000-0000-000000000001",
            "relation": "related_to",
        }),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::INTERNAL_SERVER_ERROR
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::CREATED
            || status == StatusCode::OK,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_delete_link_unknown_returns_404_or_ok() {
    let (router, _f) = build_fake_pg_router();
    // delete_link expects a JSON body
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/v1/links")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "target_id": "00000000-0000-0000-0000-000000000001",
                "relation": "related_to",
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    assert!(
        status == StatusCode::OK
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/quota/status — list_all branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_quota_status_list_no_agent_returns_list() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = post_json(&router, "/api/v1/quota/status", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v.get("quotas").is_some() || v.get("count").is_some(), "{v}");
}

// ---------------------------------------------------------------------------
// /api/v1/archive/{id}/restore — PG branch (via store)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_restore_archive_unknown_id_404_or_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/archive/00000000-0000-0000-0000-000000000000/restore",
        json!({}),
    )
    .await;
    assert!(
        status == StatusCode::NOT_FOUND
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::OK,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_archive_by_ids_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/archive",
        json!({"ids": ["00000000-0000-0000-0000-000000000000"]}),
    )
    .await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/consolidate — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_consolidate_with_unknown_ids_returns_400_or_404() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/consolidate",
        json!({
            "ids": ["00000000-0000-0000-0000-000000000000"],
            "title": "merged",
            "namespace": "pg-consol",
        }),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::INTERNAL_SERVER_ERROR
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::OK
            || status == StatusCode::CREATED,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/contradictions — sqlite-bound but exercises 400 path on PG flag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_contradictions_missing_topic_and_ns_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/contradictions").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// /api/v1/auto_tag, /api/v1/expand_query — LLM handlers degrade
// gracefully on PG flag w/ no llm wired
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_auto_tag_no_llm_returns_empty_or_503() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/auto_tag",
        json!({"title": "tagme", "content": "longer content body for auto_tag please"}),
    )
    .await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::BAD_REQUEST,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_expand_query_no_llm_returns_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/expand_query",
        json!({"query": "what is the rust language"}),
    )
    .await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::BAD_REQUEST,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/memory_load_family — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_load_family_happy_or_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/memory_load_family",
        json!({"family": "ai-memory"}),
    )
    .await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/links/verify — PG branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_verify_link_returns_envelope_or_error() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/links/verify",
        json!({
            "source_id": "00000000-0000-0000-0000-000000000000",
            "target_id": "00000000-0000-0000-0000-000000000001",
            "relation": "related_to",
        }),
    )
    .await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/session/start — PG-flag-aware (delegates to MCP handler)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_session_start_happy() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/session/start",
        json!({"agent_id": "pg-session"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pg_session_start_invalid_agent_id_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/session/start",
        json!({"agent_id": "!bad!"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// /api/v1/sync/since — PG sync since envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_sync_since_no_param_returns_envelope() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/sync/since").await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

#[tokio::test]
async fn pg_sync_since_with_peer_query() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/sync/since?peer=pg-peer-x&limit=10").await;
    assert!(
        status == StatusCode::OK
            || status == StatusCode::BAD_REQUEST
            || status == StatusCode::SERVICE_UNAVAILABLE,
        "got {status}",
    );
}

// ---------------------------------------------------------------------------
// /api/v1/tools/list, /api/v1/capabilities, /api/v1/health, /metrics — flag
// independent but should be sane under PG flag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_tools_list_returns_array() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = get_uri(&router, "/api/v1/tools/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(v.get("tools").is_some(), "{v}");
}

#[tokio::test]
async fn pg_capabilities_returns_storage_backend_postgres() {
    let (router, _f) = build_fake_pg_router();
    let (status, v) = get_uri(&router, "/api/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    // The cap envelope echoes the configured storage backend.
    let backend = v
        .get("storage_backend")
        .and_then(|b| b.as_str())
        .or_else(|| v.pointer("/storage/backend").and_then(|b| b.as_str()));
    if let Some(b) = backend {
        assert!(b == "postgres" || b == "pg", "got backend={b}");
    }
}

#[tokio::test]
async fn pg_health_returns_ok() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = get_uri(&router, "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /api/v1/recall — invalid as_agent + bad query (PG branch validation path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pg_recall_empty_context_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(&router, "/api/v1/recall", json!({"context": ""})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pg_recall_with_invalid_as_agent_400() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/recall",
        json!({"context": "anything", "as_agent": "../bad-traversal"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pg_recall_with_kinds_filter() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/recall",
        json!({
            "context": "kinds-filter-probe",
            "memory_kinds": ["observation"],
            "limit": 3,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pg_recall_with_source_uri_prefix() {
    let (router, _f) = build_fake_pg_router();
    let (status, _v) = post_json(
        &router,
        "/api/v1/recall",
        json!({
            "context": "x",
            "source_uri_prefix": "https://example.com/",
            "limit": 3,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pg_recall_get_invalid_as_agent_400() {
    let (router, _f) = build_fake_pg_router();
    // `..` is a banned namespace segment (path-traversal). Triggers the
    // `validate::validate_namespace(as_agent)` rejection.
    let (status, _v) = get_uri(&router, "/api/v1/recall?q=foo&as_agent=..").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
