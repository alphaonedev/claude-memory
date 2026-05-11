// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::await_holding_lock, clippy::doc_markdown)]
//! v0.7.0.1 G3 — postgres `verify_link` must verify the signature of a
//! link that was just signed by the same daemon's keypair.
//!
//! Reproducer for HALT finding G3 from
//! `runs/v0.7.0-a2a-cont6-cert-r1b-20260509-2148/findings/HALT.md`.
//!
//! ## What this test asserts
//!
//! 1. Boot an in-process HTTP daemon backed by [`PostgresStore`] with a
//!    valid Ed25519 active keypair so `POST /api/v1/links` produces a
//!    self-signed link (`attest_level=self_signed`,
//!    `signature_present=true`).
//! 2. Seed two memories.
//! 3. POST `/api/v1/links` to create a self-signed link between them.
//! 4. POST `/api/v1/links/verify` with the link triple and assert
//!    `verified=true`.
//!
//! ## Pre-fix behaviour (the failure shape from S52)
//!
//! `verified=false`, `signature_present=true`,
//! `attest_level=self_signed`, with `findings` containing
//! `signature verify failed: Ed25519 signature did not validate`.
//!
//! Root cause: the canonical CBOR payload at sign-time uses the
//! caller-supplied or `chrono::Utc::now()`-derived RFC3339 string with
//! whatever sub-second precision chrono emits (nanoseconds when
//! non-zero). PostgreSQL's `TIMESTAMPTZ` column truncates to
//! microsecond precision; on the verify path the row is read back as
//! `DateTime<Utc>` and re-serialized via `.to_rfc3339()`, producing a
//! microsecond-precision string. The two strings differ by trailing
//! sub-microsecond digits, the canonical CBOR bytes differ, and
//! Ed25519 signature verification rejects the payload.
//!
//! Fix: normalize the timestamp to a single canonical precision on
//! both sign and verify paths. The chosen precision must match what
//! `TIMESTAMPTZ` round-trips losslessly — microseconds.
//!
//! ## Gating
//!
//! Same gates as `serve_postgres_smoke.rs` — `feature = "sal-postgres"`
//! plus `AI_MEMORY_TEST_POSTGRES_URL` set at run time.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::store::MemoryStore;
use ai_memory::store::postgres::PostgresStore;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock};

fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// M9 — process-wide guard around `AI_MEMORY_KEY_DIR` mutation. The CI
/// postgres lane runs with `--test-threads=1` but this lock keeps the
/// guarantee local: a developer running `cargo test --features
/// sal-postgres g3_postgres_` on a fast box can still parallelise this
/// file's tests, and any future test added to this file inherits the
/// same serialization for free.
fn env_var_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Synthesize a fresh Ed25519 keypair for this test invocation, save
/// the public material into a tempdir registered as `AI_MEMORY_KEY_DIR`
/// so the verify path can look the peer up via
/// `lookup_peer_public_key`. Returns the keypair (with private half) +
/// the tempdir handle (must outlive the test body).
///
/// Caller MUST hold `env_var_lock()` before invoking this helper —
/// otherwise the `set_var` below races every other concurrent reader.
fn ephemeral_keypair() -> (
    ai_memory::identity::keypair::AgentKeypair,
    tempfile::TempDir,
) {
    let dir = tempfile::TempDir::new().expect("keys tempdir");

    // SAFETY: caller acquired `env_var_lock` before invoking. The
    // serial-test discipline for cross-file mutation is still enforced
    // by `--test-threads=1` on the postgres-backed CI lane; this
    // in-file mutex defends against intra-file parallel runs.
    unsafe {
        std::env::set_var("AI_MEMORY_KEY_DIR", dir.path());
    }

    let kp =
        ai_memory::identity::keypair::generate("ai:g3-test").expect("generate ed25519 keypair");
    ai_memory::identity::keypair::save(&kp, dir.path()).expect("persist keypair");
    (kp, dir)
}

async fn build_postgres_app_state(
    url: &str,
    keypair: ai_memory::identity::keypair::AgentKeypair,
) -> AppState {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).expect("scratch sqlite");
    let path = std::path::PathBuf::from(":memory:");
    let db: Db = Arc::new(Mutex::new((conn, path, ResolvedTtl::default(), true)));
    let store: Arc<dyn MemoryStore> = Arc::new(
        PostgresStore::connect(url)
            .await
            .expect("connect postgres adapter"),
    );
    AppState {
        db,
        embedder: Arc::new(None),
        vector_index: Arc::new(Mutex::new(None)),
        federation: Arc::new(None),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(Some(keypair)),
        family_embeddings: Arc::new(RwLock::new(Some(Vec::new()))),
        storage_backend: StorageBackend::Postgres,
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
    }
}

async fn spawn_daemon(
    url: &str,
    keypair: ai_memory::identity::keypair::AgentKeypair,
) -> (
    String,
    Arc<Notify>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState { key: None };
    let app_state = build_postgres_app_state(url, keypair).await;
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_daemon = shutdown.clone();
    let addr_for_daemon = addr.clone();
    let handle = tokio::spawn(async move {
        ai_memory::daemon_runtime::serve_http_with_shutdown(
            &addr_for_daemon,
            api_key_state,
            app_state,
            shutdown_for_daemon,
        )
        .await
    });
    let mut ready = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(resp) = reqwest::get(format!("http://{addr}/api/v1/health")).await
            && resp.status() == reqwest::StatusCode::OK
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "postgres-backed serve never became ready");
    (format!("http://{addr}"), shutdown, handle)
}

async fn store_memory(
    client: &reqwest::Client,
    base: &str,
    namespace: &str,
    title: &str,
) -> String {
    let body = json!({
        "tier": "long",
        "namespace": namespace,
        "title": title,
        "content": format!("g3 link verify body for {title}"),
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "system",
    });
    let resp = client
        .post(format!("{base}/api/v1/memories"))
        .json(&body)
        .send()
        .await
        .expect("store POST");
    assert!(
        resp.status().is_success(),
        "store should succeed: status={} body={:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let v: Value = resp.json().await.expect("body");
    v["id"].as_str().expect("id").to_string()
}

/// Direct seed of a signed link with a nanosecond-precision `valid_from`
/// through the SAL trait. Bypasses the HTTP `POST /api/v1/links` path
/// because the JSON body shape doesn't accept a caller-supplied
/// `valid_from` — the bug is on the verify-side timestamp roundtrip, so
/// the seed shape only matters insofar as it forces the
/// nanosecond-precision input that exposes the truncation drift.
///
/// On hosts whose `chrono::Utc::now()` already rounds to microsecond
/// precision (notably macOS via `clock_gettime`), the
/// `chrono::Utc::now()` shape upstream of `link_signed` produces
/// values that survive the TIMESTAMPTZ roundtrip unchanged, masking
/// G3 entirely. Injecting an explicit nanosecond stamp here makes the
/// test deterministic across host clocks.
async fn seed_signed_link_with_nanosecond_valid_from(
    store: &dyn ai_memory::store::MemoryStore,
    kp: &ai_memory::identity::keypair::AgentKeypair,
    src_id: &str,
    dst_id: &str,
) {
    use chrono::TimeZone;
    let valid_from_ns = chrono::Utc
        .timestamp_nanos(1_762_000_000_546_027_123)
        .to_rfc3339();
    let link = ai_memory::models::MemoryLink {
        source_id: src_id.to_string(),
        target_id: dst_id.to_string(),
        relation: "related_to".to_string(),
        created_at: valid_from_ns.clone(),
        valid_from: Some(valid_from_ns),
        valid_until: None,
        observed_by: None,
        signature: None,
    };
    let ctx = ai_memory::store::CallerContext::for_agent(kp.agent_id.clone());
    let attest = store
        .link_signed(&ctx, &link, Some(kp))
        .await
        .expect("link_signed must succeed");
    assert_eq!(
        attest, "self_signed",
        "active keypair must produce a self-signed link"
    );
}

/// G3 reproducer — `POST /api/v1/links/verify` against a freshly-signed
/// link must return `verified=true`.
#[tokio::test(flavor = "multi_thread")]
async fn g3_postgres_verify_link_signed_returns_verified_true() {
    let Some(url) = postgres_url() else {
        eprintln!(
            "skipping g3_postgres_verify_link_signed_returns_verified_true: \
             AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    // M9 — acquire env_var_lock for the duration of the test so the
    // `AI_MEMORY_KEY_DIR` mutation in `ephemeral_keypair` cannot race
    // any sibling test added to this file.
    let _env_g = env_var_lock();
    let (kp, _keys_keep) = ephemeral_keypair();
    let (base, shutdown, handle) = spawn_daemon(&url, kp.clone()).await;
    let client = reqwest::Client::new();

    let suffix = uuid::Uuid::new_v4();
    let ns = format!("g3-verify-{suffix}");
    let src_id = store_memory(&client, &base, &ns, &format!("g3-{suffix}-src")).await;
    let dst_id = store_memory(&client, &base, &ns, &format!("g3-{suffix}-dst")).await;

    // Connect a separate SAL handle for the seed so the in-test
    // injector and the daemon read the same `memory_links` table.
    // The daemon already holds its own `Arc<dyn MemoryStore>`; we
    // instantiate a parallel `PostgresStore` against the same URL —
    // PgPool internally multiplexes on the connection pool so the
    // two handles share storage cleanly.
    let seed_store = PostgresStore::connect(&url)
        .await
        .expect("connect seed store");
    seed_signed_link_with_nanosecond_valid_from(&seed_store, &kp, &src_id, &dst_id).await;

    // Verify via the documented HTTP surface.
    let resp = client
        .post(format!("{base}/api/v1/links/verify"))
        .json(&json!({
            "source_id": src_id,
            "target_id": dst_id,
        }))
        .send()
        .await
        .expect("verify POST");

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(json!({}));
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "verify should return 200: got status={status} body={body}"
    );

    assert_eq!(
        body["signature_present"].as_bool(),
        Some(true),
        "verify must report signature_present=true: {body}"
    );
    assert_eq!(
        body["attest_level"].as_str(),
        Some("self_signed"),
        "verify must report attest_level=self_signed: {body}"
    );
    assert_eq!(
        body["verified"].as_bool(),
        Some(true),
        "G3: verify must return verified=true on a freshly-self-signed \
         link. Pre-fix, the postgres path's `valid_from` TIMESTAMPTZ \
         column truncated sub-microsecond digits at write time and the \
         verify path re-serialized via `.to_rfc3339()` produced a \
         different RFC3339 string than was committed to the canonical \
         CBOR payload. Wire body: {body}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}
