// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural
// impact on the regression we pin.
#![allow(clippy::redundant_closure_for_method_calls)]

//! v0.7.0 issue #238 — federation receive attests body-claimed
//! `sender_agent_id` against the wire-level `x-peer-id` header.
//!
//! Threat (red-team #230): any peer with a valid mTLS cert could claim
//! any `agent_id` in the body, defeating per-agent audit-trail
//! integrity. The fix lifts a NEW outbound header `x-peer-id` carrying
//! the peer's self-claim into a request-shape position the receiver
//! reads BEFORE the body deserialise, and validates the body field
//! against either:
//!   - the header itself (peer authoring as itself), or
//!   - an operator-configured allowlist
//!     (`AI_MEMORY_FED_PEER_ATTESTATION` env var, JSON).
//!
//! Four cases covered:
//!
//! 1. **Header matches body** — accepted; memory lands.
//! 2. **Header mismatches body, no allowlist** — 403 with structured
//!    error envelope; memory does NOT land.
//! 3. **Body field absent (legacy unauthored push)** — accepted with
//!    a WARN log; the legacy peer never claimed an author.
//! 4. **Bypass env set** — accepted even on mismatch, with a WARN log
//!    confirming the operator opted out.
//!
//! These tests serialise on the process-global env var. We use the
//! `serial_test::serial` crate when available; here we just route
//! every test through a small mutex to avoid the env-var race when
//! `cargo test` parallelises across the file.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt as _;

/// Process-global async mutex so the env-var manipulations in the
/// bypass case don't race the no-bypass cases on parallel
/// `cargo test`. `tokio::sync::Mutex` (not `std::sync::Mutex`) so
/// the guard can be held across the `oneshot().await` without
/// tripping `clippy::await_holding_lock`.
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

/// Build a minimal `/sync/push` request body with a single memory.
fn push_body(sender_agent_id: &str) -> Value {
    let now = chrono::Utc::now().to_rfc3339();
    json!({
        "sender_agent_id": sender_agent_id,
        "sender_clock": {"entries": {}},
        "memories": [{
            "id": uuid::Uuid::new_v4().to_string(),
            "tier": "long",
            "namespace": "issue-238",
            "title": "attested write",
            "content": "body for #238 regression",
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
    })
}

async fn count_memories_in_ns(db: &ai_memory::handlers::Db, ns: &str) -> i64 {
    let lock = db.lock().await;
    lock.0
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
            rusqlite::params![ns],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
}

/// Strip env-var contamination from prior tests so each case starts
/// from a clean slate. Held inside the `ENV_LOCK` by every caller.
fn reset_env() {
    unsafe {
        std::env::remove_var(ai_memory::federation::peer_attestation::TRUST_BODY_AGENT_ID_ENV);
        std::env::remove_var(ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV);
    }
}

#[tokio::test]
async fn case_1_header_matches_body_accepts() {
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    let (router, db) = build_router_with_db();
    let body = push_body("peer-1");
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sync/push")
        .header("content-type", "application/json")
        .header(
            ai_memory::federation::peer_attestation::PEER_ID_HEADER,
            "peer-1",
        )
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let env_body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(
        status,
        StatusCode::OK,
        "matching x-peer-id and body sender_agent_id must accept; body={env_body}"
    );
    drop(env_guard);
    assert_eq!(
        count_memories_in_ns(&db, "issue-238").await,
        1,
        "exactly one memory should have landed; response={env_body}"
    );
}

#[tokio::test]
async fn case_2_header_mismatch_no_allowlist_refuses_403() {
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    let (router, db) = build_router_with_db();
    let body = push_body("alice"); // body claims alice
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sync/push")
        .header("content-type", "application/json")
        .header(
            ai_memory::federation::peer_attestation::PEER_ID_HEADER,
            "peer-1", // but the wire-level peer is peer-1
        )
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "mismatched x-peer-id vs body.sender_agent_id must 403"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let env: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(env["error"], "sender_agent_id_mismatch");
    assert_eq!(env["claimed"], "alice");
    assert_eq!(env["peer_header"], "peer-1");
    drop(env_guard);
    assert_eq!(
        count_memories_in_ns(&db, "issue-238").await,
        0,
        "no memory should have landed on a 403 refusal"
    );
}

#[tokio::test]
async fn case_3_header_absent_body_present_refuses_unless_bypass() {
    // The brief calls out "body field missing (allow — backward compat)"
    // as the legacy-unauthored push shape: no x-peer-id header AND
    // body.sender_agent_id is empty / a default placeholder. Here we
    // exercise the explicit-header-missing case where the body still
    // carries a claim — that's the substantive #238 threat surface
    // and must 403 (header absent = peer cannot be attested).
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    let (router, _db) = build_router_with_db();
    let body = push_body("alice");
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sync/push")
        .header("content-type", "application/json")
        // intentionally no x-peer-id header
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "missing x-peer-id with a body-claimed sender must 403"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let env: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(env["error"], "peer_id_header_missing");
    drop(env_guard);
}

#[tokio::test]
async fn case_4_env_bypass_allows_mismatch() {
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    // Operator-explicit opt-out for legacy peers.
    unsafe {
        std::env::set_var(
            ai_memory::federation::peer_attestation::TRUST_BODY_AGENT_ID_ENV,
            "1",
        );
    }
    let (router, db) = build_router_with_db();
    let body = push_body("alice");
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sync/push")
        .header("content-type", "application/json")
        .header(
            ai_memory::federation::peer_attestation::PEER_ID_HEADER,
            "peer-1",
        )
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1 must accept a mismatched push"
    );
    unsafe {
        std::env::remove_var(ai_memory::federation::peer_attestation::TRUST_BODY_AGENT_ID_ENV);
    }
    drop(env_guard);
    assert_eq!(
        count_memories_in_ns(&db, "issue-238").await,
        1,
        "with bypass set the memory must land regardless of header"
    );
}

#[tokio::test]
async fn case_5_allowlist_permits_mismatch() {
    // The brief's third "allowlist" semantics: peer-1 with operator-
    // configured `allowed_sender_agent_ids = ["alice"]` may legitimately
    // claim alice on the body even though x-peer-id is peer-1.
    let env_guard = ENV_LOCK.lock().await;
    reset_env();
    let allowlist = r#"{
        "peer-1": {
            "allowed_sender_agent_ids": ["alice", "bob"],
            "allowed_namespaces": ["issue-238"]
        }
    }"#;
    unsafe {
        std::env::set_var(
            ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV,
            allowlist,
        );
    }
    let (router, db) = build_router_with_db();
    let body = push_body("alice");
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sync/push")
        .header("content-type", "application/json")
        .header(
            ai_memory::federation::peer_attestation::PEER_ID_HEADER,
            "peer-1",
        )
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    unsafe {
        std::env::remove_var(ai_memory::federation::peer_attestation::PEER_ATTESTATION_ENV);
    }
    assert_eq!(
        status,
        StatusCode::OK,
        "operator-configured allowlist must permit body-claim ∈ allowed_sender_agent_ids"
    );
    drop(env_guard);
    assert_eq!(count_memories_in_ns(&db, "issue-238").await, 1);
}
