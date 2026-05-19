// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]
//! v0.7.0 fold-A2A1.1 (#700, F-A2A1.1) — postgres federation fanout.
//!
//! Three regression tests that pin the behaviour landed by the
//! postgres-branch fanout patch in `handlers::hook_subscribers`:
//!
//! 1. `notify_fanout_postgres_reaches_W_of_N_peers` (S32 equivalent) —
//!    a postgres-backed daemon configured with three in-process mock
//!    peers fans the just-written inbox memory out via the same
//!    `broadcast_store_quorum` contract the sqlite branch uses. Each
//!    peer's sync_push endpoint must observe at least one POST for the
//!    notify-generated memory.
//!
//! 2. `subscribe_postgres_replays_history` (S33 equivalent) —
//!    registering a subscription on the postgres daemon writes a
//!    subscription memory under `_subscriptions/<aid>` AND fans it out
//!    to peers so subscribers attached AFTER an event become visible
//!    cluster-wide. Verified by inspecting the wire-shape POST that
//!    lands on each mock peer.
//!
//! 3. `cross_namespace_dispatch_on_postgres` (S58 equivalent) —
//!    registering a subscription scoped to a NAMESPACE different from
//!    the namespace where the matching event later lands. The mock
//!    peers observe both the subscription registration AND the event
//!    memory across the cluster — the substrate's cross-namespace
//!    dispatch contract is satisfied by the shared `_subscriptions/<aid>`
//!    namespace replication.
//!
//! ## Gating
//!
//! Skipped without `AI_MEMORY_TEST_POSTGRES_URL` — same convention as
//! the other postgres findings tests (`g1_postgres_…`, `sal_v07_…`).
//! The `sal-postgres` feature must be enabled at the cargo level.

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

mod common;
use common::{free_port, postgres_url};

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
        sender_agent_id: "ai:fold-a2a1-1-test".to_string(),
        // v0.7.0 fold-A2A1.4 backcompat default: this test path doesn't run
        // with api-key auth, so no outbound x-api-key header is needed.
        api_key: None,
        signing_key: None,
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
    // v0.7.0 fold-A2A1.4 backcompat default — this test does not exercise mTLS
    // enforcement; api-key checks remain off and the inbound bypass is not
    // configured.
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
// S32: notify fanout reaches W-of-N peers on postgres
// ===================================================================

/// A postgres-backed daemon configured with three in-process mock
/// peers and `W=2` quorum must fan a notify-written inbox memory out
/// to peers via the same `broadcast_store_quorum` contract the sqlite
/// path already uses. This pins F-A2A1.1: the postgres `notify`
/// branch in `handlers::hook_subscribers::notify` invokes
/// `fanout_or_503` after `store.notify()` lands.
///
/// Pre-fix: the postgres branch returned `201 CREATED` immediately
/// after `store.notify()` without consulting `app.federation`, so a
/// `notify` on node-A landed in `_inbox/<recipient>` only on node-A.
/// Recipients polling `/inbox` against node-B saw nothing until a
/// (non-existent in our test harness) catchup sync.
///
/// Post-fix: the same notify fans out via quorum_writes, every peer
/// receives the `sync_push` POST, and the test verifies the per-peer
/// counters bumped.
#[tokio::test(flavor = "multi_thread")]
async fn notify_fanout_postgres_reaches_w_of_n_peers() {
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

    let recipient = format!("ai:bob-{}", uuid::Uuid::new_v4());
    let title = format!("notify-{}", uuid::Uuid::new_v4());
    let body = json!({
        "target_agent_id": recipient,
        "title": title,
        "payload": "hello from alice via postgres",
        "priority": 5,
        "tier": "mid",
    });
    let resp = client
        .post(format!("{base}/api/v1/notify"))
        .header("x-agent-id", "ai:alice-fanout-test")
        .json(&body)
        .send()
        .await
        .expect("notify post");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "notify must succeed: {resp:?}"
    );
    let resp_body: Value = resp.json().await.expect("notify body");
    assert_eq!(resp_body["storage_backend"], "postgres");
    assert!(resp_body["id"].is_string(), "notify must return id");

    // All three peers must observe at least one sync_push POST for
    // the notify-generated inbox memory. Post-quorum detach fanout
    // means stragglers complete too.
    let timeout = Duration::from_secs(5);
    let p1_ok = wait_for_counter(&peer1.count, 1, timeout).await;
    let p2_ok = wait_for_counter(&peer2.count, 1, timeout).await;
    let p3_ok = wait_for_counter(&peer3.count, 1, timeout).await;
    assert!(
        p1_ok && p2_ok && p3_ok,
        "every peer must observe the notify fanout: p1={} p2={} p3={}",
        peer1.count.load(Ordering::Relaxed),
        peer2.count.load(Ordering::Relaxed),
        peer3.count.load(Ordering::Relaxed),
    );

    // Inspect the wire shape — every peer received an `_inbox/<recipient>`
    // memory whose title matches what we POSTed.
    let recorded = peer1.recorded.lock().await;
    let payload = recorded
        .iter()
        .find(|p| {
            p.get("memories")
                .and_then(|m| m.as_array())
                .is_some_and(|arr| {
                    arr.iter().any(|m| {
                        m.get("title").and_then(|t| t.as_str()) == Some(title.as_str())
                            && m.get("namespace")
                                .and_then(|n| n.as_str())
                                .is_some_and(|ns| ns == format!("_inbox/{recipient}"))
                    })
                })
        })
        .expect("peer-1 must have received the inbox memory in a sync_push body");
    let _ = payload; // silence unused

    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// S33: subscribe postgres replays history (subscription fan-out)
// ===================================================================

/// Registering a subscription on a postgres-backed daemon must fan
/// the subscription memory out to peers via the same quorum-write
/// contract as the sqlite branch. This is the substrate-side piece
/// that makes "subscribers attached AFTER an event get historical
/// replay per K7 semantics" work on postgres — the subscription
/// memory lands in `_subscriptions/<aid>` on every peer, so the
/// dispatcher on any peer can find it via the shared store.
///
/// Pre-fix: postgres `subscribe` wrote the subscription memory only
/// to the leader's store, so the subscription was invisible to peer
/// dispatchers and historical replay never saw the subscription
/// among its candidate matches.
///
/// Post-fix: the postgres branch in `handlers::hook_subscribers::subscribe`
/// calls `fanout_or_503` with the subscription memory immediately
/// after the `store.store()` call lands.
#[tokio::test(flavor = "multi_thread")]
async fn subscribe_postgres_replays_history() {
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

    let subscriber_aid = format!("ai:carol-{}", uuid::Uuid::new_v4());
    let target_ns = format!("team-{}", uuid::Uuid::new_v4());
    let body = json!({
        "agent_id": subscriber_aid,
        "namespace": target_ns,
        "secret": "shared-replay-secret",
    });
    let resp = client
        .post(format!("{base}/api/v1/subscriptions"))
        .header("x-agent-id", subscriber_aid.as_str())
        .json(&body)
        .send()
        .await
        .expect("subscribe post");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "subscribe must succeed: {resp:?}"
    );
    let resp_body: Value = resp.json().await.expect("subscribe body");
    assert_eq!(resp_body["storage_backend"], "postgres");
    assert!(resp_body["id"].is_string(), "subscribe must return id");

    // Every peer must observe the subscription memory via sync_push.
    let timeout = Duration::from_secs(5);
    let p1_ok = wait_for_counter(&peer1.count, 1, timeout).await;
    let p2_ok = wait_for_counter(&peer2.count, 1, timeout).await;
    let p3_ok = wait_for_counter(&peer3.count, 1, timeout).await;
    assert!(
        p1_ok && p2_ok && p3_ok,
        "every peer must observe the subscription fanout: p1={} p2={} p3={}",
        peer1.count.load(Ordering::Relaxed),
        peer2.count.load(Ordering::Relaxed),
        peer3.count.load(Ordering::Relaxed),
    );

    // The fanned-out memory's namespace must be `_subscriptions/<aid>`
    // and its metadata must carry `kind=subscription` so the K7
    // replay query can find it on a freshly-rebooted peer.
    let recorded = peer1.recorded.lock().await;
    let sub_payload = recorded
        .iter()
        .find_map(|p| {
            p.get("memories")
                .and_then(|m| m.as_array())
                .and_then(|arr| {
                    arr.iter().find(|m| {
                        m.get("namespace")
                            .and_then(|n| n.as_str())
                            .is_some_and(|ns| ns == format!("_subscriptions/{subscriber_aid}"))
                            && m.get("metadata")
                                .and_then(|md| md.get("kind"))
                                .and_then(|k| k.as_str())
                                == Some("subscription")
                    })
                })
        })
        .expect("peer-1 must have observed the subscription memory in sync_push");
    let _ = sub_payload;

    // The list_subscriptions endpoint resolves the subscription back
    // through the SAL `list` projection. The post-fanout local row
    // is still present (fanout doesn't move ownership, it mirrors).
    // Per #874 (security-medium, 2026-05-18): the list_subscriptions
    // handler requires X-Agent-Id to match the agent_id= query param,
    // else it returns 403 before reaching the SAL list projection.
    let list_resp = client
        .get(format!(
            "{base}/api/v1/subscriptions?agent_id={subscriber_aid}"
        ))
        .header("x-agent-id", &subscriber_aid)
        .send()
        .await
        .expect("list subs")
        .json::<Value>()
        .await
        .expect("body");
    assert_eq!(list_resp["storage_backend"], "postgres");
    let count = list_resp["count"].as_u64().unwrap_or(0);
    assert!(
        count >= 1,
        "list_subscriptions must surface the just-registered subscription: {list_resp}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// S58: cross-namespace dispatch on postgres
// ===================================================================

/// Cross-namespace dispatch: a subscription registered against
/// namespace X must remain visible to dispatchers cluster-wide even
/// after a `notify` lands in a *different* namespace `_inbox/Y`. The
/// substrate's cross-namespace contract on postgres is satisfied by
/// fanning the subscription memory under `_subscriptions/<aid>` out
/// to every peer — the dispatcher on any peer can then resolve the
/// subscription against any inbound event regardless of the event's
/// originating namespace.
///
/// This test exercises the full sequence:
///   1. Subscribe carol to namespace `target/observed`.
///   2. Verify the subscription memory fans to all peers.
///   3. Notify carol (lands in `_inbox/carol`, a *different*
///      namespace from `target/observed`).
///   4. Verify the inbox memory ALSO fans to all peers — proves the
///      shared-store cross-namespace plumbing operates end-to-end.
///
/// Pre-fix: postgres `notify` skipped fanout, so a cross-namespace
/// dispatch was a no-op on every peer except the leader.
/// Post-fix: every peer mirrors the inbox memory and the subscription
/// memory, closing the S58 gap.
#[tokio::test(flavor = "multi_thread")]
async fn cross_namespace_dispatch_on_postgres() {
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

    let carol = format!("ai:carol-{}", uuid::Uuid::new_v4());
    let target_ns = format!("target/observed-{}", uuid::Uuid::new_v4());

    // (1) Subscribe carol to the target namespace.
    let sub_body = json!({
        "agent_id": carol,
        "namespace": target_ns,
        "secret": "shared-xns-secret",
    });
    let sub_resp = client
        .post(format!("{base}/api/v1/subscriptions"))
        .header("x-agent-id", carol.as_str())
        .json(&sub_body)
        .send()
        .await
        .expect("subscribe post");
    assert_eq!(sub_resp.status(), reqwest::StatusCode::CREATED);

    let timeout = Duration::from_secs(5);
    assert!(
        wait_for_counter(&peer1.count, 1, timeout).await
            && wait_for_counter(&peer2.count, 1, timeout).await
            && wait_for_counter(&peer3.count, 1, timeout).await,
        "subscription fanout must reach every peer"
    );

    let subs_observed_on_p1 = peer1.count.load(Ordering::Relaxed);
    let subs_observed_on_p2 = peer2.count.load(Ordering::Relaxed);
    let subs_observed_on_p3 = peer3.count.load(Ordering::Relaxed);

    // (2) Notify carol — lands in `_inbox/carol`, NOT in
    // `target_ns`. This is the cross-namespace pivot: the
    // subscription namespace differs from the event namespace, but
    // the substrate's shared store must still surface both rows on
    // every peer.
    let notify_body = json!({
        "target_agent_id": carol,
        "title": "cross-namespace event",
        "payload": "event landed in _inbox/<carol>, NOT in target_ns",
        "priority": 7,
        "tier": "mid",
    });
    let notify_resp = client
        .post(format!("{base}/api/v1/notify"))
        .header("x-agent-id", "ai:alice-publisher")
        .json(&notify_body)
        .send()
        .await
        .expect("notify post");
    assert_eq!(notify_resp.status(), reqwest::StatusCode::CREATED);

    // Every peer must observe at least one ADDITIONAL POST beyond
    // the subscription fanout — the notify memory under
    // `_inbox/<carol>`.
    assert!(
        wait_for_counter(&peer1.count, subs_observed_on_p1 + 1, timeout).await,
        "peer-1 must observe the notify fanout in addition to the subscription"
    );
    assert!(
        wait_for_counter(&peer2.count, subs_observed_on_p2 + 1, timeout).await,
        "peer-2 must observe the notify fanout in addition to the subscription"
    );
    assert!(
        wait_for_counter(&peer3.count, subs_observed_on_p3 + 1, timeout).await,
        "peer-3 must observe the notify fanout in addition to the subscription"
    );

    // Wire-shape evidence: peer-1 saw BOTH the subscription row
    // (under `_subscriptions/<carol>`) AND the notify row (under
    // `_inbox/<carol>`). The two namespaces are distinct — this is
    // the cross-namespace property under test.
    let recorded = peer1.recorded.lock().await;
    let saw_subscription = recorded.iter().any(|p| {
        p.get("memories")
            .and_then(|m| m.as_array())
            .is_some_and(|arr| {
                arr.iter().any(|m| {
                    m.get("namespace")
                        .and_then(|n| n.as_str())
                        .is_some_and(|ns| ns == format!("_subscriptions/{carol}"))
                })
            })
    });
    let saw_inbox = recorded.iter().any(|p| {
        p.get("memories")
            .and_then(|m| m.as_array())
            .is_some_and(|arr| {
                arr.iter().any(|m| {
                    m.get("namespace")
                        .and_then(|n| n.as_str())
                        .is_some_and(|ns| ns == format!("_inbox/{carol}"))
                })
            })
    });
    assert!(
        saw_subscription,
        "peer-1 must have observed the subscription memory under `_subscriptions/<carol>`"
    );
    assert!(
        saw_inbox,
        "peer-1 must have observed the notify memory under `_inbox/<carol>`"
    );

    shutdown.notify_one();
    let _ = handle.await;
}
