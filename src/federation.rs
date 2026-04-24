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

use crate::models::{Memory, MemoryLink, NamespaceMetaEntry, PendingAction, PendingDecision};
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
        ca_cert_path: Option<&std::path::Path>,
        sender_agent_id: String,
    ) -> anyhow::Result<Option<Self>> {
        if quorum_writes == 0 || peer_urls.is_empty() {
            return Ok(None);
        }
        // Ultrareview #341: reject duplicate peer URLs at build time.
        // If the same peer URL appears twice under different indices,
        // both would count as distinct ack sources and the quorum
        // guarantee is violated. Normalize (trim trailing slash,
        // lowercase scheme+host) before comparing.
        let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();
        for raw in peer_urls {
            let normalized = raw.trim_end_matches('/').to_ascii_lowercase();
            if !seen_urls.insert(normalized.clone()) {
                return Err(anyhow::anyhow!(
                    "duplicate peer URL in --quorum-peers: {raw} (normalized: {normalized}) \
                     — duplicates would let a single peer contribute to quorum more than once"
                ));
            }
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
        // --quorum-ca-cert: trust a caller-supplied root CA for outbound
        // federation POSTs. Required whenever peers present a cert NOT
        // rooted in webpki-roots (Mozilla CA bundle) — e.g. a self-
        // signed / ephemeral CA generated for an isolated test fleet.
        // Without this, reqwest's rustls-tls feature (webpki-roots
        // only) rejects the peer cert and every quorum write times
        // out as quorum_not_met. See alphaonedev/ai-memory-mcp#333.
        if let Some(ca_path) = ca_cert_path {
            let ca_pem = std::fs::read(ca_path)
                .map_err(|e| anyhow::anyhow!("read --quorum-ca-cert: {e}"))?;
            let ca = reqwest::Certificate::from_pem(&ca_pem)
                .map_err(|e| anyhow::anyhow!("parse --quorum-ca-cert: {e}"))?;
            client_builder = client_builder.add_root_certificate(ca);
        }
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
            let outcome = post_and_classify(&client, &url, &payload, &mem_id, Some(&mem_id)).await;
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
        // Ultrareview #343: emit a metric on detach-task failures so
        // mesh divergence is observable. The detach task itself is
        // still fire-and-forget — a full shutdown-drain would require
        // plumbing a shared JoinSet into AppState; tracked separately.
        let mem_id = mem.id.clone();
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
                        crate::metrics::registry()
                            .federation_fanout_dropped_total
                            .with_label_values(&["id_drift"])
                            .inc();
                    }
                    Ok((peer_id, AckOutcome::Fail(reason))) => {
                        tracing::warn!(
                            "federation: post-quorum peer {peer_id} did not ack for {mem_id}: {reason}"
                        );
                        crate::metrics::registry()
                            .federation_fanout_dropped_total
                            .with_label_values(&["peer_fail"])
                            .inc();
                    }
                    Err(e) => {
                        tracing::warn!("federation: post-quorum join error for {mem_id}: {e}");
                        crate::metrics::registry()
                            .federation_fanout_dropped_total
                            .with_label_values(&["join_error"])
                            .inc();
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

/// Single-attempt POST to a peer, classifying the response into an
/// `AckOutcome`. No retries — callers that want retry-on-transient-fail
/// should use [`post_and_classify`].
async fn post_once(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    expected_id: &str,
    idempotency_key: Option<&str>,
) -> AckOutcome {
    // Ultrareview #346: attach an idempotency key so peers can dedupe
    // on retry. If a tokio::timeout fires locally but the HTTP POST
    // already reached the peer, the peer applies the write once; a
    // subsequent catchup sync carrying the same memory.id will be a
    // no-op via `insert_if_newer`. The key is set from the outgoing
    // memory id by default, which is stable across retries.
    let mut req = client.post(url).json(body);
    if let Some(key) = idempotency_key {
        req = req.header("Idempotency-Key", key);
    }
    match req.send().await {
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

/// Backoff before the single retry attempt in [`post_and_classify`].
/// Short enough to fit both attempts inside the default 2s ack deadline
/// plus the per-request client timeout; long enough to let a transient
/// peer-side SQLite-mutex contention or network flap clear.
const FANOUT_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// POST to a peer with a single retry on transient failure.
///
/// v0.6.2 Patch 2 (S40): v3r26 hermes-tls scenario-40 had node-2 see
/// 499/500 bulk rows. Same scenario on ironclaw-tls passed 500/500/500.
/// Root cause: under W=2/N=4 quorum the leader returns 200 once two peers
/// ack. The third peer's POST runs in the post-quorum detach task. If
/// that POST fails (transient network flap, peer 5xx under concurrent
/// SQLite-mutex contention, TLS handshake reset), it was previously
/// fire-and-forget — the row stayed permanently missing on that peer
/// until a sync-daemon caught it up. The harness runs no sync daemon,
/// so one missed POST = one permanently missing row.
///
/// Fix: retry once on `AckOutcome::Fail`. The Idempotency-Key header
/// ensures a partial-apply race (peer received the first POST but the
/// response was lost) deduplicates to a no-op on the peer side via
/// `insert_if_newer`. `IdDrift` is NOT retried — it indicates the peer
/// semantically disagreed about the id, not a transient failure, so
/// retrying would just observe the same disagreement.
///
/// Quorum contract is unchanged: callers still observe a single
/// `AckOutcome` per peer, now reflecting the best of two attempts.
async fn post_and_classify(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    expected_id: &str,
    idempotency_key: Option<&str>,
) -> AckOutcome {
    match post_once(client, url, body, expected_id, idempotency_key).await {
        AckOutcome::Ack => AckOutcome::Ack,
        AckOutcome::IdDrift => AckOutcome::IdDrift,
        AckOutcome::Fail(first_reason) => {
            tokio::time::sleep(FANOUT_RETRY_BACKOFF).await;
            match post_once(client, url, body, expected_id, idempotency_key).await {
                AckOutcome::Ack => {
                    tracing::debug!(
                        "federation: peer POST retry succeeded for {expected_id} (first attempt: {first_reason})"
                    );
                    crate::metrics::registry()
                        .federation_fanout_retry_total
                        .with_label_values(&["ok"])
                        .inc();
                    AckOutcome::Ack
                }
                AckOutcome::IdDrift => {
                    crate::metrics::registry()
                        .federation_fanout_retry_total
                        .with_label_values(&["id_drift"])
                        .inc();
                    AckOutcome::IdDrift
                }
                AckOutcome::Fail(retry_reason) => {
                    crate::metrics::registry()
                        .federation_fanout_retry_total
                        .with_label_values(&["fail"])
                        .inc();
                    AckOutcome::Fail(format!("first: {first_reason}; retry: {retry_reason}"))
                }
            }
        }
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

/// Fan out a tombstone for `id` to every configured peer via the extended
/// `sync_push` body (`deletions: [id]`). Same quorum contract as
/// `broadcast_store_quorum`: local delete is recorded immediately, peer acks
/// counted against `policy.write_quorum`, deadline enforced, stragglers
/// detached.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` if the internal tracker Arc cannot
/// be unwrapped (only occurs under a pathological detach race).
pub async fn broadcast_delete_quorum(
    config: &FederationConfig,
    id: &str,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "deletions": [id],
        "dry_run": false,
    });

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target_id = id.to_string();
        joins.spawn(async move {
            let outcome =
                post_and_classify(&client, &url, &payload, &target_id, Some(&target_id)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!("federation: delete peer {peer_id} failed for {id}: {reason}");
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: delete peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum delete peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (S29): fan out a just-archived memory id to every peer. Payload
/// rides on `sync_push` via `archives: [id]`, mirroring the shape used
/// by `broadcast_delete_quorum` for deletions. On the receiving peer,
/// `sync_push` calls `db::archive_memory` to move the row into
/// `archived_memories` — unlike the delete path this is a soft removal
/// (the row remains queryable via `/api/v1/archive`).
///
/// Same quorum contract as `broadcast_store_quorum` / `broadcast_delete_quorum`.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` if the internal tracker Arc cannot
/// be unwrapped (only occurs under a pathological detach race).
pub async fn broadcast_archive_quorum(
    config: &FederationConfig,
    id: &str,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "archives": [id],
        "dry_run": false,
    });

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target_id = id.to_string();
        joins.spawn(async move {
            let outcome =
                post_and_classify(&client, &url, &payload, &target_id, Some(&target_id)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!("federation: archive peer {peer_id} failed for {id}: {reason}");
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: archive peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum archive peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (S29): fan out a just-restored memory id to every peer. Payload
/// rides on `sync_push` via `restores: [id]`, mirroring the shape used by
/// `broadcast_archive_quorum`. On the receiving peer, `sync_push` moves
/// the row from `archived_memories` back into `memories` via
/// `db::restore_archived`. If the peer never saw the archive or the row
/// isn't in its archive table, the sync call no-ops (same missing-on-peer
/// posture used for archives and deletions).
///
/// Same quorum contract as `broadcast_store_quorum` / `broadcast_archive_quorum`.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` if the internal tracker Arc cannot
/// be unwrapped (only occurs under a pathological detach race).
pub async fn broadcast_restore_quorum(
    config: &FederationConfig,
    id: &str,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "restores": [id],
        "dry_run": false,
    });

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target_id = id.to_string();
        joins.spawn(async move {
            let outcome =
                post_and_classify(&client, &url, &payload, &target_id, Some(&target_id)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!("federation: restore peer {peer_id} failed for {id}: {reason}");
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: restore peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum restore peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (#325): fan out a just-committed memory link to every peer.
/// Payload rides on `sync_push` via `links: [link]`. Same quorum contract
/// as `broadcast_store_quorum`.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` if the internal tracker Arc cannot
/// be unwrapped (only occurs under a pathological detach race).
pub async fn broadcast_link_quorum(
    config: &FederationConfig,
    link: &MemoryLink,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "links": [link],
        "dry_run": false,
    });
    let log_id = format!("{}→{}", link.source_id, link.target_id);

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let log_id = log_id.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(&client, &url, &payload, &log_id, Some(&log_id)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!("federation: link peer {peer_id} failed for {log_id}: {reason}");
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: link peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum link peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (#326): fan out a consolidation in a single `sync_push` — the new
/// consolidated memory + the source ids being deleted. Mirrors the local
/// semantics of `db::consolidate` (insert new + delete sources) so peers
/// end up in the same terminal state as the originator.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` on pathological detach race.
pub async fn broadcast_consolidate_quorum(
    config: &FederationConfig,
    new_mem: &Memory,
    source_ids: &[String],
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [new_mem],
        "deletions": source_ids,
        "dry_run": false,
    });

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target_id = new_mem.id.clone();
        joins.spawn(async move {
            let outcome =
                post_and_classify(&client, &url, &payload, &target_id, Some(&target_id)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!(
                    "federation: consolidate peer {peer_id} failed for {}: {reason}",
                    new_mem.id
                );
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: consolidate peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum consolidate peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (S34): fan out a just-created pending-action row to every peer
/// via `sync_push.pendings`. Callers pass the fully-hydrated `PendingAction`
/// read from their local `pending_actions` table so peers can upsert it
/// with the same id / status / approvals tuple the originator has. Mirrors
/// the quorum semantics of `broadcast_store_quorum` — local pending row
/// is already persisted at call time; peer acks are counted against
/// `policy.write_quorum`.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` on pathological detach race.
pub async fn broadcast_pending_quorum(
    config: &FederationConfig,
    pending: &PendingAction,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "pendings": [pending],
        "dry_run": false,
    });

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target_id = pending.id.clone();
        joins.spawn(async move {
            let outcome =
                post_and_classify(&client, &url, &payload, &target_id, Some(&target_id)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!(
                    "federation: pending peer {peer_id} failed for {}: {reason}",
                    pending.id
                );
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: pending peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum pending peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (S34): fan out a pending-action decision (approve/reject) to
/// peers via `sync_push.pending_decisions`. Without this, an approve on
/// node-2 leaves the row in `status='pending'` on node-1 and the caller
/// sees inconsistent governance state across the cluster. Peers apply
/// via `db::decide_pending_action` which is a no-op on already-decided
/// rows — replay-safe.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` on pathological detach race.
pub async fn broadcast_pending_decision_quorum(
    config: &FederationConfig,
    decision: &PendingDecision,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "pending_decisions": [decision],
        "dry_run": false,
    });

    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target_id = decision.id.clone();
        joins.spawn(async move {
            let outcome =
                post_and_classify(&client, &url, &payload, &target_id, Some(&target_id)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!(
                    "federation: pending-decision peer {peer_id} failed for {}: {reason}",
                    decision.id
                );
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: pending-decision peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum pending-decision peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (S35): fan out a `namespace_meta` row (the `(namespace,
/// standard_id, parent_namespace)` tuple set by `set_namespace_standard`)
/// to peers via `sync_push.namespace_meta`. Without this, peers see the
/// standard memory (already fanned out via `broadcast_store_quorum`) but
/// not the meta row tying it to a namespace + parent — so the
/// parent-chain walk on the peer falls through to `auto_detect_parent`
/// and can return a different ancestor than the originator.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` on pathological detach race.
pub async fn broadcast_namespace_meta_quorum(
    config: &FederationConfig,
    entry: &NamespaceMetaEntry,
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "namespace_meta": [entry],
        "dry_run": false,
    });

    let target_id = entry.namespace.clone();
    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target = target_id.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(&client, &url, &payload, &target, Some(&target)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!(
                    "federation: namespace_meta peer {peer_id} failed for {}: {reason}",
                    entry.namespace
                );
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: namespace_meta peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum namespace_meta peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.2 (S35 follow-up): fan out a namespace-standard *clear* to peers
/// via `sync_push.namespace_meta_clears`. PR #363 shipped set-side fanout
/// via `broadcast_namespace_meta_quorum` but left the clear path local-only
/// — alice clearing on node-1 didn't propagate to bob on node-2, so the
/// scenario-35 cross-peer clear assertion failed.
///
/// Same quorum contract as the set broadcast: local-write pre-counted, one
/// POST per peer, `sync_push` bodies stuffed with the list of cleared
/// namespaces, first W-of-N acks win.
///
/// # Errors
///
/// Returns `QuorumError::LocalWriteFailed` on pathological detach race.
pub async fn broadcast_namespace_meta_clear_quorum(
    config: &FederationConfig,
    namespaces: &[String],
) -> Result<AckTracker, QuorumError> {
    let now = Instant::now();
    let tracker = Arc::new(Mutex::new(AckTracker::new(config.policy.clone(), now)));
    tracker.lock().await.record_local();

    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": [],
        "namespace_meta_clears": namespaces,
        "dry_run": false,
    });

    // Use the joined namespace list as the ack-classifier's `target_id` so
    // post-quorum logs carry enough context to trace back to the operation.
    let target_id = namespaces.join(",");
    let mut joins: JoinSet<(String, AckOutcome)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let peer_id = peer.id.clone();
        let payload = body.clone();
        let target = target_id.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(&client, &url, &payload, &target, Some(&target)).await;
            (peer_id, outcome)
        });
    }

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
                tracing::warn!(
                    "federation: namespace_meta_clear peer {peer_id} failed for [{}]: {reason}",
                    target_id
                );
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("federation: namespace_meta_clear peer join error: {e}");
            }
            Ok(None) | Err(_) => break,
        }
        if tracker.lock().await.is_quorum_met(Instant::now()) {
            break;
        }
    }

    if !joins.is_empty() {
        tokio::spawn(async move {
            while let Some(res) = joins.join_next().await {
                if let Ok((peer_id, AckOutcome::Fail(reason))) = res {
                    tracing::debug!(
                        "federation: post-quorum namespace_meta_clear peer {peer_id} did not ack: {reason}"
                    );
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

/// v0.6.0.1 (#320) — post-partition catchup poller.
///
/// Previously a node rejoining the mesh after SIGSTOP / network blip / restart
/// would only receive NEW writes that arrived AFTER resume; anything the
/// other peers wrote during the outage stayed on those peers. r14 scenario-14
/// observed this as node-3 seeing 2/20 writes post-SIGCONT.
///
/// This loop periodically calls `GET /api/v1/sync/since?peer=<local>` against
/// each configured peer, applying returned memories via `insert_if_newer`.
/// The `since` value is the receiver-side vector clock entry for that peer,
/// so we never re-pull already-applied rows. First catchup after a restart
/// runs with `since=None`, pulling a capped snapshot (limit=500).
///
/// Interval is operator-tunable via `--catchup-interval-secs`. 0 disables.
/// The loop is a best-effort background task: errors are logged but never
/// propagated. In the happy path a partitioned node converges within one
/// interval after resume.
///
/// This is deliberately NOT a substitute for the synchronous quorum-write
/// path — it's a safety net for the tail. Normal writes still fan out via
/// `broadcast_store_quorum`; catchup only fires for rows that DIDN'T land
/// during the original write deadline.
pub fn spawn_catchup_loop(
    config: FederationConfig,
    db: crate::handlers::Db,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Small upfront delay so the first catchup doesn't fire before the
        // HTTP server has bound — avoids spurious "connection refused" on
        // node-1 during rolling start of a fresh cluster.
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            catchup_once(&config, &db).await;
            tokio::time::sleep(interval).await;
        }
    })
}

async fn catchup_once(config: &FederationConfig, db: &crate::handlers::Db) {
    let local_id = config.sender_agent_id.clone();
    for peer in &config.peers {
        // Rebuild the peer's base URL from sync_push_url to get the
        // /api/v1/sync/since endpoint without recomputing peer config.
        let base = peer
            .sync_push_url
            .trim_end_matches("/api/v1/sync/push")
            .to_string();

        // Load our local vector-clock entry for this peer so we only pull
        // the delta. First-time-ever runs with no prior clock pull a full
        // snapshot (capped below by ?limit=500 on the peer side).
        let since_opt: Option<String> = {
            let lock = db.lock().await;
            match crate::db::sync_state_load(&lock.0, &local_id) {
                Ok(clock) => clock.entries.get(&peer.id).cloned(),
                Err(_) => None,
            }
        };

        let url = match since_opt.as_deref() {
            Some(s) => format!(
                "{base}/api/v1/sync/since?since={}&peer={local_id}",
                urlencoding_encode(s)
            ),
            None => format!("{base}/api/v1/sync/since?peer={local_id}"),
        };

        let resp = match config.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::debug!(
                    "catchup: peer {} returned HTTP {} — skipping this tick",
                    peer.id,
                    r.status()
                );
                continue;
            }
            Err(e) => {
                tracing::debug!("catchup: peer {} unreachable: {e}", peer.id);
                continue;
            }
        };

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("catchup: peer {} returned unparseable body: {e}", peer.id);
                continue;
            }
        };

        let memories = match body.get("memories").and_then(|v| v.as_array()) {
            Some(arr) => arr.clone(),
            None => continue,
        };

        if memories.is_empty() {
            continue;
        }

        let mut applied = 0usize;
        let mut latest_ts: Option<String> = None;
        {
            let lock = db.lock().await;
            for raw in &memories {
                let mem: crate::models::Memory = match serde_json::from_value(raw.clone()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("catchup: unparseable memory from peer {}: {e}", peer.id);
                        continue;
                    }
                };
                if crate::validate::validate_memory(&mem).is_err() {
                    continue;
                }
                if latest_ts
                    .as_deref()
                    .is_none_or(|cur| mem.updated_at.as_str() > cur)
                {
                    latest_ts = Some(mem.updated_at.clone());
                }
                if crate::db::insert_if_newer(&lock.0, &mem).is_ok() {
                    applied += 1;
                }
            }
            if let Some(ts) = latest_ts.as_deref()
                && let Err(e) = crate::db::sync_state_observe(&lock.0, &local_id, &peer.id, ts)
            {
                tracing::warn!("catchup: sync_state_observe failed for {}: {e}", peer.id);
            }
        }

        if applied > 0 {
            tracing::info!(
                "catchup: applied {applied} memories from peer {} (since={})",
                peer.id,
                since_opt.as_deref().unwrap_or("<full-snapshot>"),
            );
        }
    }
}

// Minimal RFC 3986 percent-encoder for the `since` timestamp. Only covers
// what RFC 3339 + our namespace/id charsets can produce. We intentionally
// avoid pulling in a url-encoding crate for a 12-character string.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 6);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
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
        /// Return HTTP 500 on the first `fail_until` calls, then 200.
        /// Used to exercise the S40 retry-once path.
        FailThenAck {
            fail_until: usize,
        },
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
        let call = state.count.fetch_add(1, Ordering::Relaxed) + 1;
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
            MockBehaviour::FailThenAck { fail_until } => {
                if call <= fail_until {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        AxumJson(serde_json::json!({"error":"stub transient failure"})),
                    )
                } else {
                    (
                        StatusCode::OK,
                        AxumJson(serde_json::json!({"applied":1,"noop":0,"skipped":0})),
                    )
                }
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
    async fn transient_peer_failure_is_retried_once() {
        // S40 regression guard: a transient 5xx from a peer on the
        // first POST must be retried exactly once. Previously the post
        // was fire-and-forget — one peer that 5xx'd a single bulk row
        // left that row permanently missing on that peer (v3r26
        // hermes-tls scenario-40: node-2 saw 499/500).
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::FailThenAck { fail_until: 1 }).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let _tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        // Retry backoff is 250ms + retry round-trip; poll up to 2s.
        for _ in 0..200 {
            if count1.load(Ordering::Relaxed) >= 1 && count2.load(Ordering::Relaxed) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            count1.load(Ordering::Relaxed),
            1,
            "peer-1 acked first time, no retry"
        );
        assert_eq!(
            count2.load(Ordering::Relaxed),
            2,
            "peer-2 must see exactly two attempts (first fail, retry ack)"
        );
    }

    #[tokio::test]
    async fn persistent_peer_failure_stops_after_one_retry() {
        // Retry policy is exactly one retry — a peer that stays down
        // must NOT be called more than twice per row (no infinite
        // backoff, no thundering herd on a wedged peer).
        let (url1, _) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let _tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        // Wait long enough that any further retries would have fired.
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            count2.load(Ordering::Relaxed),
            2,
            "persistently-failing peer must be called exactly twice (1 + 1 retry)"
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

    // --- broadcast_archive_quorum tests (S29) ---

    #[tokio::test]
    async fn archive_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_archive_quorum(&cfg, "mem-s29").await.unwrap();
        let result = finalise_quorum(&tracker);
        assert!(result.is_ok(), "expected quorum met, got {result:?}");
        // Let detached fanout complete so both peers are observed.
        for _ in 0..20 {
            if count1.load(Ordering::Relaxed) == 1 && count2.load(Ordering::Relaxed) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(count1.load(Ordering::Relaxed), 1);
        assert_eq!(count2.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn archive_quorum_partition_minority_fails() {
        // N = 3, W = 3. Two peers fail → archive quorum cannot be met.
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_archive_quorum(&cfg, "mem-s29").await.unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        match err {
            QuorumError::QuorumNotMet { got, needed, .. } => {
                assert_eq!(got, 1);
                assert_eq!(needed, 3);
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }
}
