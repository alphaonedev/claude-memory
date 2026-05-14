// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//! v0.7.0 Round-2 F9 — HTTP missing-required-field returns 400, not 422.
//!
//! Spec asks for 400 + a sanitized field-name hint. Pre-F9 the daemon
//! returned axum's default 422 UNPROCESSABLE_ENTITY with the raw serde
//! diagnostic ("Failed to deserialize the JSON body... missing field
//! `content` at line 1 column 14") which forced clients into substring
//! matching on a non-stable message. F9 lands a custom JSON extractor
//! (`JsonOrBadRequest`) that maps every rejection variant to:
//!
//! ```json
//! { "error": "missing required field: <name>", "fields": ["<name>", ...] }
//! ```
//!
//! This file pins the wire contract.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};

fn build_test_router() -> (axum::Router, NamedTempFile) {
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
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

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

async fn post(router: &axum::Router, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

#[tokio::test]
async fn http_post_memories_missing_content_returns_400_with_fields() {
    // F9 acceptance test: missing `content` must surface 400 + a
    // structured envelope listing `content` in `fields`.
    let (router, _keep) = build_test_router();
    let body = json!({
        "tier": "long",
        "namespace": "round2-f9",
        "title": "no content"
        // content INTENTIONALLY OMITTED — required field on CreateMemory.
    });

    let (status, payload) = post(&router, body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "F9: must be 400, NOT axum's default 422 UNPROCESSABLE_ENTITY (got {status})"
    );

    let err = payload
        .get("error")
        .and_then(|v| v.as_str())
        .expect("F9: response body must include `error`");
    assert!(
        err.to_lowercase().contains("content")
            || err.to_lowercase().contains("missing required field"),
        "F9: error message should hint at the missing field (got {err:?})"
    );

    let fields = payload
        .get("fields")
        .and_then(|v| v.as_array())
        .expect("F9: response body must include `fields` array");
    assert!(
        fields.iter().any(|v| v.as_str() == Some("content")),
        "F9: `fields` must include the missing field name `content` (got {fields:?})"
    );
}

#[tokio::test]
async fn http_post_memories_missing_title_returns_400_with_fields() {
    // The required-field set is (title, content). Missing `title`
    // exercises the same custom-extractor path; pin it explicitly so
    // a future schema change that drops the field still trips.
    let (router, _keep) = build_test_router();
    let body = json!({
        "tier": "long",
        "namespace": "round2-f9",
        "content": "no title"
    });

    let (status, payload) = post(&router, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let fields = payload
        .get("fields")
        .and_then(|v| v.as_array())
        .expect("F9: `fields` array");
    assert!(
        fields.iter().any(|v| v.as_str() == Some("title")),
        "F9: missing-title must surface `title` in the `fields` array (got {fields:?})"
    );
}

#[tokio::test]
async fn http_post_memories_malformed_body_returns_400() {
    // F9: a body that is not even syntactically valid JSON must
    // also surface 400 (axum's default for syntax error is already
    // 400, but we map through the same extractor so the body shape
    // stays consistent — `error` field present, `fields` array
    // present (possibly empty)).
    let (router, _keep) = build_test_router();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        .body(Body::from(b"this is not json {{{".to_vec()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert!(
        payload.get("error").and_then(|v| v.as_str()).is_some(),
        "F9: malformed-JSON branch must still produce a sanitized `error` field"
    );
    assert!(
        payload.get("fields").and_then(|v| v.as_array()).is_some(),
        "F9: malformed-JSON branch must still produce a `fields` array (may be empty)"
    );
}
