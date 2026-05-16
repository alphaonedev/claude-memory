// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// v0.7.0 A1 AUTHN fix-campaign integration tests (2026-05-13).
//
// Pins:
//   - S5-C1: `POST /api/v1/pending/{id}/approve` and `/reject`
//     require a valid HMAC signature regardless of `api_key` config.
//     Without the signature header, the endpoint refuses with 401.
//     With a valid signature, the endpoint passes the gate and the
//     downstream behaviour (404 / 200 / 403) takes over.
//   - R3-S1.HMAC: `POST /api/v1/subscriptions` refuses registration
//     when neither a per-subscription `secret` nor a server-wide
//     `[hooks.subscription] hmac_secret` is configured. With either
//     of the two configured, registration succeeds.
//   - Pattern A (additive): the daemon's `bootstrap_serve` refuses
//     to start when `api_key` is unset AND the bind host is non-
//     loopback.

// Allow `await_holding_lock` — the std Mutex here is a test-serialiser
// for the process-wide HMAC secret, not a contention primitive.
#![allow(clippy::await_holding_lock)]
// pedantic: test scaffolding only.
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::too_many_lines)]

use ai_memory::config::set_active_hooks_hmac_secret;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Mutex;
use tower::ServiceExt as _;

/// Cross-test serialiser for the process-wide HMAC secret. The K10
/// approval HTTP tests use the same pattern under
/// `tests/k10_approval_http.rs::K10_HTTP_LOCK`.
static A1_HMAC_LOCK: Mutex<()> = Mutex::new(());

/// Synthesise a K10 approval signature binding method + `pending_id` per
/// release/v0.7.0 commit 99ffacc. Canonical: `<ts>.<METHOD>.<pending_id>.<body>`.
fn sign(secret: &str, timestamp: &str, method: &str, pending_id: &str, body: &str) -> String {
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
    // v0.7.0 release/v0.7.0 commit 99ffacc: K10 HMAC binds method + pending_id
    // in the canonical request to close the row-substitution attack vector.
    let canonical = format!("{timestamp}.{method}.{pending_id}.{body}");
    let sig = hmac_sha256_hex(&key_hash, &canonical);
    format!("sha256={sig}")
}

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

async fn seed_pending_store(
    db: &ai_memory::handlers::Db,
    namespace: &str,
    requested_by: &str,
) -> String {
    let lock = db.lock().await;
    let now_rfc = chrono::Utc::now().to_rfc3339();
    ai_memory::db::queue_pending_action(
        &lock.0,
        ai_memory::models::GovernedAction::Store,
        namespace,
        None,
        requested_by,
        &json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "tier": "long",
            "namespace": namespace,
            "title": "a1-pending",
            "content": "a1 fix test",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "test",
            "access_count": 0,
            "created_at": now_rfc,
            "updated_at": now_rfc,
            "metadata": {}
        }),
    )
    .expect("queue_pending_action")
}

// ===========================================================================
// S5-C1: approve / reject MUST require HMAC
// ===========================================================================

#[tokio::test]
async fn s5c1_approve_without_hmac_returns_401() {
    // No signature header → endpoint refuses, even though we never
    // configured `api_key` (legacy default-off auth posture).
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("a1-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pid = seed_pending_store(&db, "a1-ns", "alice").await;

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/pending/{pid}/approve"))
        .header("x-agent-id", "spoofed-admin")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    set_active_hooks_hmac_secret(None);
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "approve without HMAC must refuse (S5-C1)"
    );
}

#[tokio::test]
async fn s5c1_reject_without_hmac_returns_401() {
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("a1-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pid = seed_pending_store(&db, "a1-ns", "alice").await;

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/pending/{pid}/reject"))
        .header("x-agent-id", "spoofed-admin")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    set_active_hooks_hmac_secret(None);
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "reject without HMAC must refuse (S5-C1)"
    );
}

#[tokio::test]
async fn s5c1_approve_without_server_secret_returns_401_even_with_signature() {
    // No server-wide HMAC secret configured → endpoint must refuse
    // even when a "looks-valid" signature is presented. Fail-closed.
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(None);
    let (router, db) = build_router_with_db();
    let pid = seed_pending_store(&db, "a1-ns", "alice").await;

    let body = String::new();
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = sign("anything-no-server-secret", &ts, "POST", &pid, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/pending/{pid}/approve"))
        .header("x-agent-id", "alice")
        .header("x-ai-memory-timestamp", &ts)
        .header("x-ai-memory-signature", &sig)
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn s5c1_approve_with_valid_hmac_returns_200() {
    // Positive control: a properly-signed approve request gates
    // through the HMAC check and lands in the approve path.
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("a1-positive-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pid = seed_pending_store(&db, "a1-positive-ns", "alice").await;

    let body = String::new();
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = sign("a1-positive-secret", &ts, "POST", &pid, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/pending/{pid}/approve"))
        .header("x-agent-id", "alice")
        .header("x-ai-memory-timestamp", &ts)
        .header("x-ai-memory-signature", &sig)
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    set_active_hooks_hmac_secret(None);
    assert_eq!(
        status,
        StatusCode::OK,
        "approve with valid HMAC must succeed (S5-C1 positive control)"
    );
}

#[tokio::test]
async fn s5c1_reject_with_valid_hmac_returns_200() {
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("a1-positive-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pid = seed_pending_store(&db, "a1-reject-ns", "alice").await;

    let body = String::new();
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = sign("a1-positive-secret", &ts, "POST", &pid, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/pending/{pid}/reject"))
        .header("x-agent-id", "alice")
        .header("x-ai-memory-timestamp", &ts)
        .header("x-ai-memory-signature", &sig)
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    set_active_hooks_hmac_secret(None);
    assert_eq!(
        status,
        StatusCode::OK,
        "reject with valid HMAC must succeed (S5-C1 positive control)"
    );
}

// ===========================================================================
// R3-S1.HMAC: subscription registration MUST require HMAC
// ===========================================================================

#[tokio::test]
async fn r3_s1_subscribe_refuses_without_any_secret_returns_400() {
    // Neither per-sub `secret` nor server-wide override → 400.
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(None);
    let (router, _db) = build_router_with_db();

    let body = json!({
        "url": "https://example.com/webhook",
        "events": "*",
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/subscriptions")
        .header("content-type", "application/json")
        .header("x-agent-id", "alice")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let err = v["error"].as_str().unwrap_or("");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        err.contains("HMAC secret required"),
        "expected HMAC-required error, got: {err}"
    );
}

#[tokio::test]
async fn r3_s1_subscribe_succeeds_with_per_sub_secret() {
    // Per-sub secret supplied in the body → 201.
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(None);
    let (router, _db) = build_router_with_db();

    let body = json!({
        "url": "https://example.com/webhook",
        "events": "*",
        "secret": "per-sub-secret",
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/subscriptions")
        .header("content-type", "application/json")
        .header("x-agent-id", "alice")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "per-sub secret must satisfy the HMAC gate (R3-S1 positive control)"
    );
}

#[tokio::test]
async fn r3_s1_subscribe_succeeds_with_server_wide_secret() {
    // Server-wide override supplies the keying material → 201 without
    // a per-sub secret.
    let _g = A1_HMAC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("server-wide-secret".to_string()));
    let (router, _db) = build_router_with_db();

    let body = json!({
        "url": "https://example.com/webhook",
        "events": "*",
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/subscriptions")
        .header("content-type", "application/json")
        .header("x-agent-id", "alice")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    set_active_hooks_hmac_secret(None);
    assert_eq!(
        status,
        StatusCode::CREATED,
        "server-wide HMAC secret must satisfy the gate (R3-S1 positive control)"
    );
}

// ===========================================================================
// Pattern A: bootstrap_serve refuses non-loopback bind with no api_key
// ===========================================================================

#[tokio::test]
async fn s5c1_pattern_a_non_loopback_bind_without_api_key_refuses() {
    // The daemon's bootstrap must fail-fast when the operator tries
    // to bind to a routable address with no API key configured.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile for db");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    let args = ai_memory::daemon_runtime::ServeArgs {
        host: "0.0.0.0".to_string(),
        port: 0,
        tls_cert: None,
        tls_key: None,
        mtls_allowlist: None,
        shutdown_grace_secs: 30,
        quorum_writes: 0,
        quorum_peers: vec![],
        quorum_timeout_ms: 2000,
        quorum_client_cert: None,
        quorum_client_key: None,
        quorum_ca_cert: None,
        catchup_interval_secs: 30,
        #[cfg(feature = "sal")]
        store_url: None,
    };
    let cfg = ai_memory::config::AppConfig::default();
    // Sanity: default config must have no api_key.
    assert!(
        cfg.api_key.is_none(),
        "test fixture relies on default-off api_key"
    );

    let result = ai_memory::daemon_runtime::bootstrap_serve(&path, &args, &cfg).await;
    assert!(
        result.is_err(),
        "bootstrap must refuse non-loopback bind with no api_key (S5-C1)"
    );
    let err = result.err().unwrap();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("refusing to bind") && msg.contains("non-loopback"),
        "expected explicit non-loopback refusal in error chain, got: {msg}"
    );
}

#[tokio::test]
async fn s5c1_pattern_a_loopback_bind_without_api_key_succeeds_with_warn() {
    // Loopback bind with no api_key must still boot (single-tenant
    // dev convention). The startup just logs a WARN.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile for db");
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    let args = ai_memory::daemon_runtime::ServeArgs {
        host: "127.0.0.1".to_string(),
        port: 0,
        tls_cert: None,
        tls_key: None,
        mtls_allowlist: None,
        shutdown_grace_secs: 30,
        quorum_writes: 0,
        quorum_peers: vec![],
        quorum_timeout_ms: 2000,
        quorum_client_cert: None,
        quorum_client_key: None,
        quorum_ca_cert: None,
        catchup_interval_secs: 30,
        #[cfg(feature = "sal")]
        store_url: None,
    };
    let cfg = ai_memory::config::AppConfig::default();
    let result = ai_memory::daemon_runtime::bootstrap_serve(&path, &args, &cfg).await;
    assert!(
        result.is_ok(),
        "loopback bind without api_key must still boot (err: {:?})",
        result.err().map(|e| format!("{e:#}"))
    );
}
