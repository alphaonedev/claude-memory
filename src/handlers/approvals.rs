// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K10 — Approval API (HTTP + SSE).
//!
//! `POST /api/v1/approvals/{pending_id}` — approve / deny a pending row.
//! Body: `{"decision":"approve|deny","remember":"once|session|forever"}`.
//! Gated behind the K7 server-wide HMAC: caller MUST present
//! `X-AI-Memory-Signature: sha256=<hex>` keyed on
//! `SHA256([hooks.subscription].hmac_secret)` over the canonical
//! `<timestamp>.<body>` string. Missing or invalid signature → 401.
//!
//! `GET /api/v1/approvals/stream` — long-lived SSE stream that fans out
//! every `approval_requested` and `approval_decided` event from the
//! process-wide [`crate::approvals`] broadcast bus to every attached
//! subscriber.
//!
//! The SSE endpoint is intentionally unauthenticated beyond the
//! existing `api_key_auth` middleware: SSE re-key handshakes are clunky
//! and the K7 HMAC is a *write*-side gate. Read-side gating piggybacks
//! on the api-key middleware that wraps every other route.
//!
//! Extracted from `src/handlers/mod.rs` as part of the issue #650
//! file-architecture cleanup.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use super::transport::{AppState, constant_time_eq};
use crate::db;
use crate::validate;

/// Body of `POST /api/v1/approvals/{pending_id}`.
#[derive(Debug, Deserialize)]
pub struct ApprovalRequestBody {
    /// `"approve"` or `"deny"`.
    pub decision: crate::approvals::Decision,
    /// `"once"` (default), `"session"`, or `"forever"`.
    #[serde(default = "default_remember")]
    pub remember: crate::approvals::Remember,
}

fn default_remember() -> crate::approvals::Remember {
    crate::approvals::Remember::Once
}

/// Maximum age (in seconds) the `X-AI-Memory-Timestamp` header may
/// claim before we treat the request as a replay. v0.7.0 K10 review
/// #628 (blocker C1): without an upper bound on timestamp age, any
/// captured signed request can be re-issued indefinitely.
///
/// 300s mirrors the AWS SigV4 / Stripe webhook windows — long enough
/// to absorb client-side retry jitter, short enough that an exfiltrated
/// signature expires before an attacker can weaponise it.
pub(crate) const APPROVAL_HMAC_MAX_AGE_SECS: i64 = 300;

/// Maximum future-skew (in seconds) the `X-AI-Memory-Timestamp` header
/// may claim ahead of the server clock. NTP drift is real and we don't
/// want to 401 a legitimate signer whose clock is 30s fast; 60s is the
/// industry-standard tolerance.
pub(crate) const APPROVAL_HMAC_MAX_SKEW_SECS: i64 = 60;

/// HMAC-verify an inbound approval request.
///
/// Mirrors the K7 outbound construction: signature value is
/// `sha256=<hex>` where `<hex>` = `HMAC-SHA256(SHA256(secret),
/// "<timestamp>.<body>")`. Returns `Ok(())` on a valid signature;
/// `Err(StatusCode)` (always 401) on any failure mode (missing
/// header, missing timestamp, stale timestamp, bad encoding,
/// mismatch).
///
/// **Replay-window enforcement (review #628 blocker C1)**: the
/// `X-AI-Memory-Timestamp` header is parsed as a Unix epoch in
/// seconds and rejected if it is older than
/// [`APPROVAL_HMAC_MAX_AGE_SECS`] OR newer than
/// [`APPROVAL_HMAC_MAX_SKEW_SECS`]. A captured-and-replayed signed
/// request becomes unusable after the 5-minute window expires.
///
/// The caller MUST send the body verbatim — even a single
/// reformatted byte invalidates the signature, which is the whole
/// point of HMAC. We compare in constant time via `constant_time_eq`
/// to avoid timing oracles on the hex digest.
///
/// **Canonical request (#628 P1, agent-4 finding)**: the signed
/// payload binds **method + URL path + body**, not just `<ts>.<body>`.
/// Without the path binding, a captured signature for pending row A
/// could be replayed against pending row B by simply changing the URL
/// — a row-substitution attack inside the 300s replay window. The
/// canonical is now:
///
/// ```text
/// canonical = "<unix_ts>.<METHOD>.<pending_id>.<body>"
/// ```
///
/// Both signer and verifier MUST use the exact same join. Callers
/// that previously signed `<ts>.<body>` will now hard-fail (401), so
/// any in-tree test fixture or external client must be updated in
/// lockstep with this change.
pub(crate) fn verify_approval_hmac(
    headers: &HeaderMap,
    body: &[u8],
    method: &str,
    pending_id: &str,
) -> Result<(), StatusCode> {
    let secret = match crate::config::active_hooks_hmac_secret() {
        Some(s) => s,
        None => {
            // No server-wide HMAC configured → the K10 contract is
            // strict by default: reject every inbound approval. This
            // is the safe posture (better to refuse a write than to
            // accept an unauthenticated one) and matches the spec
            // header "HMAC signing per K7's pattern".
            tracing::warn!("K10 approval rejected: no [hooks.subscription].hmac_secret configured");
            return Err(StatusCode::UNAUTHORIZED);
        }
    };
    let sig_header = headers
        .get("x-ai-memory-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let sig_hex = sig_header
        .strip_prefix("sha256=")
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let timestamp = headers
        .get("x-ai-memory-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    // Replay-window check: the timestamp MUST parse as a Unix epoch
    // (seconds) and fall inside the [now-300s, now+60s] window. Any
    // failure here is a hard 401 — we log a diagnostic so operators
    // can tell a stale-replay attempt apart from a torn signature.
    let ts_secs: i64 = timestamp.parse().map_err(|_| {
        tracing::warn!(
            "K10 approval rejected: X-AI-Memory-Timestamp not a Unix epoch integer: {timestamp:?}"
        );
        StatusCode::UNAUTHORIZED
    })?;
    let now_secs = Utc::now().timestamp();
    let delta = now_secs - ts_secs;
    if delta > APPROVAL_HMAC_MAX_AGE_SECS {
        tracing::warn!(
            "K10 approval rejected: stale signature (age {delta}s > {APPROVAL_HMAC_MAX_AGE_SECS}s window)"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    if delta < -APPROVAL_HMAC_MAX_SKEW_SECS {
        tracing::warn!(
            "K10 approval rejected: future-dated signature (skew {}s > {APPROVAL_HMAC_MAX_SKEW_SECS}s tolerance)",
            -delta
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    let body_str = std::str::from_utf8(body).map_err(|_| StatusCode::UNAUTHORIZED)?;
    // P1 (#628 agent-4): bind method + pending_id so a captured
    // signature can't be redirected to a different approval row.
    let canonical = format!("{timestamp}.{method}.{pending_id}.{body_str}");
    let key_hash = crate::subscriptions::sha256_hex(&secret);
    let expected = crate::subscriptions::hmac_sha256_hex(&key_hash, &canonical);
    if !constant_time_eq(expected.as_bytes(), sig_hex.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P2 (#628 agent-4): nonce-cache enforces single-use within the
    // 300s window. Without this, a captured signature could be
    // replayed N times against the same row before the timestamp
    // staled out — rendering the row-already-decided check the only
    // line of defence. The cache keys on the signature hex itself
    // (which already commits to ts + method + path + body + secret),
    // so any change to any field produces a new key.
    if !record_hmac_nonce(sig_hex, ts_secs) {
        tracing::warn!(
            "K10 approval rejected: signature replay (sig={}…)",
            &sig_hex[..16.min(sig_hex.len())]
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

/// Process-wide replay cache for verified K10 HMAC signatures. Entries
/// expire after `APPROVAL_HMAC_MAX_AGE_SECS * 2` (twice the legitimate
/// window — safe upper bound including future-skew tolerance).
fn record_hmac_nonce(sig_hex: &str, ts_secs: i64) -> bool {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, i64>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    let now = Utc::now().timestamp();
    let ttl = APPROVAL_HMAC_MAX_AGE_SECS.saturating_mul(2);
    // Opportunistic eviction. The cache is bounded by traffic × ttl,
    // typically < 10K entries even on a busy daemon — cheap to scan.
    guard.retain(|_, t| now.saturating_sub(*t) < ttl);
    if guard.contains_key(sig_hex) {
        return false;
    }
    guard.insert(sig_hex.to_string(), ts_secs);
    true
}

/// `POST /api/v1/approvals/{pending_id}` — K10's HMAC-gated approval
/// endpoint. See module-level comment above for the full contract.
#[allow(clippy::too_many_lines)]
pub async fn approval_decide(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body_bytes: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(status) = verify_approval_hmac(&headers, &body_bytes, "POST", &id) {
        return (
            status,
            Json(json!({"error": "invalid or missing X-AI-Memory-Signature"})),
        )
            .into_response();
    }
    let body: ApprovalRequestBody = match serde_json::from_slice(&body_bytes) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid body: {e}")})),
            )
                .into_response();
        }
    };
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid agent_id: {e}")})),
            )
                .into_response();
        }
    };

    // #913 (security-medium / SOC2, 2026-05-19) — admin governance audit.
    // K10's HMAC-gated approval endpoint is the primary privileged
    // decision surface; emit the forensic-chain entry BEFORE the storage
    // write so the audit trail records the decider's identity, the
    // outcome (approve / deny), and the pending id regardless of
    // downstream consensus / execution behaviour.
    let decision_kind = match body.decision {
        crate::approvals::Decision::Approve => "approval_decide_approve",
        crate::approvals::Decision::Deny => "approval_decide_deny",
    };
    let decision_outcome = match body.decision {
        crate::approvals::Decision::Approve => "allow",
        crate::approvals::Decision::Deny => "refuse",
    };
    crate::governance::audit::record_decision(
        &agent_id,
        decision_outcome,
        decision_kind,
        "",
        json!({ "pending_id": &id }),
    );

    let lock = app.db.lock().await;
    // Snapshot the pending row before deciding so we can synthesise a
    // permission rule even after the row transitions.
    let pending_snapshot = db::get_pending_action(&lock.0, &id).ok().flatten();
    let outcome = match body.decision {
        crate::approvals::Decision::Approve => {
            match db::approve_with_approver_type(&lock.0, &id, &agent_id) {
                Ok(crate::db::ApproveOutcome::Approved) => {
                    let executed = db::execute_pending_action(&lock.0, &id);
                    match executed {
                        Ok(memory_id) => json!({
                            "approved": true,
                            "id": id,
                            "decided_by": agent_id,
                            "executed": true,
                            "memory_id": memory_id,
                            "remember": format!("{:?}", body.remember).to_lowercase(),
                        }),
                        Err(e) => {
                            tracing::error!("execute pending error: {e}");
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": "approved but execution failed"})),
                            )
                                .into_response();
                        }
                    }
                }
                Ok(crate::db::ApproveOutcome::Pending { votes, quorum }) => json!({
                    "approved": false,
                    "status": "pending",
                    "id": id,
                    "votes": votes,
                    "quorum": quorum,
                    "remember": format!("{:?}", body.remember).to_lowercase(),
                }),
                Ok(crate::db::ApproveOutcome::Rejected(reason)) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": format!("approve rejected: {reason}")})),
                    )
                        .into_response();
                }
                Err(e) => {
                    tracing::error!("handler error: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response();
                }
            }
        }
        crate::approvals::Decision::Deny => {
            match db::decide_pending_action(&lock.0, &id, false, &agent_id) {
                Ok(true) => json!({
                    "rejected": true,
                    "id": id,
                    "decided_by": agent_id,
                    "remember": format!("{:?}", body.remember).to_lowercase(),
                }),
                Ok(false) => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": "pending action not found or already decided"})),
                    )
                        .into_response();
                }
                Err(e) => {
                    tracing::error!("handler error: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response();
                }
            }
        }
    };
    drop(lock);

    // Fan out the decision on the broadcast bus and (for forever)
    // record the synthetic rule.
    let decision_label = match body.decision {
        crate::approvals::Decision::Approve => "approve",
        crate::approvals::Decision::Deny => "deny",
    };
    let remember_label = match body.remember {
        crate::approvals::Remember::Once => "once",
        crate::approvals::Remember::Session => "session",
        crate::approvals::Remember::Forever => "forever",
    };
    // Capture the namespace + original requester from the snapshot so
    // the published `ApprovalDecided` event carries enough metadata for
    // the SSE handler's tenant filter (review #628 blocker C2).
    //
    // #869 audit (Category B — safe default): `pending_snapshot` is
    // `None` when the row was decided before we could snapshot it
    // (rare race window). Empty `String` for namespace / requested_by
    // is a documented sentinel the SSE tenant filter treats as
    // "no-tenant" — degrades event visibility, not correctness.
    let evt_namespace = pending_snapshot
        .as_ref()
        .map(|p| p.namespace.clone())
        .unwrap_or_default();
    let evt_requested_by = pending_snapshot
        .as_ref()
        .map(|p| p.requested_by.clone())
        .unwrap_or_default();
    crate::approvals::publish(crate::approvals::ApprovalEvent::ApprovalDecided {
        pending_id: id.clone(),
        decision: decision_label.to_string(),
        decided_by: agent_id.clone(),
        remember: remember_label.to_string(),
        namespace: evt_namespace,
        requested_by: evt_requested_by,
    });
    if matches!(
        body.remember,
        crate::approvals::Remember::Forever | crate::approvals::Remember::Session
    ) && let Some(snap) = pending_snapshot
    {
        crate::approvals::record_synthetic_rule(crate::approvals::SyntheticPermissionRule {
            action_type: snap.action_type,
            namespace: snap.namespace,
            agent_id: Some(snap.requested_by),
            decision: decision_label.to_string(),
            recorded_at: Utc::now().to_rfc3339(),
        });
    }
    Json(outcome).into_response()
}

/// Predicate: should the SSE subscriber identified by
/// `subscriber_agent` receive the given approval event?
///
/// Review #628 blocker C2: the K10 broadcast channel is
/// process-wide, so without a receive-side filter every authenticated
/// subscriber sees every other tenant's pending rows — a critical
/// cross-tenant leak.
///
/// Visibility rules:
///   1. The subscriber sees events that originated from their own
///      `agent_id` (the original requester for an `ApprovalRequested`,
///      or the requester whose pending row was decided for an
///      `ApprovalDecided`).
///   2. The subscriber sees events whose `namespace` is reachable by
///      a K9 [`PermissionRule`] entry whose `agent_pattern` matches
///      the subscriber. This lets a designated approver agent watch
///      the rows it is actually allowed to act on, without needing
///      to share an `agent_id` with the requester.
///   3. The historical "anonymous" subscriber (agent_id empty) sees
///      nothing — opt-in is the safe default for a privileged feed.
///   4. If the subscriber agent is a server-internal id starting with
///      `host:` (the daemon's own boot id), they see everything —
///      this preserves the operator-CLI affordance of attaching to
///      the local socket and observing all activity for diagnostics.
#[must_use]
pub fn sse_event_visible_to(
    subscriber_agent: &str,
    event: &crate::approvals::ApprovalEvent,
) -> bool {
    if subscriber_agent.is_empty() {
        return false;
    }
    // Security (#628 P1, agent-4 finding): the prior `host:` prefix
    // bypass let any client passing `X-Agent-Id: host:anything` see
    // every tenant's events. `host:` is meant to be the *server-side*
    // fallback identifier from `identity::resolve_agent_id` — it is
    // never a legitimate self-asserted subscriber agent_id. The
    // `approvals_sse` handler now rejects `host:`-prefixed values at
    // the handshake; this defence-in-depth check ensures the
    // visibility predicate cannot leak cross-tenant even if a future
    // refactor admits `host:` past the handshake gate. Operators who
    // need a privileged "see all events" subscription must add an
    // explicit K9 `Allow` rule for their administrative agent_id +
    // namespace pattern.
    if subscriber_agent.starts_with("host:") {
        return false;
    }
    let event_agent = event.tenant_agent_id();
    if !event_agent.is_empty() && event_agent == subscriber_agent {
        return true;
    }
    let event_namespace = event.tenant_namespace();
    if event_namespace.is_empty() {
        // No namespace hint on the event → fall back to the strict
        // agent-id match above; we will not leak cross-agent.
        return false;
    }
    let rules = crate::permissions::active_permission_rules();
    rules.iter().any(|r| {
        matches!(r.decision, crate::permissions::RuleDecision::Allow)
            && crate::permissions::glob_matches(&r.agent_pattern, subscriber_agent)
            && crate::permissions::glob_matches(&r.namespace_pattern, event_namespace)
    })
}

/// `GET /api/v1/approvals/stream` — SSE endpoint streaming
/// `approval_requested` and `approval_decided` events from the
/// process-wide broadcast bus.
///
/// Returns the axum SSE response. Each event is a JSON-encoded
/// [`crate::approvals::ApprovalEvent`] payload tagged with `event:
/// approval_requested` (or `_decided`) per the SSE spec. A keepalive
/// comment line fires every 15 s to prevent intermediary timeouts.
///
/// **Tenant isolation (review #628 blocker C2)**: the subscriber's
/// `agent_id` is captured at subscribe time from the `X-Agent-Id`
/// header (HMAC is impractical on a long-lived empty-body GET) and
/// every event is filtered through [`sse_event_visible_to`] before
/// fan-out. Cross-tenant events are silently dropped — the
/// subscriber sees only their own pending rows and decisions, plus
/// rows in namespaces an active K9 `Allow` rule grants them.
pub async fn approvals_sse(
    State(_app): State<AppState>,
    headers: HeaderMap,
) -> axum::response::Sse<
    impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_core::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration as StdDuration;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

    // Resolve the subscriber's agent_id from the `X-Agent-Id` header
    // (the K10 SSE endpoint sits behind `api_key_auth`; HMAC signing
    // is impractical on a long-lived GET stream with an empty body).
    // An unidentified subscriber gets an empty agent_id and
    // `sse_event_visible_to` refuses all events (fail-closed).
    //
    // Security (#628 P1, agent-4 finding): reject self-asserted
    // `host:`-prefixed agent_ids. `host:` is the server-side fallback
    // produced by `identity::resolve_agent_id` when no agent_id is
    // supplied; it must never be accepted from an external client
    // (which would otherwise gain a privilege-escalation path through
    // the historical `host:` bypass in `sse_event_visible_to`). A
    // client passing `X-Agent-Id: host:…` is treated as anonymous
    // (empty subscriber_agent → fail-closed).
    let subscriber_agent = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.starts_with("host:"))
        .unwrap_or("")
        .to_string();

    /// Bridges a `BroadcastStream<ApprovalEvent>` into the
    /// `Stream<Item = Result<Event, Infallible>>` axum's `Sse` requires.
    /// We swallow `Lagged` by emitting a synthetic `lagged` SSE event
    /// so subscribers can re-sync via `GET /api/v1/pending` instead of
    /// silently missing frames; channel `Closed` ends the stream.
    /// Cross-tenant events are silently dropped via the
    /// `subscriber_agent` filter so a noisy neighbour cannot leak
    /// metadata into a different tenant's SSE feed.
    struct ApprovalSseStream {
        inner: BroadcastStream<crate::approvals::ApprovalEvent>,
        subscriber_agent: String,
    }

    impl Stream for ApprovalSseStream {
        type Item = Result<Event, std::convert::Infallible>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            loop {
                match Pin::new(&mut self.inner).poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Ready(Some(Ok(evt))) => {
                        if !sse_event_visible_to(&self.subscriber_agent, &evt) {
                            // Cross-tenant: skip without surfacing
                            // anything to this subscriber. Loop to
                            // poll the next frame.
                            continue;
                        }
                        let (event_name, json_value) = match &evt {
                            crate::approvals::ApprovalEvent::ApprovalRequested { .. } => {
                                ("approval_requested", serde_json::to_value(&evt))
                            }
                            crate::approvals::ApprovalEvent::ApprovalDecided { .. } => {
                                ("approval_decided", serde_json::to_value(&evt))
                            }
                        };
                        // #869 — silently degrading to `Value::Null`
                        // (the prior `unwrap_or_default()`) would have
                        // surfaced an SSE frame with an empty body that
                        // looked indistinguishable from a malformed
                        // event. Log + emit a typed `error` event so
                        // subscribers can re-sync via REST instead of
                        // mis-parsing the stream.
                        let data = match json_value {
                            Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()),
                            Err(e) => {
                                tracing::error!(
                                    "approvals_sse: serialise ApprovalEvent failed: {e}"
                                );
                                return Poll::Ready(Some(Ok(Event::default()
                                    .event("error")
                                    .data(r#"{"error":"event_serialise_failed"}"#))));
                            }
                        };
                        return Poll::Ready(Some(Ok(Event::default()
                            .event(event_name)
                            .data(data))));
                    }
                    Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_n)))) => {
                        // P4 (#628 agent-4): the lagged-event count `n`
                        // reflects cross-tenant traffic volume — leaking
                        // it lets a noisy-neighbour fingerprint other
                        // tenants' approval rates. Surface only "we
                        // lagged"; subscribers re-sync via
                        // GET /api/v1/pending and don't need the count.
                        let body = serde_json::json!({"lagged": true}).to_string();
                        return Poll::Ready(Some(Ok(Event::default().event("lagged").data(body))));
                    }
                }
            }
        }
    }

    let rx = crate::approvals::subscribe();
    let stream = ApprovalSseStream {
        inner: BroadcastStream::new(rx),
        subscriber_agent,
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(StdDuration::from_secs(15)))
}
