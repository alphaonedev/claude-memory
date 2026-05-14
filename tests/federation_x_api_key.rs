// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 fold-A2A1.4 (#702) — federation outbound `x-api-key` forwarding +
//! mTLS bypass for inbound api-key checks on `/api/v1/sync/*`.
//!
//! These tests pin the procurement-grade auth matrix for cross-host
//! federation:
//!
//! | Deployment mode             | Inbound `/api/v1/sync/*`  | Outbound POST  |
//! |-----------------------------|---------------------------|----------------|
//! | mTLS-only                   | mTLS cert verify          | (no api-key)   |
//! | api-key-only                | `x-api-key` required      | `x-api-key`    |
//! | mTLS + api-key              | mTLS (api-key bypassed)   | `x-api-key`    |
//!
//! Phase B test cell discovered the gap: with `[api] api_key` set, the
//! outbound federation POST DID NOT carry `x-api-key`, so the peer's
//! `api_key_auth` middleware returned 401 and `quorum_not_met` fired on
//! every cross-host write. The mTLS-bypass closes the auth-matrix gap
//! the other direction: a peer that authenticates via mTLS shouldn't
//! also need to know the shared api-key secret.

use axum::Router;
use axum::extract::{Json as AxumJson, State};
use axum::http::StatusCode;
use axum::routing::post;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use ai_memory::federation::{
    FederationConfig, PeerEndpoint, broadcast_store_quorum, finalise_quorum,
};
use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::replication::QuorumPolicy;

/// Records every inbound POST to a mock peer so tests can assert what
/// headers landed.
#[derive(Clone, Default)]
struct HeaderCapture {
    /// Number of inbound POSTs observed.
    count: Arc<AtomicUsize>,
    /// The most-recent value of the `x-api-key` header (or `None` if
    /// the request didn't carry one).
    last_api_key: Arc<Mutex<Option<String>>>,
}

#[derive(Clone, Copy)]
enum PeerMode {
    /// Always 200 — accept any inbound regardless of credentials.
    Permissive,
    /// Demand `x-api-key` matches the configured secret. Otherwise 401.
    /// Mirrors the production `api_key_auth` middleware's reject path
    /// — the peer this test stands in for runs with `[api] api_key`
    /// configured and rejects any inbound POST that doesn't carry the
    /// matching `x-api-key` header.
    RequireApiKey,
}

#[derive(Clone)]
struct MockState {
    mode: PeerMode,
    expected_api_key: String,
    capture: HeaderCapture,
}

async fn handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    AxumJson(_body): AxumJson<serde_json::Value>,
) -> (StatusCode, AxumJson<serde_json::Value>) {
    state.capture.count.fetch_add(1, Ordering::Relaxed);
    let api_key_val = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    *state.capture.last_api_key.lock().await = api_key_val.clone();
    match state.mode {
        PeerMode::Permissive => (
            StatusCode::OK,
            AxumJson(serde_json::json!({"applied":1,"noop":0,"skipped":0})),
        ),
        PeerMode::RequireApiKey => {
            if api_key_val.as_deref() == Some(state.expected_api_key.as_str()) {
                (
                    StatusCode::OK,
                    AxumJson(serde_json::json!({"applied":1,"noop":0,"skipped":0})),
                )
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    AxumJson(serde_json::json!({"error":"missing or invalid API key"})),
                )
            }
        }
    }
}

async fn spawn_peer(mode: PeerMode, expected_key: &str) -> (String, HeaderCapture) {
    let capture = HeaderCapture::default();
    let state = MockState {
        mode,
        expected_api_key: expected_key.to_string(),
        capture: capture.clone(),
    };
    let app = Router::new()
        .route("/api/v1/sync/push", post(handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), capture)
}

fn sample_memory() -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: format!("apikey-fanout-{}", uuid::Uuid::new_v4()),
        tier: Tier::Mid,
        namespace: "ns".to_string(),
        title: "x-api-key fanout".to_string(),
        content: "fanout body".to_string(),
        tags: vec!["fed".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id":"ai:fanout"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
    }
}

/// Build a `FederationConfig` pointing the leader at the given peer URLs.
fn fed_cfg(
    peer_urls: &[String],
    quorum_writes: usize,
    timeout_ms: u64,
    api_key: Option<String>,
) -> FederationConfig {
    let timeout = Duration::from_millis(timeout_ms);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("build test reqwest client");
    let n = 1 + peer_urls.len();
    let policy = QuorumPolicy::new(n, quorum_writes, timeout, Duration::from_secs(30))
        .expect("valid quorum policy");
    let peers = peer_urls
        .iter()
        .enumerate()
        .map(|(i, raw)| PeerEndpoint {
            id: format!("peer-{i}"),
            sync_push_url: format!("{}/api/v1/sync/push", raw.trim_end_matches('/')),
        })
        .collect();
    FederationConfig {
        policy,
        peers,
        client,
        sender_agent_id: "ai:fanout-leader".to_string(),
        api_key,
    }
}

// --------------------------------------------------------------------------
// Test 1 — outbound forwarding when api_key is configured
// --------------------------------------------------------------------------

/// With `api_key=Some(...)` on the leader's `FederationConfig`, the
/// `x-api-key` header MUST land on every outbound federation POST.
/// Without this, peers that themselves run with api-key auth reject
/// the fanout and quorum never converges across hosts.
#[tokio::test]
async fn federation_outbound_forwards_x_api_key_when_configured() {
    let secret = "procurement-grade-secret-9f4e";
    let (url1, cap1) = spawn_peer(PeerMode::RequireApiKey, secret).await;
    let (url2, cap2) = spawn_peer(PeerMode::RequireApiKey, secret).await;
    let cfg = fed_cfg(
        &[url1, url2],
        2, // W=2: local + one peer
        2000,
        Some(secret.to_string()),
    );

    let tracker = broadcast_store_quorum(&cfg, &sample_memory())
        .await
        .expect("broadcast must succeed when peers are reachable");
    let outcome = finalise_quorum(&tracker);
    assert!(
        outcome.is_ok(),
        "quorum must converge when outbound carries x-api-key: {outcome:?}"
    );

    // Let the post-quorum detach finish so both captures observe the
    // header value.
    for _ in 0..40 {
        if cap1.count.load(Ordering::Relaxed) >= 1 && cap2.count.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        cap1.last_api_key.lock().await.as_deref(),
        Some(secret),
        "peer-1 must see the operator-configured x-api-key header"
    );
    assert_eq!(
        cap2.last_api_key.lock().await.as_deref(),
        Some(secret),
        "peer-2 must see the operator-configured x-api-key header"
    );
}

// --------------------------------------------------------------------------
// Test 2 — backwards compat when api_key is unset
// --------------------------------------------------------------------------

/// With `api_key=None` (mTLS-only or no-auth deployments), no
/// `x-api-key` header is attached. Pre-v0.7.0 deployments that work
/// today MUST continue to work; the v0.6.x default-off auth posture
/// is preserved verbatim when the operator hasn't opted into api-key
/// auth.
#[tokio::test]
async fn federation_outbound_omits_x_api_key_when_unconfigured() {
    // The permissive peer accepts any request and records what header
    // value (if any) the leader sent.
    let (url1, cap1) = spawn_peer(PeerMode::Permissive, "unused").await;
    let cfg = fed_cfg(&[url1], 2, 2000, None);

    let tracker = broadcast_store_quorum(&cfg, &sample_memory())
        .await
        .expect("broadcast must succeed against permissive peer");
    assert!(finalise_quorum(&tracker).is_ok());

    for _ in 0..40 {
        if cap1.count.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        cap1.last_api_key.lock().await.as_deref(),
        None,
        "no x-api-key header may be attached when the leader has api_key=None — \
         pre-v0.7.0 deployments must work unchanged"
    );
}

// --------------------------------------------------------------------------
// Test 3 — mTLS bypass: peer accepts when mTLS verified, no api-key
// --------------------------------------------------------------------------

/// Substrate-level check on the inbound bypass: an `ApiKeyState` with
/// `mtls_enforced=true` lets a request to `/api/v1/sync/push` through
/// even when `x-api-key` is absent. Non-sync paths still demand the
/// key. This is the auth-matrix row where the operator deploys mTLS+
/// api-key and expects the mTLS cert to satisfy the federation auth
/// requirement (rustls has already verified the peer cert before the
/// request reaches handler code).
#[tokio::test]
async fn mtls_authenticated_request_bypasses_api_key_check() {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use tower::ServiceExt as _;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    let state = ai_memory::handlers::ApiKeyState {
        key: Some("operator-secret".to_string()),
        mtls_enforced: true,
    };
    let app = Router::new()
        .route("/api/v1/sync/push", post(ok_handler))
        .route("/api/v1/sync/since", get(ok_handler))
        .route("/api/v1/memories", get(ok_handler))
        .layer(from_fn_with_state(state, ai_memory::handlers::api_key_auth));

    // /api/v1/sync/push with NO x-api-key but mTLS-enforced listener
    // — bypass applies, 200 expected.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sync/push")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "mTLS-enforced listener must let /api/v1/sync/push through without x-api-key"
    );

    // /api/v1/sync/since (GET) is also a federation endpoint — same
    // bypass applies.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sync/since")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "mTLS-enforced listener must let /api/v1/sync/since through without x-api-key"
    );

    // /api/v1/memories is NOT a federation endpoint — bypass MUST NOT
    // apply. Without an api-key the request is rejected with 401.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/memories")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "non-federation paths must still require x-api-key when configured"
    );
}

// --------------------------------------------------------------------------
// Test 4 — full quorum convergence with cross-host api-key auth
// --------------------------------------------------------------------------

/// The procurement-grade scenario the gap originally broke. Three
/// hosts, W=2/N=3, every peer runs with api-key auth. With the fix
/// in place the leader's outbound POSTs carry `x-api-key` and the
/// quorum converges; without the fix every peer 401'd and
/// `quorum_not_met` fired on every write.
#[tokio::test]
async fn cross_host_quorum_w2_n3_with_api_key_converges() {
    let secret = "auth-matrix-3a92";
    let (url1, _) = spawn_peer(PeerMode::RequireApiKey, secret).await;
    let (url2, _) = spawn_peer(PeerMode::RequireApiKey, secret).await;
    // W=2/N=3 → local + 2 peers; we wire just 2 peers (local + 2 = N=3).
    let cfg = fed_cfg(&[url1, url2], 2, 2000, Some(secret.to_string()));

    let tracker = broadcast_store_quorum(&cfg, &sample_memory())
        .await
        .expect("broadcast must succeed when outbound carries x-api-key");
    let outcome = finalise_quorum(&tracker);
    assert!(
        outcome.is_ok(),
        "W=2/N=3 quorum must converge under api-key auth: {outcome:?}"
    );
}
