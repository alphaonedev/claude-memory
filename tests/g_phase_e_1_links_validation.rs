// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural
// impact on the regression we pin.
#![allow(clippy::redundant_closure_for_method_calls)]

//! v0.7.0 G-PHASE-E-1 (issue #706) — `/api/v1/links` validation hardening.
//!
//! Two regressions pinned:
//!
//! 1. Unknown body fields (e.g. the common-typo `link_type`) used to be
//!    silently dropped — `relation` would default to `related_to` and
//!    the response said `linked: true`. Now the handler rejects with a
//!    structured 400 `{"error":"unknown_field","fields":[...]}`.
//!
//! 2. A `relation` outside the canonical CHECK-enforced closed set
//!    (e.g. `bogus_relation`) used to surface as a generic HTTP 500
//!    `"internal server error"` because the SQL CHECK constraint
//!    failure bubbled past the validate-link gate (which accepts any
//!    `[a-z0-9_]+` for forward-compat). Now the handler pre-flights
//!    the relation against the closed set and returns 400
//!    `{"error":"invalid_relation","got":...,"allowed":[...]}`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt as _;

/// Build the router from a fresh in-memory DB so each test starts
/// clean and there's no inter-test leakage on the `memory_links` table.
fn build_router_with_db() -> (axum::Router, ai_memory::handlers::Db) {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let path = std::path::PathBuf::from(":memory:");
    let db: ai_memory::handlers::Db = std::sync::Arc::new(tokio::sync::Mutex::new((
        conn,
        path,
        ai_memory::config::ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: std::sync::Arc<dyn ai_memory::store::MemoryStore> = {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile for SqliteStore");
        let p = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        std::sync::Arc::new(
            ai_memory::store::sqlite::SqliteStore::open(&p).expect("open SqliteStore"),
        )
    };
    let app_state = ai_memory::handlers::AppState {
        db: db.clone(),
        embedder: std::sync::Arc::new(None),
        vector_index: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        federation: std::sync::Arc::new(None),
        tier_config: std::sync::Arc::new(ai_memory::config::FeatureTier::Keyword.config()),
        scoring: std::sync::Arc::new(ai_memory::config::ResolvedScoring::default()),
        profile: std::sync::Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: std::sync::Arc::new(None),
        active_keypair: std::sync::Arc::new(None),
        family_embeddings: std::sync::Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: std::sync::Arc::new(None),
        auto_tag_model: std::sync::Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: std::sync::Arc::new(None),
        deferred_audit_queue: std::sync::Arc::new(None),
    };
    let api_key_state = ai_memory::handlers::ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, db)
}

/// Insert two memories so a link can reference them. Returns
/// `(src_id, tgt_id)`.
async fn seed_two_memories(db: &ai_memory::handlers::Db) -> (String, String) {
    let lock = db.lock().await;
    let now = chrono::Utc::now().to_rfc3339();
    let src = ai_memory::models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Long,
        namespace: "phase-e-1".into(),
        title: "src".into(),
        content: "src body".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let tgt = ai_memory::models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Long,
        namespace: "phase-e-1".into(),
        title: "tgt".into(),
        content: "tgt body".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let src_id = ai_memory::db::insert(&lock.0, &src).expect("insert src");
    let tgt_id = ai_memory::db::insert(&lock.0, &tgt).expect("insert tgt");
    drop(lock);
    (src_id, tgt_id)
}

async fn post_links(
    router: axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/links")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

#[tokio::test]
async fn unknown_field_link_type_rejected_with_structured_400() {
    let (router, db) = build_router_with_db();
    let (src, tgt) = seed_two_memories(&db).await;
    let (status, body) = post_links(
        router,
        json!({
            "source_id": src,
            "target_id": tgt,
            "link_type": "reflects_on",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown field must produce 400; body={body}",
    );
    assert_eq!(body["error"], "unknown_field", "body={body}");
    let fields = body["fields"].as_array().expect("fields array");
    assert!(
        fields.iter().any(|f| f.as_str() == Some("link_type")),
        "fields must list link_type; got {fields:?}",
    );
}

#[tokio::test]
async fn unknown_field_rejected_lists_multiple_unknown_fields_sorted() {
    let (router, _db) = build_router_with_db();
    // Empty body avoids needing seeded memories — the unknown-field
    // gate runs first.
    let (status, body) = post_links(
        router,
        json!({
            "zzz_extra": 1,
            "aaa_extra": 2,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["error"], "unknown_field", "body={body}");
    let fields: Vec<&str> = body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // Deterministic sort: alphabetical.
    assert_eq!(fields, vec!["aaa_extra", "zzz_extra"], "body={body}");
}

#[tokio::test]
async fn invalid_relation_rejected_with_structured_400() {
    let (router, db) = build_router_with_db();
    let (src, tgt) = seed_two_memories(&db).await;
    let (status, body) = post_links(
        router,
        json!({
            "source_id": src,
            "target_id": tgt,
            "relation": "bogus_relation",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "invalid relation must produce 400 (not 500); body={body}",
    );
    assert_eq!(body["error"], "invalid_relation", "body={body}");
    assert_eq!(body["got"], "bogus_relation", "body={body}");
    let allowed: Vec<&str> = body["allowed"]
        .as_array()
        .expect("allowed array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(allowed.contains(&"related_to"));
    assert!(allowed.contains(&"supersedes"));
    assert!(allowed.contains(&"contradicts"));
    assert!(allowed.contains(&"derived_from"));
    assert!(allowed.contains(&"reflects_on"));
}

#[tokio::test]
async fn canonical_relation_still_accepted_post_fix() {
    let (router, db) = build_router_with_db();
    let (src, tgt) = seed_two_memories(&db).await;
    let (status, body) = post_links(
        router,
        json!({
            "source_id": src,
            "target_id": tgt,
            "relation": "reflects_on",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={body}");
    assert_eq!(body["linked"], true, "body={body}");
}

#[tokio::test]
async fn s82_aliases_still_accepted_post_fix() {
    let (router, db) = build_router_with_db();
    let (src, tgt) = seed_two_memories(&db).await;
    let (status, body) = post_links(
        router,
        json!({
            "from": src,
            "to": tgt,
            "rel_type": "supersedes",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={body}");
    assert_eq!(body["linked"], true, "body={body}");
}
