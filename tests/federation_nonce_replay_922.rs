// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 issue #922 — federation per-message nonce replay protection.
//!
//! Specialist 4's truthfulness audit (truth-4-20260519T161520Z)
//! empirically replayed a valid signed `/sync/push` body byte-for-byte
//! against a receiver running with `AI_MEMORY_FED_REQUIRE_SIG=1` and
//! observed `HTTP=200` both times. The signature verified correctly
//! (the bytes are unchanged), but no replay protection existed on the
//! federation receive path. This regression test pins the four
//! contract arms the receiver now enforces under
//! `AI_MEMORY_FED_REQUIRE_NONCE=1` (the v0.7.0 secure default):
//!
//! 1. Alice POSTs a valid nonce-bound signed body with nonce **N1** →
//!    HTTP **200** (first-seen — accepted + fingerprint recorded).
//! 2. Alice POSTs the **identical** body with the **same** nonce
//!    **N1** → HTTP **401** with stable tag `x_memory_nonce_replay`.
//! 3. Alice POSTs the same body with a **fresh** nonce **N2** →
//!    HTTP **200** (fresh fingerprint — accepted).
//! 4. With `AI_MEMORY_FED_REQUIRE_NONCE=0`, both posts are accepted
//!    regardless of nonce header presence (legacy fallback for the
//!    peer-rollout window).
//!
//! The test stands up the real `sync_push` handler via
//! `ai_memory::build_router` so we exercise the production wire shape
//! — same axum router, same `AppState`, same `verify_signature_or_reject`.
//! No HTTP daemon is spawned; `tower::ServiceExt::oneshot` drives the
//! router directly in-process.

#![allow(clippy::too_many_lines)]
// The env_lock guard is intentionally held across .await points so
// no other test can observe the intermediate `AI_MEMORY_FED_REQUIRE_*`
// env-var state. Tests run on a single-threaded runtime
// (`flavor = "current_thread"`) so no other task can race the lock
// during the awaited HTTP round-trip.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::federation::signing::{
    NONCE_HEADER, REQUIRE_NONCE_ENV, REQUIRE_SIG_ENV, SIGNATURE_HEADER, sign_body_with_nonce_header,
};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::identity::keypair as kp_mod;

struct TwoHosts {
    router: axum::Router,
    alice: kp_mod::AgentKeypair,
    _db_tmp: tempfile::NamedTempFile,
    _key_tmp: TempDir,
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn setup() -> TwoHosts {
    let db_tmp = tempfile::NamedTempFile::new().expect("db tempfile");
    let db_path = db_tmp.path().to_path_buf();
    let _ = ai_memory::db::open(&db_path).expect("db::open");
    let conn = ai_memory::db::open(&db_path).expect("reopen for AppState");
    let db: Db = Arc::new(Mutex::new((
        conn,
        db_path.clone(),
        ResolvedTtl::default(),
        true,
    )));

    let key_tmp = TempDir::new().expect("key tempdir");
    // SAFETY: env mutation; the test holds the env_lock for the
    // duration so no other test sees the intermediate state.
    unsafe {
        std::env::set_var("AI_MEMORY_KEY_DIR", key_tmp.path());
    }

    // Alice generates a fresh keypair and bob enrols her public key.
    let alice = kp_mod::generate("ai:peer-alice").expect("generate alice keypair");
    let alice_pub_only = kp_mod::AgentKeypair {
        agent_id: alice.agent_id.clone(),
        public: alice.public,
        private: None,
    };
    kp_mod::save_public_only(&alice_pub_only, key_tmp.path()).expect("enrol alice pubkey on bob");

    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
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
        storage_backend: StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store: Arc::new(
            ai_memory::store::sqlite::SqliteStore::open(&db_path).expect("open SqliteStore"),
        ),
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: Arc::new(ai_memory::identity::replay::ReplayCache::default()),
        verify_require_nonce: false,
        federation_nonce_cache: Arc::new(
            ai_memory::identity::replay::FederationNonceCache::default(),
        ),
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    };
    let router = ai_memory::build_router(api_key_state, app_state);

    TwoHosts {
        router,
        alice,
        _db_tmp: db_tmp,
        _key_tmp: key_tmp,
    }
}

fn sample_body() -> Value {
    json!({
        "sender_agent_id": "ai:peer-alice",
        "sender_clock": {"entries": {}},
        "memories": [],
        "dry_run": false,
    })
}

async fn post(
    router: &axum::Router,
    body_bytes: Vec<u8>,
    sig_header: Option<&str>,
    nonce_header: Option<&str>,
    peer_id_header: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/sync/push")
        .header("content-type", "application/json")
        .header(
            ai_memory::federation::peer_attestation::PEER_ID_HEADER,
            peer_id_header,
        );
    if let Some(sig) = sig_header {
        builder = builder.header(SIGNATURE_HEADER, sig);
    }
    if let Some(nonce) = nonce_header {
        builder = builder.header(NONCE_HEADER, nonce);
    }
    let req = builder.body(Body::from(body_bytes)).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test(flavor = "current_thread")]
async fn replay_922_first_post_with_valid_nonce_accepted() {
    let _g = env_lock();
    unsafe {
        std::env::set_var(REQUIRE_SIG_ENV, "1");
        std::env::set_var(REQUIRE_NONCE_ENV, "1");
    }
    let host = setup();
    let body = sample_body();
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let priv_key = host.alice.private.as_ref().expect("alice has private key");
    let nonce = uuid::Uuid::new_v4().to_string();
    let sig = sign_body_with_nonce_header(priv_key, &body_bytes, &nonce);

    let (status, v) = post(
        &host.router,
        body_bytes,
        Some(&sig),
        Some(&nonce),
        &host.alice.agent_id,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "first nonce-bound post must be accepted: {v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn replay_922_repeated_nonce_returns_401_with_replay_tag() {
    let _g = env_lock();
    unsafe {
        std::env::set_var(REQUIRE_SIG_ENV, "1");
        std::env::set_var(REQUIRE_NONCE_ENV, "1");
    }
    let host = setup();
    let body = sample_body();
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let priv_key = host.alice.private.as_ref().expect("alice has private key");
    let nonce = uuid::Uuid::new_v4().to_string();
    let sig = sign_body_with_nonce_header(priv_key, &body_bytes, &nonce);

    let (status1, _v1) = post(
        &host.router,
        body_bytes.clone(),
        Some(&sig),
        Some(&nonce),
        &host.alice.agent_id,
    )
    .await;
    assert_eq!(status1, StatusCode::OK, "first post must be accepted");

    let (status2, v2) = post(
        &host.router,
        body_bytes,
        Some(&sig),
        Some(&nonce),
        &host.alice.agent_id,
    )
    .await;
    assert_eq!(
        status2,
        StatusCode::UNAUTHORIZED,
        "byte-for-byte replay must be refused with 401, got {v2}"
    );
    assert_eq!(
        v2.get("error").and_then(Value::as_str),
        Some("x_memory_nonce_replay"),
        "401 envelope must carry the stable replay tag, got {v2}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn replay_922_fresh_nonce_after_replay_is_accepted() {
    let _g = env_lock();
    unsafe {
        std::env::set_var(REQUIRE_SIG_ENV, "1");
        std::env::set_var(REQUIRE_NONCE_ENV, "1");
    }
    let host = setup();
    let body = sample_body();
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let priv_key = host.alice.private.as_ref().expect("alice has private key");

    let nonce_1 = uuid::Uuid::new_v4().to_string();
    let sig_1 = sign_body_with_nonce_header(priv_key, &body_bytes, &nonce_1);
    let (status_1, _v) = post(
        &host.router,
        body_bytes.clone(),
        Some(&sig_1),
        Some(&nonce_1),
        &host.alice.agent_id,
    )
    .await;
    assert_eq!(status_1, StatusCode::OK);

    let nonce_2 = uuid::Uuid::new_v4().to_string();
    assert_ne!(
        nonce_1, nonce_2,
        "uuid v4 collision is astronomically unlikely"
    );
    let sig_2 = sign_body_with_nonce_header(priv_key, &body_bytes, &nonce_2);
    let (status_2, v2) = post(
        &host.router,
        body_bytes,
        Some(&sig_2),
        Some(&nonce_2),
        &host.alice.agent_id,
    )
    .await;
    assert_eq!(
        status_2,
        StatusCode::OK,
        "fresh nonce on the same body must be accepted: {v2}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn replay_922_legacy_fallback_accepts_unsigned_nonce_when_env_is_zero() {
    let _g = env_lock();
    unsafe {
        std::env::set_var(REQUIRE_SIG_ENV, "1");
        std::env::set_var(REQUIRE_NONCE_ENV, "0");
    }
    let host = setup();
    let body = sample_body();
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let priv_key = host.alice.private.as_ref().expect("alice has private key");
    let sig = ai_memory::federation::signing::sign_body_header(priv_key, &body_bytes);

    let (status1, _v1) = post(
        &host.router,
        body_bytes.clone(),
        Some(&sig),
        None,
        &host.alice.agent_id,
    )
    .await;
    assert_eq!(
        status1,
        StatusCode::OK,
        "legacy unsigned-nonce post must be accepted under REQUIRE_NONCE=0"
    );

    let (status2, _v2) = post(
        &host.router,
        body_bytes,
        Some(&sig),
        None,
        &host.alice.agent_id,
    )
    .await;
    assert_eq!(
        status2,
        StatusCode::OK,
        "legacy replay accepted under REQUIRE_NONCE=0 (documented permissive)"
    );
}
