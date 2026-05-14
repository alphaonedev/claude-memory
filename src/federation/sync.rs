// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Quorum-broadcast fan-out logic: post_once, post_and_classify,
//! broadcast_*_quorum, bulk_catchup_push.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::models::{Memory, MemoryLink, NamespaceMetaEntry, PendingAction, PendingDecision};
use crate::replication::{AckTracker, QuorumError};

use super::FederationConfig;

#[derive(Debug)]
pub(super) enum AckOutcome {
    Ack,
    IdDrift,
    Fail(String),
}

/// Single-attempt POST to a peer, classifying the response into an
/// `AckOutcome`. No retries — callers that want retry-on-transient-fail
/// should use [`post_and_classify`].
///
/// `api_key` (v0.7.0 fold-A2A1.4, #702) is the operator-configured
/// `[api] api_key` from the local daemon's `AppConfig`. When `Some`,
/// an `x-api-key: <value>` header is attached so peers that themselves
/// run with api-key auth accept the outbound POST. When `None`, no
/// header is attached — backwards-compatible with mTLS-only and
/// no-auth deployments.
pub(super) async fn post_once(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    expected_id: &str,
    idempotency_key: Option<&str>,
    api_key: Option<&str>,
) -> AckOutcome {
    // Ultrareview #346: attach an idempotency key so peers can dedupe
    // on retry. If a tokio::timeout fires locally but the HTTP POST
    // already reached the peer, the peer applies the write once; a
    // subsequent catchup sync carrying the same memory.id will be a
    // no-op via `insert_if_newer`. The key is set from the outgoing
    // memory id by default, which is stable across retries.
    // v0.7.0 (issue #691 fold-1) — wire the NetworkRequest governance
    // gate BEFORE the outbound HTTPS POST. A refuse rule
    // (`{"host":"evil.example.com"}` etc.) short-circuits the fan-out
    // for that peer with a typed `AckOutcome::Fail` carrying the
    // refusal reason. The quorum combiner already treats `Fail` as
    // "this peer did not ack", so a refusal counts as a peer-miss
    // without crashing the broadcast (allowing the remaining peers to
    // reach quorum). The audit chain records the refusal via the
    // governance.check signed_events row emitted on the daemon side.
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string());
    let scheme = reqwest::Url::parse(url)
        .ok()
        .map(|u| u.scheme().to_string())
        .unwrap_or_default();
    let net_action = crate::governance::agent_action::AgentAction::NetworkRequest {
        host: host.clone(),
        scheme,
    };
    if let Err(refusal) = crate::governance::wire_check::check(&net_action) {
        return AckOutcome::Fail(format!(
            "governance refused outbound to {host}: {}",
            refusal.reason
        ));
    }
    let mut req = client.post(url).json(body);
    if let Some(key) = idempotency_key {
        req = req.header("Idempotency-Key", key);
    }
    // v0.7.0 fold-A2A1.4 (#702) — forward the operator-configured
    // `[api] api_key` on every outbound federation POST. Peers that
    // themselves run with api-key auth otherwise reject with 401 and
    // cross-host quorum can never converge. Backwards-compatible:
    // `None` means no header attached.
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
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
pub(super) const FANOUT_RETRY_BACKOFF: Duration = Duration::from_millis(250);

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
pub(super) async fn post_and_classify(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    expected_id: &str,
    idempotency_key: Option<&str>,
    api_key: Option<&str>,
) -> AckOutcome {
    match post_once(client, url, body, expected_id, idempotency_key, api_key).await {
        AckOutcome::Ack => AckOutcome::Ack,
        AckOutcome::IdDrift => AckOutcome::IdDrift,
        AckOutcome::Fail(first_reason) => {
            tokio::time::sleep(FANOUT_RETRY_BACKOFF).await;
            match post_once(client, url, body, expected_id, idempotency_key, api_key).await {
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &mem_id,
                Some(&mem_id),
                api_key.as_deref(),
            )
            .await;
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
    // H9 (v0.7.0 round-2) — partial-quorum WARN. When the leader returns
    // success (quorum met) but some configured peers never ack-ed inside
    // the deadline, operators need to see the gap in logs before a
    // follow-up sync cycle catches the lagging peer up. This is the
    // canonical observation point: the tracker is finalised, the peer
    // set is known, and the configured-vs-acked subtraction surfaces
    // exactly which urls fell behind.
    if tracker.finalise(Instant::now()).is_ok() {
        let acked = tracker.acked_peer_ids();
        let mut missing: Vec<String> = config
            .peers
            .iter()
            .filter(|p| !acked.contains(&p.id))
            .map(|p| p.sync_push_url.clone())
            .collect();
        if !missing.is_empty() {
            missing.sort();
            tracing::warn!(
                memory_id = %mem.id,
                n_missing = missing.len(),
                peer_urls = ?missing,
                "federation: quorum met but {} peer(s) did not ack: {:?}",
                missing.len(),
                missing,
            );
            crate::metrics::registry()
                .federation_partial_quorum_total
                .inc();
        }
    }
    Ok(tracker)
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target_id,
                Some(&target_id),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target_id,
                Some(&target_id),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target_id,
                Some(&target_id),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &log_id,
                Some(&log_id),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target_id,
                Some(&target_id),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target_id,
                Some(&target_id),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target_id,
                Some(&target_id),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target,
                Some(&target),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let outcome = post_and_classify(
                &client,
                &url,
                &payload,
                &target,
                Some(&target),
                api_key.as_deref(),
            )
            .await;
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
        let api_key = config.api_key.clone();
        joins.spawn(async move {
            let mut req = client.post(&url).json(&payload);
            // No Idempotency-Key on the batch — the batch is itself an
            // idempotent replay, and the peer's `insert_if_newer`
            // dedupes per row by (id, updated_at).
            req = req.header("X-Catchup", "bulk");
            // v0.7.0 fold-A2A1.4 (#702) — forward the operator-configured
            // `x-api-key` on the catchup batch as well. Without this, a
            // catchup against a peer that runs with api-key auth fails
            // 401 and the row gap stays open.
            if let Some(key) = api_key.as_deref() {
                req = req.header("x-api-key", key);
            }
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
