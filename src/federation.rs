// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Federation autonomy — wires the quorum primitives from `replication`
//! into the HTTP write path (v0.7 track C, PR 2 of N).
//!
//! ## Contract
//!
//! When the `ai-memory serve` daemon is started with `--quorum-writes N`
//! and `--quorum-peers <url1,url2,…>`, every successful HTTP write
//! fans out a 1-memory `/api/v1/sync/push` POST to each peer and counts
//! 2xx responses as acks. The write returns OK to the HTTP caller only
//! once the local commit plus `W - 1` peer acks land within the
//! `--quorum-timeout-ms` deadline. Fewer acks → `503` with body
//! `{"error":"quorum_not_met", "got":X, "needed":Y, "reason":…}`.
//!
//! ## Scope of this module
//!
//! - `FederationConfig` — the serve-time config parsed from CLI flags.
//! - `broadcast_store_quorum` — async HTTP fan-out that builds an
//!   `AckTracker` from `replication::QuorumPolicy`, spawns one task
//!   per peer, and waits on either quorum-met or deadline.
//! - Mock-peer integration tests covering the happy path, a dropped
//!   ack pattern, and a total outage.
//!
//! ## NOT in scope of this module
//!
//! - The real multi-process chaos harness lives under `packaging/chaos/`
//!   as an operator-facing shell script. A campaign report is produced
//!   by `packaging/chaos/run-chaos.sh` — see that file for how to
//!   measure the convergence bound committed to in ADR-0001.
//! - MCP-over-stdio and CLI writes do NOT fan out to peers. The MCP
//!   server is a single-tenant stdio client and the CLI is local; both
//!   rely on the sync-daemon for eventual propagation. Only the HTTP
//!   daemon is a federation node.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::models::Memory;
use crate::replication::{AckTracker, QuorumError, QuorumFailureReason, QuorumPolicy};

/// Configured-at-serve federation state. Parsed from
/// `--quorum-writes` + `--quorum-peers` + `--quorum-timeout-ms`.
#[derive(Clone)]
pub struct FederationConfig {
    pub policy: QuorumPolicy,
    pub peers: Vec<PeerEndpoint>,
    pub client: reqwest::Client,
    pub sender_agent_id: String,
}

/// A single peer in the quorum mesh. The `id` is what we record in
/// the ack tracker (typically the URL or the peer's mTLS fingerprint).
#[derive(Clone, Debug)]
pub struct PeerEndpoint {
    pub id: String,
    pub sync_push_url: String,
}

impl FederationConfig {
    /// Build a `FederationConfig` from the serve-time CLI flags. Returns
    /// `None` when federation is disabled (`quorum_writes == 0` or the
    /// peer list is empty).
    ///
    /// # Errors
    ///
    /// Returns an error if the reqwest client cannot be constructed
    /// with the supplied certificate material.
    pub fn build(
        quorum_writes: usize,
        peer_urls: &[String],
        timeout: Duration,
        client_cert_path: Option<&std::path::Path>,
        client_key_path: Option<&std::path::Path>,
        sender_agent_id: String,
    ) -> anyhow::Result<Option<Self>> {
        if quorum_writes == 0 || peer_urls.is_empty() {
            return Ok(None);
        }
        let n = 1 + peer_urls.len(); // local node + remotes
        let policy = QuorumPolicy::new(n, quorum_writes, timeout, Duration::from_secs(30))
            .map_err(|e| anyhow::anyhow!("invalid quorum policy: {e}"))?;
        let peers: Vec<PeerEndpoint> = peer_urls
            .iter()
            .enumerate()
            .map(|(i, raw)| {
                // `id` is used as a Prometheus metric label; keep it
                // low-cardinality. The full URL is logged separately.
                // (#304 nit — prior form `peer-{i}:{url}` blew up the
                // label space as deployment size grew.)
                let trimmed = raw.trim_end_matches('/');
                tracing::debug!(
                    target = "federation",
                    peer_index = i,
                    url = trimmed,
                    "registered peer"
                );
                PeerEndpoint {
                    id: format!("peer-{i}"),
                    sync_push_url: format!("{trimmed}/api/v1/sync/push"),
                }
            })
            .collect();

        // Federation client tuning.
        //
        // An earlier PR #314 attempted tight `tcp_keepalive(1s)` +
        // `pool_idle_timeout(5s)` on this builder to close the Phase
        // 4 partition_minority convergence gap. Ship-gate run 21
        // showed that combination caused Phase 4 to hang for 40+
        // minutes — suspected cause was connection-pool churn on the
        // chaos-client's local 3-process mesh exhausting ephemeral
        // ports under continuous close+reopen cycles with the tight
        // keepalive generating probe traffic on every idle socket.
        //
        // Reverted to the conservative-default client here. Partition-
        // recovery under chaos is moved out of the required ship-gate
        // and into an opt-in campaign shape. Real partition resilience
        // is a v0.6.0.1+ investigation with instrumented cycle data
        // (cycles_by_fault now landed in ship-gate, giving us per-cycle
        // visibility the next time we attempt this).
        let mut client_builder = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(2))
            .use_rustls_tls();
        if let (Some(cert), Some(key)) = (client_cert_path, client_key_path) {
            let cert_pem =
                std::fs::read(cert).map_err(|e| anyhow::anyhow!("read --client-cert: {e}"))?;
            let key_pem =
                std::fs::read(key).map_err(|e| anyhow::anyhow!("read --client-key: {e}"))?;
            let mut pem = cert_pem;
            pem.extend_from_slice(b"\n");
            pem.extend_from_slice(&key_pem);
            let identity = reqwest::Identity::from_pem(&pem)
                .map_err(|e| anyhow::anyhow!("build mTLS identity: {e}"))?;
            client_builder = client_builder.identity(identity);
        }
        let client = client_builder
            .build()
            .map_err(|e| anyhow::anyhow!("build federation client: {e}"))?;

        Ok(Some(Self {
            policy,
            peers,
            client,
            sender_agent_id,
        }))
    }

    /// Count of peers in the mesh (excludes the local node). Useful for
    /// metrics labels.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}

/// Fan out a just-committed memory to every configured peer. Returns
/// an `AckTracker` whose `finalise()` you then call against the
/// deadline to get the quorum outcome.
///
/// The local node's commit is recorded as soon as this function is
/// called — callers pass in a memory that has already been persisted
/// locally. Roll-back semantics on quorum failure are handled by the
/// caller (see `handlers::create_memory` for the HTTP path contract).
pub async fn broadcast_store_quorum(
    config: &FederationConfig,
    mem: &Memory,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [mem],
        "dry_run": false,
    });

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let id = peer.id.clone();
        let mem_id = mem.id.clone();
        let payload = body.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(&client, &url, &payload, &mem_id).await;
            (id, outcome)
        });
    }

    // Deadline is computed ONCE here and never re-derived inside the
    // loop. The tracker carries the same deadline internally — passing
    // a single `Instant` through avoids the few-millisecond disagreement
    // that previously caused `finalise()` to reject quorums met 1-2 ms
    // earlier. (#299 item 1.)
    let deadline = now + config.policy.ack_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, joins.join_next()).await {
            Ok(Some(Ok((peer_id, AckOutcome::Ack)))) => {
                tracker.lock().await.record_peer_ack(peer_id);
            }
            Ok(Some(Ok((peer_id, AckOutcome::IdDrift)))) => {
                tracker.lock().await.record_id_drift(peer_id);
            }
            Ok(Some(Ok((peer_id, AckOutcome::Fail(reason))))) => {
                tracing::warn!("federation: peer {peer_id} failed for {}: {reason}", mem.id);
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: peer join error: {e}");
            }
            Ok(None) | Err(_) => break, // joinset drained or timed out
        }
        // Early-exit once the tracker says quorum is met — we don't
        // need to wait for stragglers.
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    // v0.6.0 correctness fix: once quorum is met, DETACH the remaining
    // fanouts into a background task so they complete naturally rather
    // than being aborted mid-flight. Ship-gate run 14 showed each peer
    // receiving only ~50% of burst writes under W=2/N=3 — cause: when
    // peer-B won the ack race, `joins.shutdown().await` aborted the
    // in-flight POST to peer-C, which often reached reqwest's connect
    // phase but never delivered the memory. Net effect: every write
    // landed on leader + exactly one peer, leaving the other peer
    // permanently behind until a sync-daemon (not running in the phase-2
    // harness) caught it up.
    //
    // The spawned fanout tasks do NOT hold the tracker Arc (they only
    // capture client/url/payload/id), so letting them outlive this
    // function does not block the `Arc::try_unwrap` below. Errors inside
    // the detached tasks are logged but otherwise ignored — the caller
    // has already met quorum by the time we detach.
    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                match res {
                    Ok((peer_id, AckOutcome::Ack)) => {
                        tracing::debug!("federation: post-quorum ack from {peer_id}");
                    }
                    Ok((peer_id, AckOutcome::IdDrift)) => {
                        tracing::warn!(
                            "federation: post-quorum id-drift from {peer_id} (peer rewrote id)"
                        );
                    }
                    Ok((peer_id, AckOutcome::Fail(reason))) => {
                        tracing::debug!(
                            "federation: post-quorum peer {peer_id} did not ack: {reason}"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("federation: post-quorum join error: {e}");
                    }
                }
            }
        });
    }

    let tracker = Arc::try_unwrap(tracker)
        .map_err(|_| QuorumError::LocalWriteFailed {
            detail: "tracker arc still referenced at finalise".to_string(),
        })?
        .into_inner();
    Ok(tracker)
}

#[derive(Debug)]
enum AckOutcome {
    Ack,
    IdDrift,
    Fail(String),
}

async fn post_and_classify(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    expected_id: &str,
) -> AckOutcome {
    match client.post(url).json(body).send().await {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    // sync_push responses don't echo per-memory ids; any
                    // success on a 1-memory push is treated as an ack
                    // unless the response carries an explicit `ids` array
                    // whose content disagrees.
                    if let Some(ids) = v.get("ids").and_then(|v| v.as_array())
                        && !ids.is_empty()
                        && !ids.iter().any(|x| x.as_str() == Some(expected_id))
                    {
                        return AckOutcome::IdDrift;
                    }
                    AckOutcome::Ack
                }
                Err(_) => AckOutcome::Ack, // body unparseable but 2xx = ack
            }
        }
        Ok(resp) => AckOutcome::Fail(format!("http {}", resp.status())),
        Err(e) => AckOutcome::Fail(format!("network: {e}")),
    }
}

/// Classify an `AckTracker` into either a committed quorum (`Ok(n)`) or
/// an error with a reason suitable for the `/503 quorum_not_met`
/// payload. Consumes the tracker — call after the broadcast loop.
///
/// # Errors
///
/// Returns `QuorumError::QuorumNotMet` if the tracker did not meet
/// its W threshold by the `now` tick.
pub fn finalise_quorum(tracker: &AckTracker) -> Result<usize, QuorumError> {
    tracker.finalise(Instant::now())
}

/// Serialised 503 payload for failed quorum writes.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuorumNotMetPayload {
    pub error: &'static str,
    pub got: usize,
    pub needed: usize,
    pub reason: String,
}

impl QuorumNotMetPayload {
    #[must_use]
    pub fn from_err(err: &QuorumError) -> Self {
        match err {
            QuorumError::QuorumNotMet {
                got,
                needed,
                reason,
            } => Self {
                error: "quorum_not_met",
                got: *got,
                needed: *needed,
                // InFlight shouldn't surface in the HTTP payload — the
                // broadcast loop waits until the deadline before
                // calling finalise(). If a caller somehow gets it here,
                // we map to "timeout" for the operator-facing 503 so
                // we don't leak a transient internal state as a fourth
                // public string.
                reason: match reason {
                    QuorumFailureReason::Unreachable => "unreachable".to_string(),
                    QuorumFailureReason::Timeout | QuorumFailureReason::InFlight => {
                        "timeout".to_string()
                    }
                    QuorumFailureReason::IdDrift => "id_drift".to_string(),
                },
            },
            QuorumError::InvalidPolicy { detail } => Self {
                error: "quorum_not_met",
                got: 0,
                needed: 0,
                reason: format!("invalid_policy:{detail}"),
            },
            QuorumError::LocalWriteFailed { detail } => Self {
                error: "quorum_not_met",
                got: 0,
                needed: 0,
                reason: format!("local_write_failed:{detail}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::Json as AxumJson;
    use axum::http::StatusCode;
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    fn sample_memory() -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: "fed-test".to_string(),
            tier: crate::models::Tier::Mid,
            namespace: "app".to_string(),
            title: "hello".to_string(),
            content: "world for federation test".to_string(),
            tags: vec!["t".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id":"ai:test"}),
        }
    }

    #[derive(Clone, Copy)]
    enum MockBehaviour {
        Ack,
        Fail,
        Hang,
    }

    #[derive(Clone)]
    struct MockState {
        behaviour: MockBehaviour,
        count: Arc<AtomicUsize>,
    }

    async fn mock_handler(
        axum::extract::State(state): axum::extract::State<MockState>,
        AxumJson(_body): AxumJson<serde_json::Value>,
    ) -> (StatusCode, AxumJson<serde_json::Value>) {
        state.count.fetch_add(1, Ordering::Relaxed);
        match state.behaviour {
            MockBehaviour::Ack => (
                StatusCode::OK,
                AxumJson(serde_json::json!({"applied":1,"noop":0,"skipped":0})),
            ),
            MockBehaviour::Fail => (
                StatusCode::INTERNAL_SERVER_ERROR,
                AxumJson(serde_json::json!({"error":"stub failure"})),
            ),
            MockBehaviour::Hang => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                (StatusCode::OK, AxumJson(serde_json::json!({"applied":1})))
            }
        }
    }

    async fn spawn_mock_peer(behaviour: MockBehaviour) -> (String, Arc<AtomicUsize>) {
        let call_count = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            behaviour,
            count: call_count.clone(),
        };
        let app = Router::new()
            .route("/api/v1/sync/push", post(mock_handler))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), call_count)
    }

    fn build_config(peers: Vec<String>, w: usize, timeout_ms: u64) -> FederationConfig {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .unwrap();
        let n = 1 + peers.len();
        FederationConfig {
            policy: QuorumPolicy::new(
                n,
                w,
                Duration::from_millis(timeout_ms),
                Duration::from_secs(30),
            )
            .unwrap(),
            peers: peers
                .into_iter()
                .enumerate()
                .map(|(i, url)| PeerEndpoint {
                    id: format!("peer-{i}:{url}"),
                    sync_push_url: format!("{url}/api/v1/sync/push"),
                })
                .collect(),
            client,
            sender_agent_id: "ai:fed-test".to_string(),
        }
    }

    #[tokio::test]
    async fn happy_path_two_peers_quorum_met() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        let result = finalise_quorum(&tracker);
        assert!(result.is_ok(), "expected quorum met, got {result:?}");
        // At least one peer called before quorum returned. With v0.6.0's
        // post-quorum detach, additional fan-outs complete in the
        // background and may or may not have landed by the time this
        // assertion runs — the synchronous contract is only "≥ 1 peer
        // acked before return".
        let calls = count1.load(Ordering::Relaxed) + count2.load(Ordering::Relaxed);
        assert!(calls >= 1);
    }

    #[tokio::test]
    async fn post_quorum_fanout_reaches_all_peers() {
        // Contract: once quorum is met, the background detach must still
        // deliver the write to every peer. Ship-gate run 14 uncovered the
        // prior abort-on-quorum regression that left one peer permanently
        // missing ~50% of burst writes under W=2/N=3.
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let _tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        // Give the detached fanout a slow path to complete. Mock handlers
        // are in-process, so 200ms is comfortable without being flaky.
        for _ in 0..20 {
            if count1.load(Ordering::Relaxed) == 1 && count2.load(Ordering::Relaxed) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            count1.load(Ordering::Relaxed),
            1,
            "peer-1 must receive the write post-quorum"
        );
        assert_eq!(
            count2.load(Ordering::Relaxed),
            1,
            "peer-2 must receive the write post-quorum"
        );
    }

    #[tokio::test]
    async fn partition_minority_fails_quorum() {
        // N = 3, W = 3. Two peers fail → cannot meet quorum.
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        match err {
            QuorumError::QuorumNotMet { got, needed, .. } => {
                assert_eq!(got, 1, "local commit only");
                assert_eq!(needed, 3);
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_on_hanging_peer_classified_timeout() {
        // N = 2, W = 2. One hanging peer → timeout before ack.
        let (url1, _) = spawn_mock_peer(MockBehaviour::Hang).await;
        let cfg = build_config(vec![url1], 2, 200);
        let tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        // Ensure the deadline passed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = finalise_quorum(&tracker).unwrap_err();
        match err {
            QuorumError::QuorumNotMet { reason, .. } => {
                assert!(
                    matches!(
                        reason,
                        QuorumFailureReason::Timeout | QuorumFailureReason::Unreachable
                    ),
                    "unexpected reason {reason:?}"
                );
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn majority_quorum_tolerates_one_peer_down() {
        // N = 3, W = 2 (majority). One fails, one acks → quorum met.
        let (url_up, _) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url_down, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url_up, url_down], 2, 2000);
        let tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        let result = finalise_quorum(&tracker);
        assert!(
            result.is_ok(),
            "majority should tolerate 1 peer down, got {result:?}"
        );
    }

    #[test]
    fn config_build_disabled_when_w_zero() {
        let cfg = FederationConfig::build(
            0,
            &["http://example.com".to_string()],
            Duration::from_millis(500),
            None,
            None,
            "ai:test".to_string(),
        )
        .unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn config_build_disabled_when_peers_empty() {
        let cfg = FederationConfig::build(
            2,
            &[],
            Duration::from_millis(500),
            None,
            None,
            "ai:test".to_string(),
        )
        .unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn quorum_not_met_payload_from_err() {
        let err = QuorumError::QuorumNotMet {
            got: 1,
            needed: 3,
            reason: QuorumFailureReason::Timeout,
        };
        let payload = QuorumNotMetPayload::from_err(&err);
        assert_eq!(payload.error, "quorum_not_met");
        assert_eq!(payload.got, 1);
        assert_eq!(payload.needed, 3);
        assert_eq!(payload.reason, "timeout");
    }
}
