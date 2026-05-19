// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 issue #897 — Coverage-regression closure on the post-Wave-1-
//! split `src/handlers/http.rs` shim.
//!
//! Background. Wave 1 (#650) decomposed the ~17 800-LOC monolithic
//! `src/handlers.rs` into 26 sibling files under `src/handlers/`. The
//! residual `src/handlers/http.rs` (323 LOC at landing time) is a thin
//! holder for the LLM autonomy hooks the create-memory path consumes:
//! `maybe_auto_tag` (L5), the staged-in `maybe_detect_conflicts` (issue
//! #519), and the `fetch_namespace_candidates` helper. The historic
//! coverage threshold of 42 was set against the pre-split file shape;
//! against the new shim the same threshold measured 14.71 %.
//!
//! This file is the Path-A test addition: end-to-end HTTP traversal of
//! the post-split shim through `POST /api/v1/memories`, which calls
//! `maybe_auto_tag` on every store request. Companion lib-tier tests
//! that directly exercise the private gate ladders in
//! `maybe_detect_conflicts` and `fetch_namespace_candidates` live in a
//! `#[cfg(test)] mod cov897_tests` block at the bottom of
//! `src/handlers/http.rs` (those helpers are module-private and not
//! reachable from this integration-test crate). Together the two
//! files restore measured coverage above the 42-threshold floor.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};

const LONG_CONTENT: &str = "This memory body is comfortably above the AUTO_TAG_MIN_CONTENT_LEN \
     50-character threshold so the maybe_auto_tag gate ladder must \
     traverse past the content-length check.";

fn build_test_router(tier: FeatureTier) -> (axum::Router, NamedTempFile) {
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
        tier_config: Arc::new(tier.config()),
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

async fn post_create(router: &axum::Router, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        .header("x-agent-id", "ai:cov897-test")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

// ---- HTTP-level traversal of `maybe_auto_tag` gate ladder --------------

/// Keyword tier (`llm_model = None`) — gate ladder line 107-108 must
/// short-circuit even when content is long and tags are empty. The
/// create path still returns 201 + the operator's (empty) tags.
#[tokio::test]
async fn cov897_http_create_keyword_tier_skips_auto_tag() {
    let (router, _f) = build_test_router(FeatureTier::Keyword);
    let body = json!({
        "tier": "long",
        "namespace": "cov897-kw",
        "title": "keyword-tier store",
        "content": LONG_CONTENT,
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {}
    });
    let (status, payload) = post_create(&router, &body).await;
    assert_eq!(status, StatusCode::CREATED, "{payload}");
    // No auto_tags field — keyword tier short-circuits before the LLM
    // path emits any.
    assert!(
        payload.get("auto_tags").is_none(),
        "keyword tier emits no auto_tags, got {payload}"
    );
}

/// Smart tier with `llm = Arc::new(None)` — gate ladder reaches line
/// 110-113 (the `llm_arc.is_none()` short-circuit) instead of stopping
/// at the tier check. End-to-end the create path still returns 201.
#[tokio::test]
async fn cov897_http_create_smart_tier_no_llm_arc_succeeds() {
    let (router, _f) = build_test_router(FeatureTier::Smart);
    let body = json!({
        "tier": "long",
        "namespace": "cov897-smart-no-llm",
        "title": "smart-tier no-llm store",
        "content": LONG_CONTENT,
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {}
    });
    let (status, payload) = post_create(&router, &body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Smart-tier no-llm path must still return 201, got {payload}"
    );
}

/// Operator-supplied tags short-circuit `maybe_auto_tag` at line 98-100
/// — the LLM path never executes and the response envelope must NOT
/// carry an `auto_tags` field (it's only inserted when the hook
/// actually returned tags).
#[tokio::test]
async fn cov897_http_create_with_operator_tags_skips_auto_tag() {
    let (router, _f) = build_test_router(FeatureTier::Smart);
    let body = json!({
        "tier": "long",
        "namespace": "cov897-op-tags",
        "title": "op-tags store",
        "content": LONG_CONTENT,
        "tags": ["op-tag-a", "op-tag-b"],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {}
    });
    let (status, payload) = post_create(&router, &body).await;
    assert_eq!(status, StatusCode::CREATED, "{payload}");
    // Operator-tags branch of the gate returns `Vec::new()` from
    // `maybe_auto_tag` BEFORE any LLM call, so the response envelope
    // must omit the `auto_tags` field entirely.
    assert!(
        payload.get("auto_tags").is_none(),
        "operator-tags branch must skip auto_tags response field, got {payload}"
    );
}

/// Short-content gate at line 101-103 — content below
/// `AUTO_TAG_MIN_CONTENT_LEN` (50 chars) must skip auto-tag. End-to-end
/// the create still returns 201.
#[tokio::test]
async fn cov897_http_create_short_content_skips_auto_tag() {
    let (router, _f) = build_test_router(FeatureTier::Smart);
    let body = json!({
        "tier": "long",
        "namespace": "cov897-short",
        "title": "short-content store",
        "content": "too short for auto-tag",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {}
    });
    let (status, payload) = post_create(&router, &body).await;
    assert_eq!(status, StatusCode::CREATED, "{payload}");
}

/// Internal-namespace gate at line 104-106 — namespaces starting with
/// `_` must skip auto-tag (matches MCP `handle_store` skip at
/// `src/mcp.rs:1818`).
#[tokio::test]
async fn cov897_http_create_internal_namespace_skips_auto_tag() {
    let (router, _f) = build_test_router(FeatureTier::Smart);
    let body = json!({
        "tier": "long",
        "namespace": "_cov897-internal",
        "title": "internal-namespace store",
        "content": LONG_CONTENT,
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {}
    });
    let (status, payload) = post_create(&router, &body).await;
    assert_eq!(status, StatusCode::CREATED, "{payload}");
}
