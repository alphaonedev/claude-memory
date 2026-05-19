// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #910 — scope=private visibility filter regression on
//! `GET /api/v1/memories` (`list_memories`).
//!
//! Pre-#910 the HTTP `GET /api/v1/memories` handler returned every row
//! matching the requested namespace/tier filter regardless of
//! `metadata.scope`. A caller authenticated as `bob` could enumerate
//! `alice`'s scope=private rows by listing the namespace. The fix
//! adds an in-process post-filter (correctness-equivalent to the
//! `visibility_clause` used by the search + recall paths) that drops
//! rows where `metadata.scope=="private"` AND the row's
//! `metadata.agent_id` is not the caller.
//!
//! These tests pin the contract:
//!
//! 1. `bob_cannot_list_alice_private_rows_910` — alice stores a row
//!    with scope=private; bob's list call returns count=0.
//! 2. `owner_can_list_own_private_rows_910` — alice's list call returns
//!    the same row (owner exemption).
//! 3. `collective_scope_visible_cross_tenant_910` — alice stores a row
//!    with scope=collective; bob can see it. Sanity-check that the
//!    filter is precise (only `private` is dropped, NOT all scopes).

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

fn build_router_fixture_with_seed(
    seed_scope: &str,
    seed_owner: &str,
    seed_namespace: &str,
) -> (axum::Router, NamedTempFile) {
    let f = NamedTempFile::new().expect("tempfile");
    let db_path = f.path().to_path_buf();
    let conn = ai_memory::db::open(&db_path).expect("db::open");
    let now = chrono::Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: seed_namespace.to_string(),
        title: "scoped-row".to_string(),
        content: format!("body for {seed_scope}/{seed_owner}"),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": seed_owner, "scope": seed_scope}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    ai_memory::db::insert(&conn, &mem).expect("insert seed");

    let conn2 = ai_memory::db::open(&db_path).expect("reopen for AppState");
    let db: Db = Arc::new(Mutex::new((
        conn2,
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

async fn list_as(router: &axum::Router, ns: &str, caller: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/memories?namespace={ns}"))
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
async fn bob_cannot_list_alice_private_rows_910() {
    let (router, _f) = build_router_fixture_with_seed("private", "alice", "shared-ns-910");
    let (status, body) = list_as(&router, "shared-ns-910", "bob").await;
    assert_eq!(status, StatusCode::OK, "got {status} body={body}");
    let count = body["count"].as_u64().unwrap_or(99);
    assert_eq!(
        count, 0,
        "#910: bob must NOT see alice's scope=private row in shared namespace, got body={body}"
    );
}

#[tokio::test]
async fn owner_can_list_own_private_rows_910() {
    let (router, _f) = build_router_fixture_with_seed("private", "alice", "shared-ns-910b");
    let (status, body) = list_as(&router, "shared-ns-910b", "alice").await;
    assert_eq!(status, StatusCode::OK, "got {status} body={body}");
    let count = body["count"].as_u64().unwrap_or(0);
    assert_eq!(
        count, 1,
        "#910 owner-exemption: alice MUST see her own scope=private row, got body={body}"
    );
}

#[tokio::test]
async fn collective_scope_visible_cross_tenant_910() {
    let (router, _f) = build_router_fixture_with_seed("collective", "alice", "shared-ns-910c");
    let (status, body) = list_as(&router, "shared-ns-910c", "bob").await;
    assert_eq!(status, StatusCode::OK, "got {status} body={body}");
    let count = body["count"].as_u64().unwrap_or(0);
    assert_eq!(
        count, 1,
        "#910 precision: scope=collective rows MUST be visible cross-tenant, got body={body}"
    );
}
