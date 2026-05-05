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

/// v0.6.2 Patch 2 (S40): post-fanout catchup for `bulk_create`.
///
/// After the per-row `broadcast_store_quorum` fanouts complete, issue a
/// single batched `sync_push` per peer with *every* row the leader just
/// committed. Peer-side `insert_if_newer` is idempotent, so rows that
/// already landed via the per-row fanout are no-ops on the peer; rows
/// that a peer missed (post-quorum detach failure + retry both failed,
/// or post-quorum detach timed out on that peer) are applied.
///
/// ## Why a catchup batch in addition to retry-once?
///
/// v3r26 hermes-tls S40 and v3r27 ironclaw-off S40 both showed a
/// single row missing on one specific peer (499/500) despite the
/// retry-once fix in [`post_and_classify`]. Retry-once is a probability
/// improver, not a guarantee: a peer under sustained SQLite-mutex
/// contention can drop two consecutive POSTs inside the ~250ms retry
/// window. A terminal batched catchup closes that last gap at O(1)
/// extra POST per peer instead of O(N) retries per row.
///
/// ## Safety
///
/// - Idempotent: peer's `insert_if_newer` matches on `id` + `updated_at`
///   and no-ops on already-applied rows.
/// - Quorum contract unchanged: the catchup runs AFTER quorum has been
///   met and the HTTP response shape decided. It cannot weaken any
///   guarantee; it only strengthens eventual consistency.
/// - Non-blocking for caller semantics: errors are logged and returned
///   but the leader still returns 200 to the client. The `bulk_create`
///   HTTP contract only promises local commit + W-1 peer acks, and
///   those have already landed by the time this is called.
///
/// Returns a map of `peer_id -> error string` for peers where the
/// catchup POST itself failed (logged by the caller). A successful
/// catchup POST appears in the map as an empty string or is omitted.
pub async fn bulk_catchup_push(
    config: &FederationConfig,
    memories: &[Memory],
) -> Vec<(String, String)> {
    if memories.is_empty() || config.peers.is_empty() {
        return Vec::new();
    }
    let body = serde_json::json!({
        "sender_agent_id": config.sender_agent_id,
        "memories": memories,
        "dry_run": false,
    });
    let mut joins: JoinSet<(String, Result<(), String>)> = JoinSet::new();
    for peer in &config.peers {
        let client = config.client.clone();
        let url = peer.sync_push_url.clone();
        let id = peer.id.clone();
        let payload = body.clone();
        joins.spawn(async move {
            let mut req = client.post(&url).json(&payload);
            // No Idempotency-Key on the batch — the batch is itself an
            // idempotent replay, and the peer's `insert_if_newer`
            // dedupes per row by (id, updated_at).
            req = req.header("X-Catchup", "bulk");
            let outcome = match req.send().await {
                Ok(resp) if resp.status().is_success() => Ok(()),
                Ok(resp) => Err(format!("http {}", resp.status())),
                Err(e) => Err(format!("network: {e}")),
            };
            (id, outcome)
        });
    }
    let mut errors = Vec::new();
    while let Some(res) = joins.join_next().await {
        match res {
            Ok((peer_id, Err(err))) => {
                tracing::warn!("bulk_catchup_push: peer {peer_id} failed: {err}");
                errors.push((peer_id, err));
            }
            Ok((_, Ok(()))) => {}
            Err(e) => {
                tracing::warn!("bulk_catchup_push: join error: {e:?}");
                errors.push(("unknown".to_string(), e.to_string()));
            }
        }
    }
    errors
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
    async fn bulk_catchup_push_hits_every_peer_once() {
        // S40 catchup: verify the terminal batch POST reaches every
        // peer exactly once, with the full row set in a single request.
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let mems = vec![sample_memory(), sample_memory(), sample_memory()];
        let errors = bulk_catchup_push(&cfg, &mems).await;
        assert!(
            errors.is_empty(),
            "catchup must succeed on healthy peers, got {errors:?}"
        );
        assert_eq!(
            count1.load(Ordering::Relaxed),
            1,
            "peer-1 must receive exactly one catchup batch"
        );
        assert_eq!(
            count2.load(Ordering::Relaxed),
            1,
            "peer-2 must receive exactly one catchup batch"
        );
    }

    #[tokio::test]
    async fn bulk_catchup_push_reports_peer_failures() {
        // Catchup errors must be surfaced to the caller for logging —
        // quorum was already met upstream, so the HTTP contract holds,
        // but the leader should record which peers fell behind.
        let (url1, _) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let mems = vec![sample_memory()];
        let errors = bulk_catchup_push(&cfg, &mems).await;
        assert_eq!(errors.len(), 1, "exactly one peer failed the catchup");
        assert!(
            errors[0].1.contains("500") || errors[0].1.contains("http"),
            "error must name the HTTP failure, got {:?}",
            errors[0]
        );
    }

    #[tokio::test]
    async fn bulk_catchup_push_empty_inputs_are_noop() {
        // No rows + no peers → no work, no panics, no POSTs.
        let cfg = build_config(vec![], 1, 500);
        assert!(bulk_catchup_push(&cfg, &[]).await.is_empty());

        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1], 1, 500);
        assert!(bulk_catchup_push(&cfg, &[]).await.is_empty());
        assert_eq!(
            count1.load(Ordering::Relaxed),
            0,
            "no catchup POST must fire when the row set is empty"
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

    // --- broadcast_delete_quorum tests (Wave 3) ---
    //
    // The delete fanout mirrors the store fanout but rides a `deletions: [id]`
    // payload instead of memory bodies. These two cases hit the entire
    // function body — happy ack loop, deadline check, post-quorum detach,
    // tracker unwrap.

    #[tokio::test]
    async fn delete_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_delete_quorum(&cfg, "mem-del").await.unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn delete_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_delete_quorum(&cfg, "mem-del").await.unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        match err {
            QuorumError::QuorumNotMet { got, needed, .. } => {
                assert_eq!(got, 1);
                assert_eq!(needed, 3);
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }

    // --- broadcast_restore_quorum tests (Wave 3) ---

    #[tokio::test]
    async fn restore_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_restore_quorum(&cfg, "mem-restore").await.unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn restore_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_restore_quorum(&cfg, "mem-restore").await.unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
    }

    // --- broadcast_link_quorum tests (Wave 3) ---

    fn sample_link() -> MemoryLink {
        MemoryLink {
            source_id: "mem-a".to_string(),
            target_id: "mem-b".to_string(),
            relation: "related_to".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            signature: None,
            observed_by: None,
            valid_from: None,
            valid_until: None,
        }
    }

    #[tokio::test]
    async fn link_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_link_quorum(&cfg, &sample_link()).await.unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn link_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_link_quorum(&cfg, &sample_link()).await.unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
    }

    // --- broadcast_consolidate_quorum tests (Wave 3) ---

    #[tokio::test]
    async fn consolidate_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let new_mem = sample_memory();
        let sources = vec!["src-a".to_string(), "src-b".to_string()];
        let tracker = broadcast_consolidate_quorum(&cfg, &new_mem, &sources)
            .await
            .unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn consolidate_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let new_mem = sample_memory();
        let tracker = broadcast_consolidate_quorum(&cfg, &new_mem, &[])
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
    }

    // --- broadcast_pending_quorum tests (Wave 3) ---

    fn sample_pending() -> PendingAction {
        PendingAction {
            id: "pa-1".to_string(),
            action_type: "delete".to_string(),
            memory_id: Some("mem-x".to_string()),
            namespace: "app".to_string(),
            payload: serde_json::json!({}),
            requested_by: "ai:test".to_string(),
            requested_at: chrono::Utc::now().to_rfc3339(),
            status: "pending".to_string(),
            decided_by: None,
            decided_at: None,
            approvals: vec![],
        }
    }

    #[tokio::test]
    async fn pending_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_pending_quorum(&cfg, &sample_pending())
            .await
            .unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn pending_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_pending_quorum(&cfg, &sample_pending())
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
    }

    // --- broadcast_pending_decision_quorum tests (Wave 3) ---

    fn sample_decision() -> PendingDecision {
        PendingDecision {
            id: "pa-1".to_string(),
            approved: true,
            decider: "ai:approver".to_string(),
        }
    }

    #[tokio::test]
    async fn pending_decision_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_pending_decision_quorum(&cfg, &sample_decision())
            .await
            .unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn pending_decision_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_pending_decision_quorum(&cfg, &sample_decision())
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
    }

    // --- broadcast_namespace_meta_quorum tests (Wave 3) ---

    fn sample_namespace_meta() -> NamespaceMetaEntry {
        NamespaceMetaEntry {
            namespace: "app/team".to_string(),
            standard_id: "mem-std-1".to_string(),
            parent_namespace: Some("app".to_string()),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[tokio::test]
    async fn namespace_meta_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let tracker = broadcast_namespace_meta_quorum(&cfg, &sample_namespace_meta())
            .await
            .unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn namespace_meta_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let tracker = broadcast_namespace_meta_quorum(&cfg, &sample_namespace_meta())
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
    }

    // --- broadcast_namespace_meta_clear_quorum tests (Wave 3) ---

    #[tokio::test]
    async fn namespace_meta_clear_quorum_two_peers_ack_meets_quorum() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let namespaces = vec!["app/team".to_string(), "app/other".to_string()];
        let tracker = broadcast_namespace_meta_clear_quorum(&cfg, &namespaces)
            .await
            .unwrap();
        assert!(finalise_quorum(&tracker).is_ok());
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
    async fn namespace_meta_clear_quorum_partition_minority_fails() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 3, 500);
        let namespaces = vec!["app/team".to_string()];
        let tracker = broadcast_namespace_meta_clear_quorum(&cfg, &namespaces)
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
    }

    // --- QuorumNotMetPayload::from_err branch coverage (Wave 3) ---
    //
    // The non-Timeout reasons (Unreachable, IdDrift, InFlight) and the
    // non-QuorumNotMet variants (InvalidPolicy, LocalWriteFailed) were
    // never exercised — `from_err` had only the Timeout path covered.

    #[test]
    fn quorum_not_met_payload_unreachable_reason() {
        let err = QuorumError::QuorumNotMet {
            got: 1,
            needed: 2,
            reason: QuorumFailureReason::Unreachable,
        };
        let payload = QuorumNotMetPayload::from_err(&err);
        assert_eq!(payload.reason, "unreachable");
    }

    #[test]
    fn quorum_not_met_payload_id_drift_reason() {
        let err = QuorumError::QuorumNotMet {
            got: 1,
            needed: 2,
            reason: QuorumFailureReason::IdDrift,
        };
        let payload = QuorumNotMetPayload::from_err(&err);
        assert_eq!(payload.reason, "id_drift");
    }

    #[test]
    fn quorum_not_met_payload_in_flight_reason_maps_to_timeout() {
        // InFlight is a transient internal state; HTTP payload maps it to
        // "timeout" rather than leaking a fourth public reason string.
        let err = QuorumError::QuorumNotMet {
            got: 1,
            needed: 2,
            reason: QuorumFailureReason::InFlight,
        };
        let payload = QuorumNotMetPayload::from_err(&err);
        assert_eq!(payload.reason, "timeout");
    }

    #[test]
    fn quorum_not_met_payload_invalid_policy_branch() {
        let err = QuorumError::InvalidPolicy {
            detail: "bad-thing".to_string(),
        };
        let payload = QuorumNotMetPayload::from_err(&err);
        assert_eq!(payload.error, "quorum_not_met");
        assert_eq!(payload.got, 0);
        assert_eq!(payload.needed, 0);
        assert!(payload.reason.starts_with("invalid_policy:"));
        assert!(payload.reason.contains("bad-thing"));
    }

    #[test]
    fn quorum_not_met_payload_local_write_failed_branch() {
        let err = QuorumError::LocalWriteFailed {
            detail: "disk-full".to_string(),
        };
        let payload = QuorumNotMetPayload::from_err(&err);
        assert_eq!(payload.error, "quorum_not_met");
        assert!(payload.reason.starts_with("local_write_failed:"));
        assert!(payload.reason.contains("disk-full"));
    }

    // --- FederationConfig::build coverage (Wave 3) ---

    #[test]
    fn config_build_constructs_when_w_and_peers_set() {
        let cfg = FederationConfig::build(
            2,
            &[
                "http://peer-a.example/".to_string(),
                "http://peer-b.example".to_string(),
            ],
            Duration::from_millis(500),
            None,
            None,
            None,
            "ai:builder".to_string(),
        )
        .unwrap()
        .expect("config should be Some when w>0 and peers nonempty");
        assert_eq!(cfg.peer_count(), 2);
        assert_eq!(cfg.peers[0].id, "peer-0");
        assert_eq!(cfg.peers[1].id, "peer-1");
        // Trailing slash is stripped during URL normalization.
        assert_eq!(
            cfg.peers[0].sync_push_url,
            "http://peer-a.example/api/v1/sync/push"
        );
        assert_eq!(
            cfg.peers[1].sync_push_url,
            "http://peer-b.example/api/v1/sync/push"
        );
        assert_eq!(cfg.sender_agent_id, "ai:builder");
    }

    #[test]
    fn config_build_rejects_duplicate_peer_urls() {
        let result = FederationConfig::build(
            2,
            &[
                "http://peer.example".to_string(),
                "http://peer.example/".to_string(),
            ],
            Duration::from_millis(500),
            None,
            None,
            None,
            "ai:builder".to_string(),
        );
        let err = match result {
            Ok(_) => panic!("expected duplicate-URL rejection"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate peer URL"),
            "expected duplicate-URL rejection, got {msg:?}"
        );
    }

    #[test]
    fn config_build_rejects_missing_ca_cert_path() {
        // ca_cert_path supplied but file doesn't exist → read error
        let bogus = std::path::PathBuf::from("/definitely/does/not/exist/ca.pem");
        let result = FederationConfig::build(
            2,
            &["http://peer.example".to_string()],
            Duration::from_millis(500),
            None,
            None,
            Some(&bogus),
            "ai:builder".to_string(),
        );
        let err = match result {
            Ok(_) => panic!("expected ca-cert read error"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("read --quorum-ca-cert"),
            "expected ca-cert read error, got {msg:?}"
        );
    }

    #[test]
    fn config_build_rejects_invalid_ca_cert_pem() {
        // Write a non-PEM file and confirm parse-side rejection.
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("not-a-cert.pem");
        std::fs::write(&bad, b"this is not a valid pem certificate").unwrap();
        let result = FederationConfig::build(
            2,
            &["http://peer.example".to_string()],
            Duration::from_millis(500),
            None,
            None,
            Some(&bad),
            "ai:builder".to_string(),
        );
        let err = match result {
            Ok(_) => panic!("expected ca-cert parse error"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("parse --quorum-ca-cert") || msg.contains("--quorum-ca-cert"),
            "expected ca-cert parse error, got {msg:?}"
        );
    }

    #[test]
    fn config_build_rejects_missing_client_cert_path() {
        let bogus_cert = std::path::PathBuf::from("/definitely/missing/cert.pem");
        let bogus_key = std::path::PathBuf::from("/definitely/missing/key.pem");
        let result = FederationConfig::build(
            2,
            &["http://peer.example".to_string()],
            Duration::from_millis(500),
            Some(&bogus_cert),
            Some(&bogus_key),
            None,
            "ai:builder".to_string(),
        );
        let err = match result {
            Ok(_) => panic!("expected client-cert read error"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("read --client-cert"),
            "expected client-cert read error, got {msg:?}"
        );
    }

    #[test]
    fn peer_count_matches_peer_list() {
        let cfg = build_config(
            vec![
                "http://a.example".to_string(),
                "http://b.example".to_string(),
                "http://c.example".to_string(),
            ],
            2,
            500,
        );
        assert_eq!(cfg.peer_count(), 3);
    }

    // --- urlencoding_encode coverage (Wave 3) ---

    #[test]
    fn urlencoding_encode_passthrough_safe_chars() {
        // ASCII alpha-numeric + RFC3986 unreserved (-_.~) pass through.
        let encoded = urlencoding_encode("abcXYZ-09_.~");
        assert_eq!(encoded, "abcXYZ-09_.~");
    }

    #[test]
    fn urlencoding_encode_percent_encodes_reserved_and_high_bits() {
        // Space, colon, plus, slash all get percent-encoded.
        let encoded = urlencoding_encode("2026-04-26T12:00:00+00:00 / x");
        assert!(
            encoded.contains("%3A"),
            "expected colon to be percent-encoded: {encoded}"
        );
        assert!(
            encoded.contains("%2B"),
            "expected + to be percent-encoded: {encoded}"
        );
        assert!(
            encoded.contains("%2F"),
            "expected / to be percent-encoded: {encoded}"
        );
        assert!(
            encoded.contains("%20"),
            "expected space to be percent-encoded: {encoded}"
        );
        // Hyphen IS in the unreserved set → must NOT be percent-encoded.
        assert!(
            !encoded.contains("%2D"),
            "hyphen must pass through unencoded: {encoded}"
        );
    }

    #[test]
    fn urlencoding_encode_empty_string() {
        assert_eq!(urlencoding_encode(""), "");
    }

    // --- broadcast_store_quorum id-drift path (Wave 3) ---
    //
    // The `IdDrift` arm in post_once + broadcast_store_quorum (lines around
    // 243-244 / 362-366) was uncovered. A peer that returns a 200 with an
    // `ids` array NOT containing the expected memory id should be classified
    // as IdDrift, not Ack.

    async fn id_drift_handler(
        AxumJson(_body): AxumJson<serde_json::Value>,
    ) -> (StatusCode, AxumJson<serde_json::Value>) {
        // 200 OK but ids[0] disagrees with the memory the leader sent.
        (
            StatusCode::OK,
            AxumJson(serde_json::json!({"ids": ["some-other-id"], "applied": 1})),
        )
    }

    async fn spawn_id_drift_peer() -> String {
        let app = Router::new().route("/api/v1/sync/push", post(id_drift_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn id_drift_peer_does_not_count_as_ack() {
        // Two peers, both return 200 but with `ids: [other-id]`. Quorum
        // can't be met because neither counts as a peer ack — only the
        // local commit registers.
        let url1 = spawn_id_drift_peer().await;
        let url2 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1, url2], 2, 1000);
        let tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        let result = finalise_quorum(&tracker);
        // With W=2, N=3 (local + 2 peers), local + 0 peer-acks = 1 < 2.
        let err = result.unwrap_err();
        match err {
            QuorumError::QuorumNotMet {
                got,
                needed,
                reason,
            } => {
                assert_eq!(got, 1, "only local should count");
                assert_eq!(needed, 2);
                // IdDrift / Timeout / InFlight are all valid here. The
                // tracker classifies based on whether ANY peer reported a
                // drift (IdDrift), the deadline elapsed first (Timeout),
                // or all peers reported but the deadline still hadn't
                // passed when finalise was called (InFlight). The
                // important invariant is just "peer with drifted ids does
                // NOT count toward quorum".
                assert!(
                    matches!(
                        reason,
                        QuorumFailureReason::IdDrift
                            | QuorumFailureReason::Timeout
                            | QuorumFailureReason::InFlight
                    ),
                    "expected IdDrift / Timeout / InFlight, got {reason:?}"
                );
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // W9 (v0.6.3) — catchup_once + spawn_catchup_loop coverage.
    //
    // Lines 1406-1525 of `federation.rs` were uncovered through W3 because
    // they require a mock peer that serves `/api/v1/sync/since`, plus a
    // real `Db` to track the sync_state vector clock between ticks. We
    // reuse the existing in-process axum mock-peer pattern (see
    // `spawn_mock_peer` above) and a `:memory:` rusqlite handle.
    // -----------------------------------------------------------------

    /// Behaviours the `/api/v1/sync/since` mock peer can take. Each variant
    /// is a single canned response shape — we don't need long-running
    /// stateful peers for catchup coverage because `catchup_once` is a
    /// one-shot function.
    #[derive(Clone)]
    enum SinceMockBehaviour {
        /// Return a 200 with `{ "memories": <list> }` on every call.
        ReturnMemories(Vec<Memory>),
        /// Return a 500 server error.
        Error500,
        /// Sleep `delay` then return memories (used for client-timeout test).
        Hang(Duration),
        /// Return 200 but with a non-JSON body so `resp.json()` fails.
        MalformedBody,
    }

    #[derive(Clone)]
    struct SinceMockState {
        behaviour: SinceMockBehaviour,
        hits: Arc<AtomicUsize>,
        last_since: Arc<Mutex<Option<String>>>,
        last_peer: Arc<Mutex<Option<String>>>,
    }

    async fn since_handler(
        axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
        axum::extract::State(state): axum::extract::State<SinceMockState>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        state.hits.fetch_add(1, Ordering::Relaxed);
        {
            let mut s = state.last_since.lock().await;
            *s = q.get("since").cloned();
        }
        {
            let mut p = state.last_peer.lock().await;
            *p = q.get("peer").cloned();
        }
        match &state.behaviour {
            SinceMockBehaviour::ReturnMemories(mems) => {
                let body = serde_json::json!({"memories": mems});
                (StatusCode::OK, AxumJson(body)).into_response()
            }
            SinceMockBehaviour::Error500 => (
                StatusCode::INTERNAL_SERVER_ERROR,
                AxumJson(serde_json::json!({"error":"oops"})),
            )
                .into_response(),
            SinceMockBehaviour::Hang(d) => {
                tokio::time::sleep(*d).await;
                (
                    StatusCode::OK,
                    AxumJson(serde_json::json!({"memories": []})),
                )
                    .into_response()
            }
            SinceMockBehaviour::MalformedBody => {
                // 200 OK but the body is not JSON — `resp.json::<Value>()`
                // will return an Err on the parse step.
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    "this is not json {{{",
                )
                    .into_response()
            }
        }
    }

    /// Spawn a `/api/v1/sync/since` mock and return its base URL plus the
    /// hit-counter and last-query-param tracker.
    async fn spawn_since_peer(
        behaviour: SinceMockBehaviour,
    ) -> (
        String,
        Arc<AtomicUsize>,
        Arc<Mutex<Option<String>>>,
        Arc<Mutex<Option<String>>>,
    ) {
        let hits = Arc::new(AtomicUsize::new(0));
        let last_since = Arc::new(Mutex::new(None));
        let last_peer = Arc::new(Mutex::new(None));
        let state = SinceMockState {
            behaviour,
            hits: hits.clone(),
            last_since: last_since.clone(),
            last_peer: last_peer.clone(),
        };
        let app = Router::new()
            .route("/api/v1/sync/since", axum::routing::get(since_handler))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), hits, last_since, last_peer)
    }

    /// Build an in-memory `Db` matching `handlers::Db` shape. Catchup only
    /// uses `lock().await.0` (the `Connection`), so the path / TTL / pragma
    /// fields can be defaults.
    fn build_test_db() -> crate::handlers::Db {
        let conn = crate::db::open(std::path::Path::new(":memory:")).unwrap();
        let path = std::path::PathBuf::from(":memory:");
        Arc::new(Mutex::new((
            conn,
            path,
            crate::config::ResolvedTtl::default(),
            true,
        )))
    }

    /// Build a `FederationConfig` whose peer's `id` matches the segment we
    /// pull from sync_state — `peer-0`. This mirrors the production
    /// invariant: the catchup loop keys vector-clock entries by peer.id.
    /// We intentionally use the W9-shape (id = "peer-0") here rather than
    /// the W3-shape ("peer-0:<url>") because `catchup_once`'s url-trim path
    /// depends on the trailing `/api/v1/sync/push` and the id stays opaque
    /// either way — but the simpler shape is also closer to production.
    fn build_catchup_cfg(peer_url: &str, timeout_ms: u64) -> FederationConfig {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .unwrap();
        FederationConfig {
            policy: QuorumPolicy::new(
                2,
                1,
                Duration::from_millis(timeout_ms),
                Duration::from_secs(30),
            )
            .unwrap(),
            peers: vec![PeerEndpoint {
                id: "peer-0".to_string(),
                sync_push_url: format!("{peer_url}/api/v1/sync/push"),
            }],
            client,
            sender_agent_id: "ai:catchup-test".to_string(),
        }
    }

    /// Memory factory dedicated to catchup tests — every memory gets a
    /// unique title so `insert_if_newer`'s ON CONFLICT(title, namespace)
    /// path doesn't collapse them into one row. Timestamp is a fixed
    /// progression so the test asserts deterministic ordering.
    fn catchup_memory(title: &str, updated_at: &str) -> Memory {
        Memory {
            id: format!("cat-{title}"),
            tier: crate::models::Tier::Mid,
            namespace: "catchup".to_string(),
            title: title.to_string(),
            content: format!("content for {title}"),
            tags: vec!["catchup".to_string()],
            priority: 5,
            confidence: 1.0,
            // `validate_memory` enforces a source-allowlist (user, claude,
            // hook, api, cli, import, consolidation, system, chaos, notify).
            // Use "system" so catchup_once's `validate_memory(&mem).is_err()`
            // skip-branch isn't tripped — that's what we're trying NOT to
            // exercise in the happy-path tests below.
            source: "system".to_string(),
            access_count: 0,
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id":"ai:peer-0"}),
        }
    }

    // ---- catchup_once: pulls `since`, advances state ----

    #[tokio::test]
    async fn test_catchup_once_pulls_since_cursor_advances_state() {
        // First-time catchup with empty sync_state: we expect the request
        // to land WITHOUT a `since` query param, and after the call
        // sync_state should be advanced to the latest memory's timestamp.
        let mems = vec![
            catchup_memory("a", "2026-04-26T10:00:00Z"),
            catchup_memory("b", "2026-04-26T10:00:01Z"),
            catchup_memory("c", "2026-04-26T10:00:02Z"),
            catchup_memory("d", "2026-04-26T10:00:03Z"),
            catchup_memory("e", "2026-04-26T10:00:04Z"),
        ];
        let latest_ts = mems.last().unwrap().updated_at.clone();
        let (url, hits, last_since, last_peer) =
            spawn_since_peer(SinceMockBehaviour::ReturnMemories(mems.clone())).await;
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();

        catchup_once(&cfg, &db).await;

        assert_eq!(hits.load(Ordering::Relaxed), 1, "peer hit exactly once");
        // First-time call → no `since` query param.
        assert!(
            last_since.lock().await.is_none(),
            "first catchup must omit since"
        );
        // Local agent id is forwarded.
        assert_eq!(last_peer.lock().await.as_deref(), Some("ai:catchup-test"));
        // sync_state advanced to the latest memory's timestamp.
        let lock = db.lock().await;
        let clock =
            crate::db::sync_state_load(&lock.0, "ai:catchup-test").expect("load sync state");
        assert_eq!(
            clock.entries.get("peer-0").map(String::as_str),
            Some(latest_ts.as_str()),
            "sync state advanced to latest pulled memory's updated_at"
        );
        // All 5 memories landed.
        let count: i64 = lock
            .0
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 5, "all five memories inserted");
    }

    // ---- catchup_once: empty array no-op ----

    #[tokio::test]
    async fn test_catchup_once_no_new_memories_no_op() {
        let (url, hits, _, _) = spawn_since_peer(SinceMockBehaviour::ReturnMemories(vec![])).await;
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();

        catchup_once(&cfg, &db).await;

        assert_eq!(hits.load(Ordering::Relaxed), 1);
        let lock = db.lock().await;
        let clock = crate::db::sync_state_load(&lock.0, "ai:catchup-test").unwrap();
        assert!(
            clock.entries.get("peer-0").is_none(),
            "empty response must not advance sync_state"
        );
        let count: i64 = lock
            .0
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // ---- catchup_once: 5xx error swallowed, state untouched ----

    #[tokio::test]
    async fn test_catchup_once_peer_500_error_logged_no_panic() {
        let (url, hits, _, _) = spawn_since_peer(SinceMockBehaviour::Error500).await;
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();

        // Must NOT panic. The function logs at debug! and continues.
        catchup_once(&cfg, &db).await;

        assert_eq!(hits.load(Ordering::Relaxed), 1);
        let lock = db.lock().await;
        let clock = crate::db::sync_state_load(&lock.0, "ai:catchup-test").unwrap();
        assert!(
            clock.entries.get("peer-0").is_none(),
            "500 must not advance sync state"
        );
    }

    // ---- catchup_once: timeout swallowed ----

    #[tokio::test]
    async fn test_catchup_once_peer_timeout_handled() {
        // Mock hangs for 2s, client timeout is 200ms → reqwest returns Err,
        // catchup logs at debug! and skips this peer.
        let (url, hits, _, _) =
            spawn_since_peer(SinceMockBehaviour::Hang(Duration::from_secs(2))).await;
        let cfg = build_catchup_cfg(&url, 200);
        let db = build_test_db();

        let start = Instant::now();
        catchup_once(&cfg, &db).await;
        let elapsed = start.elapsed();

        // Must return promptly after the client-timeout fires, not after
        // the full 2s mock-side hang.
        assert!(
            elapsed < Duration::from_millis(1500),
            "catchup_once should honour the client timeout, took {elapsed:?}"
        );
        assert_eq!(hits.load(Ordering::Relaxed), 1, "request was sent");
        let lock = db.lock().await;
        let clock = crate::db::sync_state_load(&lock.0, "ai:catchup-test").unwrap();
        assert!(clock.entries.get("peer-0").is_none());
    }

    // ---- catchup_once: malformed JSON body ----

    #[tokio::test]
    async fn test_catchup_once_malformed_response_handled() {
        let (url, hits, _, _) = spawn_since_peer(SinceMockBehaviour::MalformedBody).await;
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();

        // No panic — the function `tracing::warn!`s and skips the peer.
        catchup_once(&cfg, &db).await;

        assert_eq!(hits.load(Ordering::Relaxed), 1);
        let lock = db.lock().await;
        let clock = crate::db::sync_state_load(&lock.0, "ai:catchup-test").unwrap();
        assert!(
            clock.entries.get("peer-0").is_none(),
            "malformed body must not advance sync state"
        );
    }

    // ---- catchup_once: only newer memories overwrite local ----

    #[tokio::test]
    async fn test_catchup_once_inserts_only_newer_memories() {
        // Pre-seed local DB with a memory titled "shared" at t=10:00:01.
        // Mock peer returns:
        //   - "shared" at t=10:00:00  (older — must NOT clobber local)
        //   - "fresh"  at t=10:00:02  (new title — must insert)
        let db = build_test_db();
        {
            let lock = db.lock().await;
            let local = catchup_memory("shared", "2026-04-26T10:00:01Z");
            // Insert via the test path — this is the "we already have it
            // locally at a newer timestamp" precondition.
            crate::db::insert_if_newer(&lock.0, &local).unwrap();
            // Confirm pre-state.
            let cnt: i64 = lock
                .0
                .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .unwrap();
            assert_eq!(cnt, 1, "pre-seeded shared row");
        }

        let mut stale_shared = catchup_memory("shared", "2026-04-26T10:00:00Z");
        // Distinct content so the "did the older catchup body win?" assertion
        // is meaningful — base catchup_memory derives content from title.
        stale_shared.content = "stale-from-catchup-peer".to_string();
        stale_shared.id = "cat-shared-OLD".to_string();
        let stale_shared_content = stale_shared.content.clone();
        let new_fresh = catchup_memory("fresh", "2026-04-26T10:00:02Z");
        let (url, _, _, _) = spawn_since_peer(SinceMockBehaviour::ReturnMemories(vec![
            stale_shared,
            new_fresh,
        ]))
        .await;
        let cfg = build_catchup_cfg(&url, 2000);

        catchup_once(&cfg, &db).await;

        let lock = db.lock().await;
        // Both rows now exist.
        let cnt: i64 = lock
            .0
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 2, "fresh row inserted, shared kept");
        // The "shared" row's content must still be the locally-seeded
        // version (older catchup body did NOT win).
        let shared_content: String = lock
            .0
            .query_row(
                "SELECT content FROM memories WHERE title = 'shared' AND namespace = 'catchup'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            shared_content, stale_shared_content,
            "older catchup memory must NOT overwrite newer local row"
        );
        // sync_state advanced to the LATEST timestamp seen, not to the
        // one we actually applied — function tracks `latest_ts` over the
        // whole batch.
        let clock = crate::db::sync_state_load(&lock.0, "ai:catchup-test").unwrap();
        assert_eq!(
            clock.entries.get("peer-0").map(String::as_str),
            Some("2026-04-26T10:00:02Z"),
        );
    }

    // ---- spawn_catchup_loop: runs at interval (paused-time) ----

    #[tokio::test(start_paused = true)]
    async fn test_spawn_catchup_loop_runs_at_interval() {
        // The loop sleeps 5s up-front then ticks every `interval`. With
        // paused time, advance past the initial sleep and one full tick
        // and assert the mock saw at least one hit.
        let (url, hits, _, _) = spawn_since_peer(SinceMockBehaviour::ReturnMemories(vec![])).await;
        let cfg = build_catchup_cfg(&url, 5000);
        let db = build_test_db();

        let handle = spawn_catchup_loop(cfg, db, Duration::from_secs(60));

        // Advance past the 5s startup delay + give the first catchup_once
        // a slice of real wall-clock to actually execute the network call.
        // Paused time still yields() between awaits; the network IO is
        // not virtualized — so we step in chunks separated by yields.
        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        // Allow the spawned reqwest::send to actually complete on the
        // real runtime — a small real-time wait covers in-process axum
        // round-trip latency without paused-time interference.
        for _ in 0..50 {
            if hits.load(Ordering::Relaxed) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(10)).await;
        }

        assert!(
            hits.load(Ordering::Relaxed) >= 1,
            "first catchup tick must hit the mock peer (got {})",
            hits.load(Ordering::Relaxed),
        );

        handle.abort();
    }

    // ---- spawn_catchup_loop: aborts cleanly on handle drop ----

    #[tokio::test]
    async fn test_spawn_catchup_loop_aborts_cleanly_on_handle_drop() {
        // Drop the JoinHandle (via abort) and confirm the task ends quickly
        // — no lingering tasks, no panics from being killed mid-tick.
        let (url, _, _, _) = spawn_since_peer(SinceMockBehaviour::ReturnMemories(vec![])).await;
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();

        let handle = spawn_catchup_loop(cfg, db, Duration::from_secs(3600));
        // Don't let it run a full 5s startup-sleep. Abort and confirm
        // the join future resolves promptly with a Cancelled error.
        handle.abort();
        let result = tokio::time::timeout(Duration::from_millis(500), handle).await;
        let join = result.expect("aborted handle must resolve within 500ms");
        assert!(
            join.is_err() && join.unwrap_err().is_cancelled(),
            "handle.abort() must surface as is_cancelled() == true"
        );
    }

    // ---- mTLS client-cert flow: build_config happy path ----

    #[test]
    fn test_build_config_mtls_with_valid_files() {
        // Use the existing rcgen-generated test fixtures (PEM cert +
        // PKCS#8 key). The build path concatenates them into one PEM
        // and feeds that to `reqwest::Identity::from_pem`. We only need
        // to assert the client builds — TLS handshake itself isn't part
        // of this path's contract.
        let cert = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_cert.pem");
        let key = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_key_pkcs8.pem");
        // Sanity: fixtures exist on disk.
        assert!(cert.exists(), "missing test fixture: {cert:?}");
        assert!(key.exists(), "missing test fixture: {key:?}");

        let result = FederationConfig::build(
            2,
            &["http://peer.example".to_string()],
            Duration::from_millis(500),
            Some(&cert),
            Some(&key),
            None,
            "ai:builder".to_string(),
        );
        let cfg = match result {
            Ok(Some(c)) => c,
            Ok(None) => panic!("expected Some(FederationConfig), got None"),
            Err(e) => panic!("expected Ok, got Err: {e}"),
        };
        assert_eq!(cfg.peer_count(), 1);
    }

    // ---- mTLS client-cert flow: missing key file errors ----

    #[test]
    fn test_build_config_mtls_with_missing_files_returns_error() {
        // Cert path exists, key path doesn't → the second `read` errors
        // with "read --client-key:". This exercises the second arm of
        // the `(Some(cert), Some(key))` branch that the existing
        // `config_build_rejects_missing_client_cert_path` test (which
        // makes BOTH paths missing) doesn't reach.
        let cert = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_cert.pem");
        let bogus_key = std::path::PathBuf::from("/definitely/missing/key.pem");
        assert!(cert.exists(), "missing test fixture: {cert:?}");

        let result = FederationConfig::build(
            2,
            &["http://peer.example".to_string()],
            Duration::from_millis(500),
            Some(&cert),
            Some(&bogus_key),
            None,
            "ai:builder".to_string(),
        );
        let err = match result {
            Ok(_) => panic!("expected client-key read error"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("read --client-key"),
            "expected client-key read error, got {msg:?}"
        );
    }

    // -----------------------------------------------------------------
    // W12-G (v0.6.3) — federation.rs remaining edges (89.87% → 94%+).
    //
    // Targets the residual uncovered surface after W3 + W9 F9:
    //   - post_and_classify direct: persistent retry-fail and id-drift
    //     skip-retry paths.
    //   - bulk_catchup_push edge cases not previously reached
    //     (no-peers shortcut, mixed pass+fail outcomes).
    //   - Quorum-policy edges: W=1 single-peer-ack already returns,
    //     QuorumPolicy::majority convenience constructor, FederationConfig
    //     duplicate detection on trailing-slash and case differences.
    //   - Each broadcast_*_quorum has only the all-Ack and all-Fail
    //     paths — exercise the `Hang` (timeout-mid-loop) classification
    //     for the remaining variants so the inner `Ok(None) | Err(_)`
    //     break arm is hit on every flavour.
    //   - catchup_once: 5xx classified as "Ok(r) where !success" arm
    //     (F9 covers it once but with peer.id == "peer-0"; the
    //     ServerError + non-empty body path is already covered).
    //     New: peer URL whose `sync_push_url` does NOT carry the
    //     `/api/v1/sync/push` suffix — the trim_end_matches no-ops
    //     and the `since` URL is built from the raw base.
    //   - QuorumNotMetPayload: `from_err` on a peer-acks-empty result
    //     after the deadline (Unreachable variant via real broadcast).
    //
    // All tests reuse the in-process axum mock-peer infrastructure
    // (`spawn_mock_peer`, `spawn_since_peer`) and do not require disk.
    // -----------------------------------------------------------------

    /// W12-G #1: `post_and_classify` returns `Fail` after retry also fails,
    /// and the failure string carries BOTH attempts' reasons (`first:` /
    /// `retry:` prefixes). Hits the `Fail(format!("first: {}; retry: {}"))`
    /// arm at lines ~437-440 directly — the outer broadcast tests only
    /// assert that quorum-not-met surfaces, not the format of the error.
    #[tokio::test]
    async fn post_and_classify_persistent_fail_concatenates_both_reasons() {
        let (url, count) = spawn_mock_peer(MockBehaviour::Fail).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(2000))
            .build()
            .unwrap();
        let body = serde_json::json!({"sender_agent_id":"ai:test","memories":[]});
        let target = format!("{url}/api/v1/sync/push");

        let outcome = post_and_classify(&client, &target, &body, "mem-x", Some("mem-x")).await;
        match outcome {
            AckOutcome::Fail(reason) => {
                assert!(
                    reason.contains("first:") && reason.contains("retry:"),
                    "expected both attempts in reason, got {reason:?}"
                );
                // 5xx → both attempts should have classified as `http 500`.
                assert!(
                    reason.contains("http 500"),
                    "expected 5xx in reason, got {reason:?}"
                );
            }
            other => panic!("expected AckOutcome::Fail, got {other:?}"),
        }
        assert_eq!(
            count.load(Ordering::Relaxed),
            2,
            "first attempt + one retry = exactly two POSTs"
        );
    }

    /// W12-G #2: `post_and_classify` does NOT retry on `IdDrift`. A peer
    /// that semantically disagrees on the id is not a transient failure;
    /// retrying would just observe the same disagreement. Hits the
    /// outer-match `IdDrift => IdDrift` arm at line ~410 (no inner retry
    /// dispatch) — distinct from the `Fail` arm that performs the retry.
    #[tokio::test]
    async fn post_and_classify_id_drift_does_not_retry() {
        // Hand-rolled mock that always 200's with a divergent id.
        let count = Arc::new(AtomicUsize::new(0));
        let cnt_clone = count.clone();
        let app = Router::new().route(
            "/api/v1/sync/push",
            post(move |AxumJson(_b): AxumJson<serde_json::Value>| {
                let c = cnt_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    (
                        StatusCode::OK,
                        AxumJson(serde_json::json!({"ids":["other-id"],"applied":1})),
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let url = format!("http://{addr}/api/v1/sync/push");

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(2000))
            .build()
            .unwrap();
        let body = serde_json::json!({"sender_agent_id":"ai:test","memories":[]});
        let outcome = post_and_classify(&client, &url, &body, "mem-x", Some("mem-x")).await;
        assert!(
            matches!(outcome, AckOutcome::IdDrift),
            "expected IdDrift, got {outcome:?}"
        );
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "IdDrift must NOT trigger the retry path (only one POST)"
        );
    }

    /// W12-G #3: `bulk_catchup_push` with no peers returns immediately
    /// without spawning. Hits the `if memories.is_empty() || config.peers
    /// .is_empty()` shortcut — the existing
    /// `bulk_catchup_push_empty_inputs_are_noop` covers `memories.is_empty()`
    /// only.
    #[tokio::test]
    async fn bulk_catchup_push_no_peers_is_noop() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let cfg = FederationConfig {
            policy: QuorumPolicy::new(1, 1, Duration::from_millis(500), Duration::from_secs(30))
                .unwrap(),
            peers: Vec::new(),
            client,
            sender_agent_id: "ai:no-peers".to_string(),
        };
        // Non-empty memories list — the shortcut should still fire because
        // the peer list is empty.
        let mems = vec![sample_memory()];
        let errors = bulk_catchup_push(&cfg, &mems).await;
        assert!(
            errors.is_empty(),
            "no-peers catchup must return empty error vec immediately, got {errors:?}"
        );
    }

    /// W12-G #4: `bulk_catchup_push` with mixed peer outcomes (one Ack,
    /// one Fail). The Ack peer must NOT appear in the error vec; the
    /// Fail peer MUST appear with its `peer.id` and an http-500 reason.
    /// Validates the per-peer error propagation more precisely than the
    /// existing `bulk_catchup_push_reports_peer_failures` — that test
    /// uses two failing peers.
    #[tokio::test]
    async fn bulk_catchup_push_mixed_outcomes_only_failing_peer_in_errors() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2], 2, 2000);
        let mems = vec![sample_memory()];
        let errors = bulk_catchup_push(&cfg, &mems).await;
        assert_eq!(
            errors.len(),
            1,
            "exactly one failing peer should be in errors, got {errors:?}"
        );
        let (peer_id, reason) = &errors[0];
        // build_config assigns `peer-0:<url>` and `peer-1:<url>`. The
        // failing peer is the second one we registered.
        assert!(
            peer_id.starts_with("peer-1"),
            "failing peer should be peer-1, got {peer_id}"
        );
        assert!(
            reason.contains("http 500"),
            "expected http 500 reason, got {reason}"
        );
        // Both peers were called regardless.
        assert_eq!(count1.load(Ordering::Relaxed), 1);
        assert_eq!(count2.load(Ordering::Relaxed), 1);
    }

    /// W12-G #5: W=1 quorum is met by the local commit alone — no peer
    /// ack needed. Even when every peer fails, the broadcast still
    /// returns Ok and `finalise_quorum` returns `Ok(1)`. Exercises the
    /// `is_quorum_met` early-exit path with `acks.len() == 0`.
    #[tokio::test]
    async fn quorum_w1_local_commit_alone_is_sufficient() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        // W=1, N=3 — local commit is enough on its own.
        let cfg = build_config(vec![url1, url2], 1, 1000);
        let tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        let count = finalise_quorum(&tracker).expect("W=1 must succeed on local commit alone");
        assert_eq!(count, 1, "W=1 quorum returns local-only count");
    }

    /// W12-G #6: `QuorumPolicy::majority` builds the convenience config
    /// with `W = ceil((N+1)/2)`. N=3 → W=2; N=5 → W=3. The existing
    /// suite uses `QuorumPolicy::new` directly everywhere — `majority`
    /// goes uncovered.
    #[test]
    fn quorum_policy_majority_builds_with_ceil_n_plus_1_div_2() {
        let p3 = QuorumPolicy::majority(3).expect("N=3 majority builds");
        // public field for tests: re-derive via finalise round-trip if
        // the internal `w` is private. Instead use a lightweight
        // tracker-based check.
        let mut t = AckTracker::new(p3, Instant::now());
        t.record_local();
        // With W=2, local-only is NOT yet quorum.
        assert!(
            !t.is_quorum_met(Instant::now()),
            "majority-of-3 needs more than local"
        );
        t.record_peer_ack("peer-a");
        assert!(
            t.is_quorum_met(Instant::now()),
            "local + 1 peer ack = 2 = majority of 3"
        );

        let p5 = QuorumPolicy::majority(5).expect("N=5 majority builds");
        let mut t5 = AckTracker::new(p5, Instant::now());
        t5.record_local();
        t5.record_peer_ack("a");
        assert!(
            !t5.is_quorum_met(Instant::now()),
            "majority-of-5 needs 3 acks"
        );
        t5.record_peer_ack("b");
        assert!(t5.is_quorum_met(Instant::now()), "local + 2 peers = 3");
    }

    /// W12-G #7: `QuorumPolicy::majority(0)` rejects with InvalidPolicy.
    /// Hits the `n == 0` guard via the convenience constructor (the
    /// existing `quorum_not_met_payload_invalid_policy_branch` builds
    /// the error directly without going through `QuorumPolicy::new`).
    #[test]
    fn quorum_policy_majority_rejects_zero() {
        let err = QuorumPolicy::majority(0).expect_err("n=0 must be rejected");
        match err {
            QuorumError::InvalidPolicy { detail } => {
                assert!(
                    detail.contains("n must be"),
                    "expected n>=1 message, got {detail}"
                );
            }
            other => panic!("expected InvalidPolicy, got {other:?}"),
        }
    }

    /// W12-G #8: `FederationConfig::build` rejects duplicate peers
    /// where the URLs differ only in trailing-slash. Existing test
    /// (`config_build_rejects_duplicate_peer_urls`) uses identical
    /// strings; this exercises the normalization branch
    /// (`trim_end_matches('/').to_ascii_lowercase()`).
    #[test]
    fn config_build_rejects_duplicate_peers_differing_only_in_trailing_slash() {
        let result = FederationConfig::build(
            2,
            &[
                "http://peer.example".to_string(),
                "http://peer.example/".to_string(),
            ],
            Duration::from_millis(500),
            None,
            None,
            None,
            "ai:dup-test".to_string(),
        );
        let err = match result {
            Ok(_) => panic!("trailing-slash dup must be rejected"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate peer URL"),
            "expected duplicate-peer error, got {msg}"
        );
    }

    /// W12-G #9: `FederationConfig::build` rejects duplicate peers where
    /// the URLs differ only in scheme/host casing. Mirrors the
    /// `to_ascii_lowercase` half of the normalization.
    #[test]
    fn config_build_rejects_duplicate_peers_differing_only_in_case() {
        let result = FederationConfig::build(
            2,
            &[
                "http://Peer.Example".to_string(),
                "http://peer.example".to_string(),
            ],
            Duration::from_millis(500),
            None,
            None,
            None,
            "ai:dup-case-test".to_string(),
        );
        let err = match result {
            Ok(_) => panic!("case-only dup must be rejected"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate peer URL"),
            "expected duplicate-peer error, got {msg}"
        );
    }

    /// W12-G #10: archive_quorum classifies a hanging peer as
    /// non-acking — the existing tests for archive_quorum use Ack and
    /// Fail only. With Hang behaviour and a tight 200ms timeout, the
    /// `Ok(None) | Err(_) => break` arm fires in the inner timeout
    /// match. (Ditto for restore/link/consolidate — covered together
    /// via a sweep below to keep this test focused.)
    #[tokio::test]
    async fn archive_quorum_hanging_peer_times_out_to_break_arm() {
        let (url1, _) = spawn_mock_peer(MockBehaviour::Hang).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Hang).await;
        // W=2 with two hanging peers + 200ms timeout. The local commit
        // is the only source of acks; quorum cannot be met.
        let cfg = build_config(vec![url1, url2], 2, 200);
        let start = Instant::now();
        let tracker = broadcast_archive_quorum(&cfg, "mem-arch-id").await.unwrap();
        let elapsed = start.elapsed();
        // Loop must give up at the deadline, not hang for the full 10s
        // peer sleep.
        assert!(
            elapsed < Duration::from_secs(2),
            "archive_quorum must exit at deadline, took {elapsed:?}"
        );
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(
            matches!(err, QuorumError::QuorumNotMet { .. }),
            "expected QuorumNotMet, got {err:?}"
        );
    }

    /// W12-G #11: `QuorumNotMetPayload::from_err` round-trip on a real
    /// `Unreachable` outcome from the broadcast loop. Existing direct
    /// tests build the QuorumError by hand; this end-to-end path has
    /// the broadcast actually classify the failure reason.
    #[tokio::test]
    async fn quorum_not_met_payload_unreachable_round_trip_from_broadcast() {
        // Two peers both Fail (not Hang) — we want the deadline to
        // elapse with zero peer acks. The broadcast finalises with
        // `Unreachable` because acks.is_empty() AND past deadline.
        let (url1, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        let (url2, _) = spawn_mock_peer(MockBehaviour::Fail).await;
        // Tight timeout so the deadline beats the 250ms backoff retry.
        let cfg = build_config(vec![url1, url2], 2, 100);
        let tracker = broadcast_store_quorum(&cfg, &sample_memory())
            .await
            .unwrap();
        // Wait past the deadline before finalising — this guarantees
        // `now > deadline` in finalise() so the Unreachable branch is
        // selected (rather than InFlight).
        tokio::time::sleep(Duration::from_millis(150)).await;
        let err = finalise_quorum(&tracker).unwrap_err();
        let payload = QuorumNotMetPayload::from_err(&err);
        assert_eq!(payload.error, "quorum_not_met");
        assert_eq!(payload.got, 1, "only local commit");
        assert_eq!(payload.needed, 2);
        assert!(
            payload.reason == "unreachable" || payload.reason == "timeout",
            "expected unreachable/timeout, got {}",
            payload.reason
        );
    }

    /// W12-G #12: `catchup_once` against a peer with an unusual base URL
    /// (no `/api/v1/sync/push` suffix) — `trim_end_matches` no-ops, so
    /// the constructed `since` URL appends `/api/v1/sync/since` to the
    /// raw base. Exercises the trim-noop branch at the start of
    /// catchup_once.
    #[tokio::test]
    async fn catchup_once_peer_url_without_push_suffix_still_builds_since() {
        let (url, hits, _, last_peer) =
            spawn_since_peer(SinceMockBehaviour::ReturnMemories(vec![])).await;
        // Build a config whose peer.sync_push_url does NOT end in
        // `/api/v1/sync/push`. The trim_end_matches in catchup_once is
        // a no-op for this shape, so the base URL is the raw `url`.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(2000))
            .build()
            .unwrap();
        let cfg = FederationConfig {
            policy: QuorumPolicy::new(2, 1, Duration::from_millis(2000), Duration::from_secs(30))
                .unwrap(),
            peers: vec![PeerEndpoint {
                id: "peer-0".to_string(),
                // No /api/v1/sync/push suffix — verifies the trim is
                // tolerant of unexpected shapes.
                sync_push_url: url.clone(),
            }],
            client,
            sender_agent_id: "ai:no-suffix".to_string(),
        };
        let db = build_test_db();
        catchup_once(&cfg, &db).await;
        // The mock saw a hit at /api/v1/sync/since with the local agent id.
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        assert_eq!(
            last_peer.lock().await.as_deref(),
            Some("ai:no-suffix"),
            "local agent id should be forwarded as ?peer="
        );
    }

    /// W12-G #13: `catchup_once` skips memories that fail
    /// `validate_memory` (e.g. invalid `source` enum). The valid memory
    /// IS applied; sync_state advances to the latest TS seen. Exercises
    /// the `if crate::validate::validate_memory(&mem).is_err() { continue; }`
    /// branch which the F9 happy-path tests don't trigger.
    #[tokio::test]
    async fn catchup_once_skips_invalid_memory_but_applies_valid_neighbour() {
        // valid memory uses source="system" (whitelisted by validate_memory).
        let valid = catchup_memory("ok-mem", "2026-04-26T10:00:00Z");
        // invalid memory has source not in the allowlist (validate fails).
        let mut bad = catchup_memory("bad-source", "2026-04-26T10:00:01Z");
        bad.source = "made-up-source-not-in-allowlist".to_string();
        let mems = vec![valid.clone(), bad];

        let (url, hits, _, _) = spawn_since_peer(SinceMockBehaviour::ReturnMemories(mems)).await;
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();
        catchup_once(&cfg, &db).await;

        assert_eq!(hits.load(Ordering::Relaxed), 1);
        let lock = db.lock().await;
        // Only the valid memory was inserted.
        let count: i64 = lock
            .0
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "only the valid memory should land");
        let title: String = lock
            .0
            .query_row(
                "SELECT title FROM memories WHERE namespace='catchup' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "ok-mem");
        // sync_state advanced to the latest TS of the APPLIED rows
        // only — the validate-fail `continue` happens before the
        // `latest_ts` bump, so the invalid 10:00:01 row does NOT
        // contribute. Net: latest_ts == valid memory's timestamp.
        let clock = crate::db::sync_state_load(&lock.0, "ai:catchup-test").unwrap();
        assert_eq!(
            clock.entries.get("peer-0").map(String::as_str),
            Some("2026-04-26T10:00:00Z"),
            "sync_state tracks latest_ts of validate-passing rows"
        );
    }

    /// W12-G #14: `AckTracker::record_peer_ack` is idempotent — recording
    /// the same peer id twice does not double-count. Exercised
    /// indirectly by the broadcast layer (the tracker is a HashSet under
    /// the hood) but never asserted directly.
    #[test]
    fn ack_tracker_record_peer_ack_is_idempotent() {
        let policy = QuorumPolicy::new(3, 2, Duration::from_secs(1), Duration::from_secs(30))
            .expect("policy");
        let mut t = AckTracker::new(policy, Instant::now());
        t.record_local();
        t.record_peer_ack("peer-a");
        t.record_peer_ack("peer-a"); // dup — must dedupe
        // 2 acks (local + 1 distinct peer) = 2 = W → quorum met.
        assert!(t.is_quorum_met(Instant::now()));
        // Adding a third distinct peer does not regress quorum.
        t.record_peer_ack("peer-b");
        assert!(t.is_quorum_met(Instant::now()));
    }

    /// W12-G #15a: `catchup_once` against a peer whose 200 body lacks
    /// a `memories` key — `body.get("memories")` returns None and the
    /// loop `continue`s without applying anything or advancing
    /// sync_state. Hits the `None => continue` arm at line ~1478
    /// (the existing F9 tests always include the `memories` array).
    #[tokio::test]
    async fn catchup_once_body_without_memories_key_is_skipped() {
        // Hand-rolled handler returning `{"applied": 0}` (no memories key).
        let app = Router::new().route(
            "/api/v1/sync/since",
            axum::routing::get(|| async {
                (
                    StatusCode::OK,
                    AxumJson(serde_json::json!({"applied":0,"note":"empty cluster"})),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let url = format!("http://{addr}");
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();
        catchup_once(&cfg, &db).await;
        let lock = db.lock().await;
        let count: i64 = lock
            .0
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "no memories key → no inserts");
        let clock = crate::db::sync_state_load(&lock.0, "ai:catchup-test").unwrap();
        assert!(
            clock.entries.get("peer-0").is_none(),
            "no memories key → sync_state untouched"
        );
    }

    /// W12-G #15b: `catchup_once` against a peer that returns a 200 with
    /// a `memories` array containing an unparseable element. The
    /// individual element is skipped (`serde_json::from_value` Err) and
    /// the rest of the batch is applied. Hits lines 1492-1494.
    #[tokio::test]
    async fn catchup_once_unparseable_individual_memory_is_skipped() {
        // `memories[0]` is a valid Memory, `memories[1]` is a JSON object
        // with the wrong shape (missing required fields).
        let valid_mem = serde_json::to_value(catchup_memory("ok", "2026-04-26T10:00:00Z")).unwrap();
        let bad_mem = serde_json::json!({"id":"oops","not_a_memory_field": true});
        let app = Router::new().route(
            "/api/v1/sync/since",
            axum::routing::get(move || {
                let valid = valid_mem.clone();
                let bad = bad_mem.clone();
                async move {
                    (
                        StatusCode::OK,
                        AxumJson(serde_json::json!({"memories": [valid, bad]})),
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let url = format!("http://{addr}");
        let cfg = build_catchup_cfg(&url, 2000);
        let db = build_test_db();
        catchup_once(&cfg, &db).await;
        let lock = db.lock().await;
        // Only the parseable memory landed.
        let count: i64 = lock
            .0
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "only parseable memory inserted");
    }

    /// W12-G #16: id-drift on `broadcast_delete_quorum` exercises the
    /// `IdDrift => record_id_drift` arm at line ~591 (the existing
    /// `id_drift_peer_does_not_count_as_ack` only hits the store path).
    #[tokio::test]
    async fn delete_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let url2 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1, url2], 2, 1000);
        let tracker = broadcast_delete_quorum(&cfg, "mem-del-x").await.unwrap();
        // local + 0 peer acks = 1 < W=2 → not met.
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(
            matches!(err, QuorumError::QuorumNotMet { got: 1, .. }),
            "expected QuorumNotMet got=1, got {err:?}"
        );
        // Both peers reported drift.
        assert_eq!(
            tracker.id_drift_count(),
            2,
            "both peers should be recorded as drift"
        );
    }

    /// W12-G #17: id-drift on `broadcast_archive_quorum` exercises the
    /// IdDrift arm at line ~679.
    #[tokio::test]
    async fn archive_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let tracker = broadcast_archive_quorum(&cfg, "mem-arch-x").await.unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #18: id-drift on `broadcast_restore_quorum` exercises the
    /// IdDrift arm at line ~768.
    #[tokio::test]
    async fn restore_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let tracker = broadcast_restore_quorum(&cfg, "mem-res-x").await.unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #19: id-drift on `broadcast_link_quorum` exercises the
    /// IdDrift arm at line ~851.
    #[tokio::test]
    async fn link_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let tracker = broadcast_link_quorum(&cfg, &sample_link()).await.unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #20: id-drift on `broadcast_consolidate_quorum` exercises
    /// the IdDrift arm at line ~935.
    #[tokio::test]
    async fn consolidate_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let new_mem = sample_memory();
        let tracker = broadcast_consolidate_quorum(&cfg, &new_mem, &[])
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #21: id-drift on `broadcast_pending_quorum` exercises the
    /// IdDrift arm at line ~1024.
    #[tokio::test]
    async fn pending_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let tracker = broadcast_pending_quorum(&cfg, &sample_pending())
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #22: id-drift on `broadcast_pending_decision_quorum`
    /// exercises the IdDrift arm at line ~1112.
    #[tokio::test]
    async fn pending_decision_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let tracker = broadcast_pending_decision_quorum(&cfg, &sample_decision())
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #23: id-drift on `broadcast_namespace_meta_quorum`
    /// exercises the IdDrift arm at line ~1201.
    #[tokio::test]
    async fn namespace_meta_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let tracker = broadcast_namespace_meta_quorum(&cfg, &sample_namespace_meta())
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #24: id-drift on `broadcast_namespace_meta_clear_quorum`
    /// exercises the IdDrift arm at line ~1294.
    #[tokio::test]
    async fn namespace_meta_clear_quorum_id_drift_peer_records_drift_not_ack() {
        let url1 = spawn_id_drift_peer().await;
        let cfg = build_config(vec![url1], 2, 1000);
        let namespaces = vec!["app/team".to_string()];
        let tracker = broadcast_namespace_meta_clear_quorum(&cfg, &namespaces)
            .await
            .unwrap();
        let err = finalise_quorum(&tracker).unwrap_err();
        assert!(matches!(err, QuorumError::QuorumNotMet { .. }));
        assert_eq!(tracker.id_drift_count(), 1);
    }

    /// W12-G #25: post-quorum detach for `broadcast_delete_quorum`
    /// fanout exercises the post-quorum spawn block at lines 608-616
    /// (the `if !joins.is_empty()` arm). With W=2 N=3 and one peer
    /// hanging, quorum is met by the two ack peers and the detached
    /// task drains the still-running join.
    #[tokio::test]
    async fn delete_quorum_post_quorum_detach_drains_remaining_peer() {
        let (url1, count1) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url2, count2) = spawn_mock_peer(MockBehaviour::Ack).await;
        let (url3, count3) = spawn_mock_peer(MockBehaviour::Fail).await;
        let cfg = build_config(vec![url1, url2, url3], 2, 2000);
        let _tracker = broadcast_delete_quorum(&cfg, "mem-detach").await.unwrap();
        // Wait long enough for the detached failing peer to finish its
        // first attempt + 250ms backoff + retry.
        for _ in 0..100 {
            if count1.load(Ordering::Relaxed) >= 1
                && count2.load(Ordering::Relaxed) >= 1
                && count3.load(Ordering::Relaxed) >= 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Failing peer must have been called by the detach (it
        // wouldn't have been if the detach was aborted on quorum-met).
        assert!(
            count3.load(Ordering::Relaxed) >= 1,
            "failing peer must be reached by the detached fanout"
        );
    }

    /// W12-G #15: `AckTracker::finalise` returns `InFlight` when called
    /// pre-deadline with insufficient acks. Distinct from Timeout
    /// (post-deadline w/ partial) and Unreachable (post-deadline w/ none).
    /// Validates the third reason variant directly.
    #[test]
    fn ack_tracker_finalise_pre_deadline_returns_in_flight() {
        // Long timeout so we are pre-deadline at finalise().
        let policy = QuorumPolicy::new(3, 2, Duration::from_secs(60), Duration::from_secs(30))
            .expect("policy");
        let now = Instant::now();
        let mut t = AckTracker::new(policy, now);
        t.record_local();
        // No peer acks yet — finalise pre-deadline should be InFlight.
        let err = t.finalise(now).unwrap_err();
        match err {
            QuorumError::QuorumNotMet {
                got,
                needed,
                reason,
            } => {
                assert_eq!(got, 1);
                assert_eq!(needed, 2);
                assert_eq!(
                    reason,
                    QuorumFailureReason::InFlight,
                    "pre-deadline insufficient-ack must classify as InFlight"
                );
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }
}
