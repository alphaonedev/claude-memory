// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP pending-approval handlers and decision recording.

use crate::{db, validate};
use serde_json::{Value, json};
/// v0.7 K7 — MCP handler for `memory_subscription_dlq_list`. Wraps
/// [`crate::subscriptions::list_dlq`] and applies the optional
/// `limit` cap (default 100, max 1000) so an operator inspecting a
/// runaway DLQ can't blow the response size budget. Family: `Power`.

pub(crate) fn handle_subscription_dlq_list(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let subscription_id = params["subscription_id"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(100))
        .unwrap_or(100)
        .clamp(1, 1000);
    let mut rows =
        crate::subscriptions::list_dlq(conn, subscription_id).map_err(|e| e.to_string())?;
    if rows.len() > limit {
        rows.truncate(limit);
    }
    Ok(json!({
        "count": rows.len(),
        "subscription_id": subscription_id,
        "limit": limit,
        "entries": rows,
    }))
}

pub(super) fn handle_pending_list(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let status = params["status"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(100))
        .unwrap_or(usize::MAX)
        .min(1000);
    let items = db::list_pending_actions(conn, status, limit).map_err(|e| e.to_string())?;
    Ok(json!({"count": items.len(), "pending": items}))
}

/// v0.7 K10 — parse the optional `remember` MCP param.
///
/// Defaults to `Once` when absent or invalid (the K10 contract is
/// best-effort: a typoed `remember` value MUST NOT block the underlying
/// approve/reject path). Validation drift is logged at WARN so
/// operators can see the regression without it surfacing as a
/// caller-facing error.
fn parse_remember_param(params: &Value) -> crate::approvals::Remember {
    match params["remember"].as_str() {
        Some("session") => crate::approvals::Remember::Session,
        Some("forever") => crate::approvals::Remember::Forever,
        Some("once") | None => crate::approvals::Remember::Once,
        Some(other) => {
            tracing::warn!(
                "memory_pending_*: unknown remember value {other:?}, defaulting to once"
            );
            crate::approvals::Remember::Once
        }
    }
}

/// v0.7 K10 — record a synthetic rule + publish on the approval bus
/// for an MCP-side approve/reject. Mirrors the HTTP-side hook in
/// `handlers::approval_decide` so the three transports stay
/// behaviourally identical.
fn record_mcp_decision(
    conn: &rusqlite::Connection,
    pending_id: &str,
    decided_by: &str,
    decision_label: &str,
    remember: crate::approvals::Remember,
) {
    let pa = crate::db::get_pending_action(conn, pending_id)
        .ok()
        .flatten();
    let remember_label = match remember {
        crate::approvals::Remember::Once => "once",
        crate::approvals::Remember::Session => "session",
        crate::approvals::Remember::Forever => "forever",
    };
    // Carry the originating namespace + requester onto the bus so the
    // K10 SSE handler can scope this decision to the right tenant
    // (review #628 blocker C2). Snapshot may be absent if the row was
    // already swept; the SSE filter treats empty fields as "no tenant
    // hint" and falls back to the subscriber's K9 policy.
    let evt_namespace = pa.as_ref().map(|p| p.namespace.clone()).unwrap_or_default();
    let evt_requested_by = pa
        .as_ref()
        .map(|p| p.requested_by.clone())
        .unwrap_or_default();
    crate::approvals::publish(crate::approvals::ApprovalEvent::ApprovalDecided {
        pending_id: pending_id.to_string(),
        decision: decision_label.to_string(),
        decided_by: decided_by.to_string(),
        remember: remember_label.to_string(),
        namespace: evt_namespace,
        requested_by: evt_requested_by,
    });
    if matches!(
        remember,
        crate::approvals::Remember::Forever | crate::approvals::Remember::Session
    ) && let Some(snap) = pa
    {
        crate::approvals::record_synthetic_rule(crate::approvals::SyntheticPermissionRule {
            action_type: snap.action_type,
            namespace: snap.namespace,
            agent_id: Some(snap.requested_by),
            decision: decision_label.to_string(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        });
    }
}

pub fn handle_pending_approve(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    use crate::db::ApproveOutcome;
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
        .map_err(|e| e.to_string())?;
    let remember = parse_remember_param(params);
    match db::approve_with_approver_type(conn, id, &agent_id).map_err(|e| e.to_string())? {
        ApproveOutcome::Approved => {
            // Task 1.10: auto-execute the queued action on final approval.
            let executed = db::execute_pending_action(conn, id).map_err(|e| e.to_string())?;
            record_mcp_decision(conn, id, &agent_id, "approve", remember);
            Ok(json!({
                "approved": true,
                "id": id,
                "decided_by": agent_id,
                "executed": true,
                "memory_id": executed,
                "remember": match remember {
                    crate::approvals::Remember::Once => "once",
                    crate::approvals::Remember::Session => "session",
                    crate::approvals::Remember::Forever => "forever",
                },
            }))
        }
        ApproveOutcome::Pending { votes, quorum } => Ok(json!({
            "approved": false,
            "status": "pending",
            "id": id,
            "votes": votes,
            "quorum": quorum,
            "reason": "consensus threshold not yet reached",
        })),
        ApproveOutcome::Rejected(reason) => Err(format!("approve rejected: {reason}")),
    }
}

pub fn handle_pending_reject(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
        .map_err(|e| e.to_string())?;
    let remember = parse_remember_param(params);
    let transitioned =
        db::decide_pending_action(conn, id, false, &agent_id).map_err(|e| e.to_string())?;
    if !transitioned {
        return Err(format!("pending action not found or already decided: {id}"));
    }
    record_mcp_decision(conn, id, &agent_id, "deny", remember);
    Ok(json!({
        "rejected": true,
        "id": id,
        "decided_by": agent_id,
        "remember": match remember {
            crate::approvals::Remember::Once => "once",
            crate::approvals::Remember::Session => "session",
            crate::approvals::Remember::Forever => "forever",
        },
    }))
}
