// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Governance HTTP handlers — pending-action list / approve / reject.
//!
//! Extracted from [`super::http`] under issue #650 follow-up 2. Wire
//! shape is identical (re-exported from [`super`] so `handlers::list_pending`
//! / `handlers::approve_pending` / `handlers::reject_pending` continue
//! to resolve). The K10 SSE approval stream lives in [`super::approvals`]
//! because it carries its own state (subscriber map).

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::db;
use crate::validate;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;
use super::fanout_or_503;
#[cfg(feature = "sal")]
use super::store_err_to_response;

#[derive(Deserialize)]
pub struct PendingListQuery {
    #[serde(default)]
    pub status: Option<String>,
    /// Optional namespace filter — S34 uses `?namespace=...&limit=50`.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_pending_limit")]
    pub limit: Option<usize>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_pending_limit() -> Option<usize> {
    Some(100)
}

pub async fn list_pending(
    State(app): State<AppState>,
    Query(p): Query<PendingListQuery>,
) -> impl IntoResponse {
    let limit = p.limit.unwrap_or(100).min(1000);

    // v0.7.0 Wave-3 Continuation 5 — postgres-backed daemons read
    // from the `pending_actions` table directly. The full governance
    // pipeline (Phase 20 / Cont 4 chain walk) writes pending rows on
    // both backends; this list path lights them up on the read side
    // so S34's "bob lists pending → approve/reject → charlie sees
    // approved" round-trip works end-to-end on postgres.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match crate::store::postgres::list_pending_actions_via_store(
            &app.store,
            p.status.as_deref(),
            p.namespace.as_deref(),
            limit,
        )
        .await
        {
            Ok(items) => Json(json!({
                "count": items.len(),
                "pending": items,
                "storage_backend": "postgres",
            }))
            .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::list_pending_actions(&lock.0, p.status.as_deref(), limit) {
        Ok(items) => Json(json!({"count": items.len(), "pending": items})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn approve_pending(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body_bytes: axum::body::Bytes,
) -> impl IntoResponse {
    use crate::db::ApproveOutcome;
    use crate::models::PendingDecision;
    // S5-C1 (v0.7.0 fix campaign 2026-05-13): privileged governance
    // endpoints MUST verify HMAC. The legacy `api_key_auth` middleware
    // pass-throughs when `api_key` is unset (default!), which means an
    // attacker could approve any pending action by spoofing `X-Agent-Id`.
    // We mirror the K10 SSE handler's posture and require
    // `X-AI-Memory-Signature` on every inbound approve request,
    // regardless of `api_key` configuration. Without a server-wide
    // `[hooks.subscription].hmac_secret`, `verify_approval_hmac`
    // refuses every request — the safe default.
    if let Err(status) = super::verify_approval_hmac(&headers, &body_bytes, "POST", &id) {
        return (
            status,
            Json(json!({
                "error": "invalid or missing X-AI-Memory-Signature",
                "hint": "POST /api/v1/pending/{id}/approve requires HMAC signing per K7's pattern. \
                        Set [hooks.subscription] hmac_secret in config and send \
                        X-AI-Memory-Signature: sha256=<HMAC-SHA256(SHA256(secret), \"<ts>.<METHOD>.<pending_id>.<body>\")> \
                        with X-AI-Memory-Timestamp: <unix-epoch-secs>."
            })),
        )
            .into_response();
    }
    let state = app.db.clone();
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
    // Approve is the canonical privileged gate operation; the forensic-
    // chain row MUST land before the storage write so the audit trail
    // captures the approver's identity + pending_id even when the
    // downstream consensus / execution path errors.
    crate::governance::audit::record_decision(
        &agent_id,
        "allow",
        "pending_approve",
        "",
        json!({ "pending_id": &id }),
    );

    // v0.7.0 Wave-3 Continuation 3 (Phase 20) — postgres-backed approve
    // routes through the FULL governance pipeline:
    // - inheritance-chain walk over `namespace_meta` (with explicit
    //   parent + `/`-derived ancestors, bounded + cycle-safe)
    // - approver_type variations: Human / Agent(required) / Consensus(N)
    // - multi-vote consensus state machine: registered-agent gating,
    //   case-insensitive duplicate-vote dedup, threshold transition
    // - audit emit + structured response envelope (Approved / Pending
    //   with vote count + quorum / Rejected with reason)
    //
    // Federation fanout for the decision + executed memory remains
    // sqlite-only (the broadcast_pending_decision_quorum path uses
    // sqlite-coupled fed-tracker state); postgres operators relying on
    // multi-node consistency should poll peers.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        use crate::store::ApproveOutcome as SalOutcome;
        let ctx = crate::store::CallerContext::for_agent(agent_id.clone());
        return match app
            .store
            .governance_approve_with_consensus(&ctx, &id, &agent_id)
            .await
        {
            Ok(SalOutcome::Approved) => {
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Approve,
                        crate::audit::actor(agent_id.clone(), "http_header", None),
                        crate::audit::target_memory(id.clone(), String::new(), None, None, None),
                    ));
                }
                // v0.7.0 Wave-3 Continuation 5 (S34) — execute the
                // approved action so the memory materialises in the
                // namespace where the cert oracle expects it. Mirrors
                // sqlite's `db::execute_pending_action` for the
                // `store` / `delete` / `promote` action types.
                let executed_id: Option<String> =
                    match app.store.execute_pending_action(&ctx, &id).await {
                        Ok(eid) => eid,
                        Err(e) => {
                            tracing::warn!(
                                "approve_pending: execute_pending_action failed for {id}: {e}"
                            );
                            None
                        }
                    };
                Json(json!({
                    "approved": true,
                    "id": id,
                    "decided_by": agent_id,
                    "executed": executed_id.is_some(),
                    "memory_id": executed_id,
                    "storage_backend": "postgres",
                }))
                .into_response()
            }
            Ok(SalOutcome::Pending { votes, quorum }) => (
                StatusCode::ACCEPTED,
                Json(json!({
                    "approved": false,
                    "status": "pending",
                    "id": id,
                    "votes": votes,
                    "quorum": quorum,
                    "reason": "consensus threshold not yet reached",
                    "storage_backend": "postgres",
                })),
            )
                .into_response(),
            Ok(SalOutcome::Rejected(reason)) => (
                StatusCode::FORBIDDEN,
                Json(json!({"error": format!("approve rejected: {reason}")})),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    match db::approve_with_approver_type(&lock.0, &id, &agent_id) {
        Ok(ApproveOutcome::Approved) => match db::execute_pending_action(&lock.0, &id) {
            Ok(memory_id) => {
                // v0.6.2 (S34): fan out the decision AND the resulting
                // memory so approve on one node makes the governed write
                // visible on every peer. Drop the DB lock before any
                // outbound HTTP.
                let produced_mem = memory_id
                    .as_deref()
                    .and_then(|mid| db::get(&lock.0, mid).ok().flatten());
                drop(lock);
                if let Some(fed) = app.federation.as_ref() {
                    let decision = PendingDecision {
                        id: id.clone(),
                        approved: true,
                        decider: agent_id.clone(),
                    };
                    match crate::federation::broadcast_pending_decision_quorum(fed, &decision).await
                    {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                // #869 — typed 503 envelope via the shared helper.
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return super::quorum_not_met_response(&payload);
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
                        }
                    }
                    // If approval produced a brand-new memory (store
                    // path), also broadcast it so peers have the row.
                    // delete / promote paths produce no new memory
                    // (the pending payload carries memory_id).
                    if let Some(ref mem) = produced_mem
                        && let Some(resp) = fanout_or_503(&app, mem).await
                    {
                        return resp;
                    }
                }
                Json(json!({
                    "approved": true,
                    "id": id,
                    "decided_by": agent_id,
                    "executed": true,
                    "memory_id": memory_id,
                }))
                .into_response()
            }
            Err(e) => {
                tracing::error!("execute pending error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "approved but execution failed"})),
                )
                    .into_response()
            }
        },
        Ok(ApproveOutcome::Pending { votes, quorum }) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "approved": false,
                "status": "pending",
                "id": id,
                "votes": votes,
                "quorum": quorum,
                "reason": "consensus threshold not yet reached",
            })),
        )
            .into_response(),
        Ok(ApproveOutcome::Rejected(reason)) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": format!("approve rejected: {reason}")})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn reject_pending(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body_bytes: axum::body::Bytes,
) -> impl IntoResponse {
    use crate::models::PendingDecision;
    // S5-C1 (v0.7.0 fix campaign 2026-05-13): parity with approve_pending.
    // Legacy reject endpoint MUST verify HMAC for the same reason — an
    // unsigned reject is just as dangerous (denial-of-service against
    // governance state, write-amplifies pending row churn).
    if let Err(status) = super::verify_approval_hmac(&headers, &body_bytes, "POST", &id) {
        return (
            status,
            Json(json!({
                "error": "invalid or missing X-AI-Memory-Signature",
                "hint": "POST /api/v1/pending/{id}/reject requires HMAC signing per K7's pattern. \
                        Set [hooks.subscription] hmac_secret in config and send \
                        X-AI-Memory-Signature: sha256=<HMAC-SHA256(SHA256(secret), \"<ts>.<METHOD>.<pending_id>.<body>\")> \
                        with X-AI-Memory-Timestamp: <unix-epoch-secs>."
            })),
        )
            .into_response();
    }
    let state = app.db.clone();
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
    // Reject is the privileged-gate denial path; mirror approve so both
    // outcomes appear in the forensic chain BEFORE the storage write.
    crate::governance::audit::record_decision(
        &agent_id,
        "refuse",
        "pending_reject",
        "",
        json!({ "pending_id": &id }),
    );

    // v0.7.0 Wave-3 Continuation 2 (Phase 11) — postgres-backed reject.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent(agent_id.clone());
        return match app.store.pending_decide(&ctx, &id, false, &agent_id).await {
            Ok(true) => {
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Reject,
                        crate::audit::actor(agent_id.clone(), "http_header", None),
                        crate::audit::target_memory(id.clone(), String::new(), None, None, None),
                    ));
                }
                Json(json!({
                    "rejected": true,
                    "id": id,
                    "decided_by": agent_id,
                    "storage_backend": "postgres",
                }))
                .into_response()
            }
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "pending action not found or already decided"})),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    match db::decide_pending_action(&lock.0, &id, false, &agent_id) {
        Ok(true) => {
            drop(lock);
            // v0.6.2 (S34): fan out the reject so peers converge.
            if let Some(fed) = app.federation.as_ref() {
                let decision = PendingDecision {
                    id: id.clone(),
                    approved: false,
                    decider: agent_id.clone(),
                };
                match crate::federation::broadcast_pending_decision_quorum(fed, &decision).await {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            // #869 — typed 503 envelope via the shared helper.
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
                        }
                    }
                    Err(err) => {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return super::quorum_not_met_response(&payload);
                    }
                }
            }
            Json(json!({"rejected": true, "id": id, "decided_by": agent_id})).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "pending action not found or already decided"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}
