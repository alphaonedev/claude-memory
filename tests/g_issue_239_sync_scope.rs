// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural
// impact on the regression we pin.
#![allow(clippy::redundant_closure_for_method_calls)]

//! v0.7.0 issue #239 — federation `/sync/since` per-peer namespace
//! scope-allowlist substrate.
//!
//! Threat (red-team #230): pre-v0.7.0 `/api/v1/sync/since` returned
//! every memory newer than the watermark with no per-peer namespace
//! scope. Compromise of any single mTLS peer key exfiltrated the
//! entire database. The fix:
//!
//! - Operator-configured allowlist
//!   (`AI_MEMORY_FED_PEER_ATTESTATION` env var, JSON) maps
//!   `peer_id → allowed_namespaces` (globs).
//! - Caller identifies via the same `x-peer-id` header used by #238.
//! - No-scope-row default-deny: empty page + `excluded_for_scope`
//!   count + WARN log.
//! - Backwards-compat env bypass `AI_MEMORY_FED_SYNC_TRUST_PEER=1`
//!   restores the legacy "full dump per peer" posture for the live
//!   Mac Mini test cell + the `DigitalOcean` campaign.
//!
//! Four cases covered:
//!
//! 1. **Allowlist match returns memories** — peer with
//!    `allowed_namespaces = ["public/*"]` pulls memories in
//!    `public/foo` AND sees rows in `private/x` excluded.
//! 2. **Allowlist mismatch returns empty** — peer with allowlist
//!    that doesn't cover any seeded namespace gets an empty page
//!    + an honest `excluded_for_scope` count.
//! 3. **No allowlist + env bypass = full dump (legacy)**.
//! 4. **No allowlist + no bypass = empty + WARN** (default-deny).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

/// Process-global async mutex so the env-var manipulations don't
/// race on parallel `cargo test`. `tokio::sync::Mutex` (not
/// `std::sync::Mutex`) so the guard can be held across the
/// `oneshot().await` without tripping `clippy::await_holding_lock`.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

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

async fn seed(db: &ai_memory::handlers::Db, ns: &str, title: &str) {
    let lock = db.lock().await;
    let now = chrono::Utc::now().to_rfc3339();
    let mem = ai_memory::models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Long,
        namespace: ns.into(),
        title: title.into(),
        content: "x".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "user".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: ai_memory::models::default_metadata(),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    ai_memory::db::insert(&lock.0, &mem).expect("seed insert");
}

fn reset_env() {
    unsafe {
        std::env::remove_var(ai_memory::federation::peer_attestation::SYNC_TRUST_PEER_ENV);
        std::env::remove_var(ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV);
    }
}

async fn sync_since_body(router: axum::Router, peer_id: Option<&str>) -> Value {
    let mut req_builder = Request::builder()
        .method("GET")
        .uri("/api/v1/sync/since")
        .header("content-type", "application/json");
    if let Some(p) = peer_id {
        req_builder =
            req_builder.header(ai_memory::federation::peer_attestation::PEER_ID_HEADER, p);
    }
    let req = req_builder.body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "sync_since must always 200");
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn case_1_allowlist_match_returns_in_scope_excludes_others() {
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    let allowlist = r#"{
        "peer-1": {
            "allowed_namespaces": ["public/*"]
        }
    }"#;
    unsafe {
        std::env::set_var(
            ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV,
            allowlist,
        );
    }
    let (router, db) = build_router_with_db();
    seed(&db, "public/alpha", "a").await;
    seed(&db, "public/beta", "b").await;
    seed(&db, "private/secret", "c").await;
    let body = sync_since_body(router, Some("peer-1")).await;
    unsafe {
        std::env::remove_var(ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV);
    }
    drop(env_guard);
    let mems = body["memories"].as_array().unwrap();
    assert_eq!(
        mems.len(),
        2,
        "only the two public/* rows should be returned"
    );
    for m in mems {
        let ns = m["namespace"].as_str().unwrap();
        assert!(ns.starts_with("public/"), "leaked outside scope: {ns}");
    }
    assert_eq!(
        body["excluded_for_scope"], 1,
        "the private/secret row must be reported as excluded"
    );
    assert_eq!(body["scope_status"], "scoped");
}

#[tokio::test]
async fn case_2_allowlist_mismatch_returns_empty() {
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    // peer-1's allowlist doesn't intersect anything we seed.
    let allowlist = r#"{
        "peer-1": {
            "allowed_namespaces": ["nonexistent/*"]
        }
    }"#;
    unsafe {
        std::env::set_var(
            ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV,
            allowlist,
        );
    }
    let (router, db) = build_router_with_db();
    seed(&db, "public/alpha", "a").await;
    seed(&db, "private/secret", "b").await;
    let body = sync_since_body(router, Some("peer-1")).await;
    unsafe {
        std::env::remove_var(ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV);
    }
    drop(env_guard);
    assert_eq!(
        body["memories"].as_array().unwrap().len(),
        0,
        "non-intersecting allowlist must return zero rows"
    );
    assert_eq!(
        body["excluded_for_scope"], 2,
        "both seeded rows must be reported as excluded"
    );
}

#[tokio::test]
async fn case_3_no_allowlist_with_bypass_is_full_dump() {
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    // Legacy posture: operator deliberately opted out.
    unsafe {
        std::env::set_var(
            ai_memory::federation::peer_attestation::SYNC_TRUST_PEER_ENV,
            "1",
        );
    }
    let (router, db) = build_router_with_db();
    seed(&db, "public/alpha", "a").await;
    seed(&db, "private/secret", "b").await;
    let body = sync_since_body(router, Some("peer-1")).await;
    unsafe {
        std::env::remove_var(ai_memory::federation::peer_attestation::SYNC_TRUST_PEER_ENV);
    }
    drop(env_guard);
    assert_eq!(
        body["memories"].as_array().unwrap().len(),
        2,
        "AI_MEMORY_FED_SYNC_TRUST_PEER=1 must return every row (legacy posture)"
    );
    assert_eq!(body["scope_status"], "legacy_bypass");
}

#[tokio::test]
async fn case_4_no_allowlist_no_bypass_default_denies() {
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    let (router, db) = build_router_with_db();
    seed(&db, "public/alpha", "a").await;
    seed(&db, "private/secret", "b").await;
    let body = sync_since_body(router, Some("peer-1")).await;
    drop(env_guard);
    assert_eq!(
        body["memories"].as_array().unwrap().len(),
        0,
        "default-deny: peer without an allowlist must see zero rows"
    );
    assert_eq!(body["scope_status"], "no_allowlist_default_deny");
}

#[tokio::test]
async fn case_5_no_peer_header_default_denies() {
    // Defense-in-depth — even a peer that skips the header gets
    // default-deny rather than the legacy full dump.
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    let (router, db) = build_router_with_db();
    seed(&db, "public/alpha", "a").await;
    let body = sync_since_body(router, None).await;
    drop(env_guard);
    assert_eq!(
        body["memories"].as_array().unwrap().len(),
        0,
        "no x-peer-id header default-denies"
    );
}
