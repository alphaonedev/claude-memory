// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::redundant_closure_for_method_calls)]
//! v0.7.0 K10 — `POST /api/v1/approvals/{pending_id}` HMAC gate.
//!
//! Pins:
//!   - With a valid `X-AI-Memory-Signature` (and a server-wide
//!     `[hooks.subscription].hmac_secret` configured) the endpoint
//!     accepts the body and approves the pending row → 200.
//!   - Without the signature header, the endpoint rejects with 401
//!     even when the body would otherwise be valid.
//!   - Without ANY server-wide HMAC secret configured, the endpoint
//!     fails closed (rejects every inbound request → 401), matching
//!     the K10 spec contract that the K7 secret is the only
//!     authentication mode for write-side approval traffic.

// `await_holding_lock` lints fire on `std::sync::Mutex` — the lock
// here is purely a test-serialisation primitive (the global HMAC
// secret state is itself thread-safe). Allow at the file level.
#![allow(clippy::await_holding_lock)]

use ai_memory::config::set_active_hooks_hmac_secret;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Mutex;
use tower::ServiceExt as _;

static K10_HTTP_LOCK: Mutex<()> = Mutex::new(());

/// Build the router from a shared `Db` so the test body can both
/// hit HTTP routes AND seed `pending_actions` rows directly.
fn build_router_with_db() -> (axum::Router, ai_memory::handlers::Db) {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let path = std::path::PathBuf::from(":memory:");
    let db: ai_memory::handlers::Db = std::sync::Arc::new(tokio::sync::Mutex::new((
        conn,
        path,
        ai_memory::config::ResolvedTtl::default(),
        true,
    )));
    // v0.7.0 Wave-3 — populate a tempfile-backed SqliteStore for the
    // SAL trait handle. The legacy `db` connection lives in `:memory:`
    // and the trait handle's tempfile is disjoint; this k10 test
    // exercises only the legacy direct-rusqlite path so the disjoint
    // backing file is harmless.
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
    };
    let api_key_state = ai_memory::handlers::ApiKeyState { key: None };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, db)
}

async fn seed_pending_row_via_db(
    db: &ai_memory::handlers::Db,
    namespace: &str,
    requested_by: &str,
) -> String {
    // Seed a real memory and queue a delete-pending row that targets
    // it. The delete branch of `db::execute_pending_action` only needs
    // a valid `memory_id`, so the test doesn't have to forge a full
    // store payload.
    let lock = db.lock().await;
    let mem = ai_memory::models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Long,
        namespace: namespace.to_string(),
        title: "k10-test".into(),
        content: "from K10 HTTP test".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
    };
    let mem_id = ai_memory::db::insert(&lock.0, &mem).expect("insert memory");
    let payload = json!({"reason": "k10-test"});
    ai_memory::db::queue_pending_action(
        &lock.0,
        ai_memory::models::GovernedAction::Delete,
        namespace,
        Some(&mem_id),
        requested_by,
        &payload,
    )
    .expect("queue_pending_action")
}

/// Compute the K7-style HMAC signature header value for a request body.
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
async fn http_approve_with_valid_hmac_returns_200() {
    let _g = K10_HTTP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("k10-test-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pending_id = seed_pending_row_via_db(&db, "scratch", "alice").await;

    let body = json!({"decision": "approve", "remember": "once"}).to_string();
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let sig = sign("k10-test-secret", &timestamp, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/approvals/{pending_id}"))
        .header("content-type", "application/json")
        .header("x-ai-memory-timestamp", &timestamp)
        .header("x-ai-memory-signature", sig)
        .header("x-agent-id", "operator-1")
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    assert_eq!(
        status,
        StatusCode::OK,
        "expected 200 with valid HMAC; got {status} body={json}"
    );
    assert_eq!(json["approved"], json!(true));
    assert_eq!(json["remember"], json!("once"));
    set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_approve_without_hmac_returns_401() {
    let _g = K10_HTTP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("k10-test-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pending_id = seed_pending_row_via_db(&db, "scratch", "alice").await;

    let body = json!({"decision": "approve", "remember": "once"}).to_string();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/approvals/{pending_id}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "expected 401 without signature header"
    );
    set_active_hooks_hmac_secret(None);
}

#[tokio::test]
async fn http_approve_without_server_secret_returns_401() {
    let _g = K10_HTTP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // Explicitly clear the server-wide secret. Without it the endpoint
    // MUST refuse all inbound approvals — fail-closed posture.
    set_active_hooks_hmac_secret(None);
    let (router, db) = build_router_with_db();
    let pending_id = seed_pending_row_via_db(&db, "scratch", "alice").await;

    // Even with a "looks-valid" signature, no server secret → 401.
    let body = json!({"decision": "approve", "remember": "once"}).to_string();
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let sig = sign("anything-since-no-server-secret", &timestamp, &body);
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/approvals/{pending_id}"))
        .header("content-type", "application/json")
        .header("x-ai-memory-timestamp", &timestamp)
        .header("x-ai-memory-signature", sig)
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "expected 401 when server has no HMAC secret configured"
    );
}

#[tokio::test]
async fn http_approve_with_wrong_signature_returns_401() {
    let _g = K10_HTTP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("real-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pending_id = seed_pending_row_via_db(&db, "scratch", "alice").await;

    let body = json!({"decision": "approve", "remember": "once"}).to_string();
    let timestamp = chrono::Utc::now().timestamp().to_string();
    // Sign with the wrong key — must be rejected even though header
    // is well-formed.
    let sig = sign("wrong-secret", &timestamp, &body);
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/approvals/{pending_id}"))
        .header("content-type", "application/json")
        .header("x-ai-memory-timestamp", &timestamp)
        .header("x-ai-memory-signature", sig)
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "expected 401 when signature does not match the configured secret"
    );
    set_active_hooks_hmac_secret(None);
}
