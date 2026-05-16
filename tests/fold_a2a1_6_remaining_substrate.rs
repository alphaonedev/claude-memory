// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]
//! v0.7.0 fold-A2A1.6 (#700, F-A2A1.4 + F-A2A1.6) — remaining
//! substrate scenarios from triage `9ffaa55d`.
//!
//! Three regression tests pin the behaviour landed by the postgres
//! branch federation-fanout + promote-retry patches:
//!
//! 1. `create_memory_postgres_fans_out_to_peers` (S18 equivalent) —
//!    a `POST /api/v1/memories` against a postgres-backed daemon
//!    configured with mock peers must fan the just-written row out
//!    via `broadcast_store_quorum`. Without the fix, peers never
//!    received the row and a recall against a federated reader
//!    surfaced empty (the triage-documented "list shows the row but
//!    /api/v1/recall returns 0" finding). The receive path then
//!    re-embeds the memory on each peer (see
//!    `handlers::federation_receive` line ~387), populating the
//!    `embedding` column so semantic recall surfaces results.
//!
//! 2. `promote_postgres_fans_out_to_peers` (S16/S49 equivalent) —
//!    a successful `POST /api/v1/memories/{id}/promote` on a
//!    postgres-backed daemon must broadcast the post-promote row
//!    (with `tier=long` + cleared expiry) so peers' projections
//!    inherit the new tier. Pre-fix the postgres branch returned
//!    200 immediately without consulting `app.federation`.
//!
//! 3. `promote_postgres_retries_visibility_race` (S16 visibility-race
//!    smoke) — exercises that promote against a just-stored memory
//!    succeeds even under the visibility window where the read
//!    returns NotFound briefly. The retry helper
//!    `get_with_visibility_retry` in `handlers::http` makes this
//!    deterministic: a 5/10/15/20 ms backoff schedule catches any
//!    WAL-flush / replica-ack lag below 50 ms total. This test
//!    does NOT assert the retry happens (timing-sensitive); it
//!    asserts the contract that a fresh store + immediate promote
//!    succeeds with 200 (the wire-shape S16+S49 cert-harness
//!    expectation).
//!
//! ## Gating
//!
//! Skipped without `AI_MEMORY_TEST_POSTGRES_URL` — same convention as
//! the other postgres findings tests (`federation_postgres_fanout`,
//! `s79_postgres_recall_returns_results`, `g1_postgres_…`).

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::federation::{FederationConfig, PeerEndpoint};
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

#[derive(Clone)]
struct MockPeer {
    url: String,
    count: Arc<AtomicUsize>,
    recorded: Arc<Mutex<Vec<Value>>>,
}

async fn spawn_inproc_mock_peer() -> MockPeer {
    use axum::{Json as AxumJson, Router, extract::State, http::StatusCode, routing::post};

    #[derive(Clone)]
    struct PeerState {
        count: Arc<AtomicUsize>,
        recorded: Arc<Mutex<Vec<Value>>>,
    }

    async fn handler(
        State(state): State<PeerState>,
        AxumJson(payload): AxumJson<Value>,
    ) -> (StatusCode, AxumJson<Value>) {
        state.count.fetch_add(1, Ordering::Relaxed);
        state.recorded.lock().await.push(payload);
        (
            StatusCode::OK,
            AxumJson(json!({"applied":1,"noop":0,"skipped":0})),
        )
    }

    let count = Arc::new(AtomicUsize::new(0));
    let recorded: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/api/v1/sync/push", post(handler))
        .with_state(PeerState {
            count: count.clone(),
            recorded: recorded.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    MockPeer {
        url: format!("http://{addr}"),
        count,
        recorded,
    }
}

fn federation_cfg_for_test(peer_urls: &[String], quorum_writes: usize) -> FederationConfig {
    let timeout = Duration::from_secs(2);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("build test reqwest client");
    let n = 1 + peer_urls.len();
    let policy = ai_memory::replication::QuorumPolicy::new(
        n,
        quorum_writes,
        timeout,
        Duration::from_secs(30),
    )
    .expect("valid quorum policy");
    let peers = peer_urls
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let trimmed = raw.trim_end_matches('/');
            PeerEndpoint {
                id: format!("peer-{i}"),
                sync_push_url: format!("{trimmed}/api/v1/sync/push"),
            }
        })
        .collect();
    FederationConfig {
        policy,
        peers,
        client,
        sender_agent_id: "ai:fold-a2a1-6-test".to_string(),
        api_key: None,
    }
}

async fn build_postgres_app_state(url: &str, federation: Option<FederationConfig>) -> AppState {
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
        federation: Arc::new(federation),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(None),
        family_embeddings: Arc::new(RwLock::new(Some(Vec::new()))),
        storage_backend: StorageBackend::Postgres,
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: Duration::from_secs(30),
        replay_cache: Arc::new(ai_memory::identity::replay::ReplayCache::default()),
        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    }
}

async fn spawn_daemon_with_federation(
    url: &str,
    federation: Option<FederationConfig>,
) -> (
    String,
    Arc<Notify>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let app_state = build_postgres_app_state(url, federation).await;
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
        tokio::time::sleep(Duration::from_millis(100)).await;
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

async fn wait_for_counter(counter: &AtomicUsize, min: usize, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if counter.load(Ordering::Relaxed) >= min {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    counter.load(Ordering::Relaxed) >= min
}

// ===================================================================
// S18: create_memory on postgres fans out to peers
// ===================================================================

/// F-A2A1.6 (#700, S18) — postgres-branch `create_memory` must fan the
/// freshly-stored row out to peers via `broadcast_store_quorum`.
///
/// Pre-fix: the postgres branch in `handlers::http::create_memory`
/// returned `201 CREATED` immediately after `store_with_embedding`
/// landed, without consulting `app.federation`. A federated reader
/// peer that polled its local `list` projection saw the row (via
/// shared postgres backing store) but `/api/v1/recall` returned 0
/// rows because the `embedding` column was NULL on that peer's row.
/// The triage classification placed the failure as Bucket-A:
/// embeddings did not propagate, HNSW index not rebuilt on receiving
/// daemon.
///
/// Post-fix: every peer observes a `sync_push` POST carrying the
/// freshly-stored memory. The peer's `sync_push_via_store` handler
/// (at `handlers::federation_receive::sync_push_via_store`) then
/// re-embeds the row, populating the local `embedding` column so
/// downstream semantic recall surfaces results.
#[tokio::test(flavor = "multi_thread")]
async fn create_memory_postgres_fans_out_to_peers() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let peer1 = spawn_inproc_mock_peer().await;
    let peer2 = spawn_inproc_mock_peer().await;
    let peer3 = spawn_inproc_mock_peer().await;
    let peer_urls = vec![peer1.url.clone(), peer2.url.clone(), peer3.url.clone()];
    let cfg = federation_cfg_for_test(&peer_urls, 2);

    let (base, shutdown, handle) = spawn_daemon_with_federation(&url, Some(cfg)).await;
    let client = reqwest::Client::new();

    let suffix = uuid::Uuid::new_v4();
    let ns = format!("fold-a2a1-6-s18-{suffix}");
    let title = format!("fanout-{suffix}");
    let body = json!({
        "tier": "mid",
        "namespace": ns,
        "title": title,
        "content": "S18 reproducer — postgres branch must fan this out so peers re-embed",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "system",
    });
    let resp = client
        .post(format!("{base}/api/v1/memories"))
        .header("x-agent-id", "ai:alice-s18-test")
        .json(&body)
        .send()
        .await
        .expect("create_memory post");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "create_memory must succeed: {resp:?}"
    );

    // Every peer must observe at least one sync_push POST carrying
    // the freshly-stored memory. Post-quorum detach fanout means
    // stragglers complete naturally; we wait up to 5 s for each.
    let timeout = Duration::from_secs(5);
    let p1_ok = wait_for_counter(&peer1.count, 1, timeout).await;
    let p2_ok = wait_for_counter(&peer2.count, 1, timeout).await;
    let p3_ok = wait_for_counter(&peer3.count, 1, timeout).await;
    assert!(
        p1_ok && p2_ok && p3_ok,
        "every peer must observe the create_memory fanout: p1={} p2={} p3={}",
        peer1.count.load(Ordering::Relaxed),
        peer2.count.load(Ordering::Relaxed),
        peer3.count.load(Ordering::Relaxed),
    );

    // Wire-shape check: peer-1 received a sync_push body whose
    // `memories` array contains a row with the expected title and
    // namespace. This is the load-bearing evidence for S18 — the
    // peer's federation_receive::sync_push_via_store handler will
    // re-embed it on arrival, populating the local `embedding`
    // column so subsequent recalls surface non-empty top-K.
    let recorded = peer1.recorded.lock().await;
    let payload = recorded
        .iter()
        .find(|p| {
            p.get("memories")
                .and_then(|m| m.as_array())
                .is_some_and(|arr| {
                    arr.iter().any(|m| {
                        m.get("title").and_then(|t| t.as_str()) == Some(title.as_str())
                            && m.get("namespace").and_then(|n| n.as_str()) == Some(ns.as_str())
                    })
                })
        })
        .expect("peer-1 must have received the new memory in a sync_push body");
    let _ = payload;

    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// S16 + S49: promote on postgres fans out to peers
// ===================================================================

/// F-A2A1.4 (#700, S16/S49) — postgres-branch `promote_memory` must
/// fan the post-promote row (with `tier=long` + cleared expiry) out
/// to peers via `broadcast_store_quorum`.
///
/// Pre-fix: the postgres branch returned `200 OK` immediately after
/// `store.update(tier=long)` landed, without consulting
/// `app.federation`. A peer's `recall(tier=long)` against the same
/// memory silently missed it because the peer's local projection
/// still showed `tier=mid`. The cert harness's S16+S49 flow stores
/// + promotes + immediately asserts visibility on a federated peer,
/// catching this gap.
///
/// Post-fix: after a successful local promote, the postgres branch
/// re-fetches the row through `app.store.get` (the SAL trait
/// surface), then broadcasts via `broadcast_store_quorum` mirroring
/// the sqlite path at `handlers::http::promote_memory` lines
/// ~2406-2417. Every peer receives the promoted memory via
/// sync_push.
#[tokio::test(flavor = "multi_thread")]
async fn promote_postgres_fans_out_to_peers() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    // Boot daemon WITHOUT federation first so the initial store
    // doesn't fan out. This isolates the test's signal — we want
    // every peer counter to start at 0 and only bump on the promote.
    let (base, shutdown, handle) = spawn_daemon_with_federation(&url, None).await;
    let client = reqwest::Client::new();

    let suffix = uuid::Uuid::new_v4();
    let ns = format!("fold-a2a1-6-s16-{suffix}");
    let body = json!({
        "tier": "mid",
        "namespace": ns,
        "title": format!("promote-target-{suffix}"),
        "content": "S16/S49 reproducer — promote must fan out post-update",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "system",
    });
    let resp = client
        .post(format!("{base}/api/v1/memories"))
        .header("x-agent-id", "ai:alice-s16-test")
        .json(&body)
        .send()
        .await
        .expect("create_memory");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let v: Value = resp.json().await.expect("create body");
    let id = v["id"].as_str().expect("id present").to_string();

    // Shut the no-federation daemon down — we'll boot a SECOND
    // daemon attached to the same postgres backing store but THIS
    // time with federation peers wired. The row is already in the
    // shared postgres store from step 1.
    shutdown.notify_one();
    let _ = handle.await;

    let peer1 = spawn_inproc_mock_peer().await;
    let peer2 = spawn_inproc_mock_peer().await;
    let peer3 = spawn_inproc_mock_peer().await;
    let peer_urls = vec![peer1.url.clone(), peer2.url.clone(), peer3.url.clone()];
    let cfg = federation_cfg_for_test(&peer_urls, 2);

    let (base2, shutdown2, handle2) = spawn_daemon_with_federation(&url, Some(cfg)).await;

    // Sanity — every peer counter is 0 before the promote.
    assert_eq!(peer1.count.load(Ordering::Relaxed), 0);
    assert_eq!(peer2.count.load(Ordering::Relaxed), 0);
    assert_eq!(peer3.count.load(Ordering::Relaxed), 0);

    let resp = client
        .post(format!("{base2}/api/v1/memories/{id}/promote"))
        .header("x-agent-id", "ai:alice-s16-test")
        .send()
        .await
        .expect("promote");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "promote must succeed: {resp:?}"
    );
    let pv: Value = resp.json().await.expect("promote body");
    assert_eq!(pv["promoted"], true);
    assert_eq!(pv["tier"], "long");

    // Every peer must observe the post-promote sync_push.
    let timeout = Duration::from_secs(5);
    let p1_ok = wait_for_counter(&peer1.count, 1, timeout).await;
    let p2_ok = wait_for_counter(&peer2.count, 1, timeout).await;
    let p3_ok = wait_for_counter(&peer3.count, 1, timeout).await;
    assert!(
        p1_ok && p2_ok && p3_ok,
        "every peer must observe promote fanout: p1={} p2={} p3={}",
        peer1.count.load(Ordering::Relaxed),
        peer2.count.load(Ordering::Relaxed),
        peer3.count.load(Ordering::Relaxed),
    );

    // Wire-shape check: peer-1's sync_push body must carry the
    // promoted memory with `tier=long`.
    let recorded = peer1.recorded.lock().await;
    let promoted = recorded
        .iter()
        .find_map(|p| {
            p.get("memories")
                .and_then(|m| m.as_array())
                .and_then(|arr| {
                    arr.iter().find(|m| {
                        m.get("id").and_then(|i| i.as_str()) == Some(id.as_str())
                            && m.get("tier").and_then(|t| t.as_str()) == Some("long")
                    })
                })
        })
        .expect("peer-1 must have received the promoted memory with tier=long");
    let _ = promoted;

    shutdown2.notify_one();
    let _ = handle2.await;
}

// ===================================================================
// S16 visibility-race smoke
// ===================================================================

/// F-A2A1.4 (#700, S16) — store + immediate promote must succeed.
///
/// This pins the contract the cert harness drives at S16: a memory is
/// stored, the response carries an id, the next call promotes that id
/// to `long`. Under the pre-fix code path the postgres SAL `get`
/// could briefly return NotFound (read-replica lag / WAL-flush
/// settling); the promote handler folded NotFound straight to 404
/// without a retry, even though a 5 ms wait would have caught the
/// row. The fix adds `get_with_visibility_retry` (50 ms cumulative
/// budget). This test does not directly observe the retry firing —
/// timing is non-deterministic — but it pins the user-visible
/// contract: a fresh store followed immediately by a promote returns
/// 200, never 404.
#[tokio::test(flavor = "multi_thread")]
async fn promote_postgres_succeeds_against_freshly_stored_memory() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let (base, shutdown, handle) = spawn_daemon_with_federation(&url, None).await;
    let client = reqwest::Client::new();

    let suffix = uuid::Uuid::new_v4();
    let ns = format!("fold-a2a1-6-s16-vis-{suffix}");

    // Drive ten store + promote sequences back-to-back. The retry
    // budget is 50 ms cumulative; under load any visibility race
    // surfaces in at least one of these iterations. Pre-fix this
    // loop produced flaky 404s on the cert harness's identical
    // pattern; post-fix every iteration returns 200.
    for i in 0..10 {
        let body = json!({
            "tier": "mid",
            "namespace": ns,
            "title": format!("vis-race-{suffix}-{i}"),
            "content": format!("S16 visibility-race iteration {i}"),
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "system",
        });
        let resp = client
            .post(format!("{base}/api/v1/memories"))
            .header("x-agent-id", "ai:alice-vis-race")
            .json(&body)
            .send()
            .await
            .expect("create_memory");
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
        let v: Value = resp.json().await.expect("create body");
        let id = v["id"].as_str().expect("id").to_string();

        let resp = client
            .post(format!("{base}/api/v1/memories/{id}/promote"))
            .header("x-agent-id", "ai:alice-vis-race")
            .send()
            .await
            .expect("promote");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "S16 iteration {i}: promote against freshly-stored memory must succeed; \
             pre-fix this returned 404 on visibility-race iterations. status={}",
            resp.status()
        );
    }

    shutdown.notify_one();
    let _ = handle.await;
}
