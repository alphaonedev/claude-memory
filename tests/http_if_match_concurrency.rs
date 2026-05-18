// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 Provenance Gap 1 (issue #884) — HTTP `If-Match: <version>`
//! header → 409 CONFLICT envelope end-to-end coverage.
//!
//! Pins the wire shape the substrate documents in CLAUDE.md and the
//! gap-1 release notes: a `PUT /api/v1/memories/:id` carrying
//! `If-Match: <version>` MUST refuse the mutation with a 409 status +
//! a structured JSON envelope naming both the expected + current
//! versions so the caller can re-read and retry. When the header is
//! absent (legacy v0.6.x callers) the mutation lands without any
//! gate, preserving back-compat.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};
use ai_memory::models::{Memory, Tier};

/// Mirror of the build helper used by every other HTTP integration
/// test (e.g. `tests/round2_f9_http_400.rs`). Stands up a router with
/// the keyword tier (no embedder, no federation) so the only moving
/// parts are the JSON extractor + storage layer.
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

/// Seed a single memory directly through the substrate so the test
/// has a stable id + version=1 starting point.
fn seed(path: &std::path::Path, title: &str) -> String {
    let conn = ai_memory::db::open(path).expect("reopen for seed");
    let now = chrono::Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        content: "v1 body".to_string(),
        namespace: "ifmatch-test".to_string(),
        tier: Tier::Mid,
        created_at: now.clone(),
        updated_at: now,
        ..Default::default()
    };
    ai_memory::db::insert(&conn, &mem).expect("insert")
}

async fn put_with_if_match(
    router: &axum::Router,
    id: &str,
    if_match: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("PUT")
        .uri(format!("/api/v1/memories/{id}"))
        .header("content-type", "application/json");
    if let Some(v) = if_match {
        req = req.header("if-match", v);
    }
    let req = req
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

#[tokio::test]
async fn http_put_with_matching_if_match_succeeds() {
    let (router, file) = build_test_router();
    let id = seed(file.path(), "match-success");
    // Baseline version is 1 (newly inserted row).
    let (status, body) = put_with_if_match(
        &router,
        &id,
        Some("1"),
        json!({"content": "v2 body via match"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "matching If-Match must produce 200, got {status}: {body}"
    );
    assert_eq!(body["content"].as_str(), Some("v2 body via match"));
    assert_eq!(body["version"].as_i64(), Some(2), "version bumped");
}

#[tokio::test]
async fn http_put_with_stale_if_match_returns_409_with_envelope() {
    let (router, file) = build_test_router();
    let id = seed(file.path(), "stale-conflict");
    // First write lands at version=2.
    let _ = put_with_if_match(
        &router,
        &id,
        Some("1"),
        json!({"content": "winner from caller A"}),
    )
    .await;
    // Second caller still believes the row is at version=1.
    let (status, body) = put_with_if_match(
        &router,
        &id,
        Some("1"),
        json!({"content": "loser from caller B"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "stale If-Match must produce 409, got {status}: {body}"
    );
    assert_eq!(body["status"].as_str(), Some("conflict"));
    assert_eq!(body["id"].as_str(), Some(id.as_str()));
    assert_eq!(body["expected_version"].as_i64(), Some(1));
    assert_eq!(
        body["current_version"].as_i64(),
        Some(2),
        "envelope must name the current stored version so caller can re-read + retry"
    );
}

#[tokio::test]
async fn http_put_without_if_match_preserves_legacy_last_write_wins() {
    let (router, file) = build_test_router();
    let id = seed(file.path(), "no-header");
    // Two updates without `If-Match` both succeed — the gate is
    // strictly opt-in. The version column still advances internally so
    // a later If-Match caller will see the latest value.
    let (a, _) = put_with_if_match(&router, &id, None, json!({"content": "a"})).await;
    let (b, _) = put_with_if_match(&router, &id, None, json!({"content": "b"})).await;
    assert_eq!(a, StatusCode::OK, "first update: {a}");
    assert_eq!(b, StatusCode::OK, "second update: {b}");
}

#[tokio::test]
async fn http_put_with_quoted_if_match_etag_style_value_parses() {
    // The header value may arrive ETag-style with surrounding quotes
    // (`If-Match: "1"`). The handler trims them before parsing the
    // int — pin that behaviour so a future strict-parser refactor is
    // loud.
    let (router, file) = build_test_router();
    let id = seed(file.path(), "etag-quoted");
    let (status, body) = put_with_if_match(
        &router,
        &id,
        Some("\"1\""),
        json!({"content": "etag-quoted body"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "quoted If-Match value should parse: {status}: {body}"
    );
}

#[tokio::test]
async fn http_put_with_unparseable_if_match_falls_through_to_legacy() {
    // If the header is present but the value is not a valid integer,
    // the gate is silently skipped (treated as None) and the update
    // succeeds. The contract is "opt-in, integer-only" — a malformed
    // header should not fail-closed and brick the caller.
    let (router, file) = build_test_router();
    let id = seed(file.path(), "bogus-header");
    let (status, _) = put_with_if_match(
        &router,
        &id,
        Some("not-an-integer"),
        json!({"content": "fallback"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unparseable If-Match value falls through to legacy path"
    );
}
