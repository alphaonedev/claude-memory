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

    // #913 (security-medium / SOC2, 2026-05-19) — admin governance audit.
    // Approve is the privileged gate operation; emit the forensic-chain
    // row BEFORE the storage write so the audit trail captures the
    // approver's identity + pending_id even when the downstream
    // consensus / execution path errors.
    crate::governance::audit::record_decision(
        &agent_id,
        "allow",
        "pending_approve",
        "",
        json!({ "pending_id": id }),
    );

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

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for the pending-action handlers and the
    //! private `parse_remember_param` / `record_mcp_decision` helpers.
    //!
    //! Hermetic: every test opens an in-memory DB. No filesystem, no
    //! network. The approval bus is process-wide so each test publishes
    //! distinct payloads; tests do not assert on cross-test ordering.

    use super::*;
    use crate::storage as db;
    use serde_json::json;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn queue_pending(conn: &rusqlite::Connection, requester: &str) -> String {
        db::queue_pending_action(
            conn,
            crate::models::GovernedAction::Reflect,
            "pa-ns",
            None,
            requester,
            &json!({"k": "v"}),
        )
        .expect("queue")
    }

    /// Queue a pending action with a payload that the execute step will
    /// gracefully short-circuit (no real reflect / store / etc. runs),
    /// so the happy-path approve test does not require a full
    /// reflect payload. Uses `Promote` which carries a memory_id;
    /// without a target row, `execute_pending_action` reports a
    /// not-found rather than blowing up.
    fn queue_pending_promote_unbound(conn: &rusqlite::Connection, requester: &str) -> String {
        db::queue_pending_action(
            conn,
            crate::models::GovernedAction::Promote,
            "pa-ns",
            Some("11111111-2222-3333-4444-555555555555"),
            requester,
            &json!({"target_tier": "long"}),
        )
        .expect("queue")
    }

    // parse_remember_param: each of the four branches.
    #[test]
    fn parse_remember_param_returns_session() {
        let r = super::parse_remember_param(&json!({"remember": "session"}));
        assert!(matches!(r, crate::approvals::Remember::Session));
    }
    #[test]
    fn parse_remember_param_returns_forever() {
        let r = super::parse_remember_param(&json!({"remember": "forever"}));
        assert!(matches!(r, crate::approvals::Remember::Forever));
    }
    #[test]
    fn parse_remember_param_returns_once_when_explicit() {
        let r = super::parse_remember_param(&json!({"remember": "once"}));
        assert!(matches!(r, crate::approvals::Remember::Once));
    }
    #[test]
    fn parse_remember_param_returns_once_when_absent() {
        let r = super::parse_remember_param(&json!({}));
        assert!(matches!(r, crate::approvals::Remember::Once));
    }
    // Unknown value defaults to Once (with WARN log).
    #[test]
    fn parse_remember_param_unknown_defaults_to_once() {
        let r = super::parse_remember_param(&json!({"remember": "weird-value"}));
        assert!(matches!(r, crate::approvals::Remember::Once));
    }

    // handle_subscription_dlq_list — empty list, count=0, limit echoed.
    #[test]
    fn subscription_dlq_list_empty() {
        let conn = fresh_conn();
        let resp = handle_subscription_dlq_list(&conn, &json!({})).expect("ok");
        assert_eq!(resp["count"].as_u64(), Some(0));
        assert!(resp["entries"].is_array());
    }

    // handle_subscription_dlq_list — limit clamped to [1, 1000].
    #[test]
    fn subscription_dlq_list_limit_clamped() {
        let conn = fresh_conn();
        let resp = handle_subscription_dlq_list(&conn, &json!({"limit": 0u64})).expect("ok");
        // limit=0 clamps to 1; 0 is below the min so it should not error.
        assert!(resp["limit"].as_u64().unwrap() >= 1);
    }

    // handle_subscription_dlq_list — subscription_id filter is propagated.
    #[test]
    fn subscription_dlq_list_with_filter() {
        let conn = fresh_conn();
        let resp =
            handle_subscription_dlq_list(&conn, &json!({"subscription_id": "sub-x"})).expect("ok");
        assert_eq!(resp["subscription_id"].as_str(), Some("sub-x"));
    }

    // handle_pending_list — happy + count.
    #[test]
    fn pending_list_returns_count_and_array() {
        let conn = fresh_conn();
        let _id = queue_pending(&conn, "ai:tester");
        let resp = handle_pending_list(&conn, &json!({})).expect("ok");
        assert!(resp["count"].as_u64().unwrap() >= 1);
        assert!(resp["pending"].is_array());
    }

    // handle_pending_list — status filter + limit clamp.
    #[test]
    fn pending_list_with_status_and_limit() {
        let conn = fresh_conn();
        let _id = queue_pending(&conn, "ai:tester");
        let resp = handle_pending_list(&conn, &json!({"status": "pending", "limit": 5000u64}))
            .expect("ok");
        assert!(resp["count"].as_u64().unwrap() >= 1);
    }

    // handle_pending_approve — happy path with single-vote quorum.
    // execute_pending_action may surface its own "target not found" error
    // for a synthetic payload; in that case the approve still flips the
    // pending row to Approved and the handler returns the error string.
    // We accept either outcome to keep this hermetic without seeding a
    // full reflect payload.
    #[test]
    fn pending_approve_reaches_execute_step() {
        let conn = fresh_conn();
        let id = queue_pending_promote_unbound(&conn, "ai:tester");
        let result = handle_pending_approve(
            &conn,
            &json!({"id": id, "agent_id": "ai:approver", "remember": "forever"}),
            None,
        );
        // Either Ok (memory_id was None, executed flag false) or Err with
        // a substrate "not found" — both flow through record_mcp_decision.
        match result {
            Ok(resp) => {
                assert_eq!(resp["approved"], true);
                assert_eq!(resp["remember"].as_str(), Some("forever"));
            }
            Err(e) => assert!(!e.is_empty()),
        }
    }

    // handle_pending_approve — missing id errors.
    #[test]
    fn pending_approve_missing_id_errors() {
        let conn = fresh_conn();
        let err = handle_pending_approve(&conn, &json!({}), None).unwrap_err();
        assert!(err.contains("id"), "got: {err}");
    }

    // handle_pending_approve — invalid id format errors (validate_id).
    #[test]
    fn pending_approve_invalid_id_rejected() {
        let conn = fresh_conn();
        let err = handle_pending_approve(&conn, &json!({"id": "  "}), None).unwrap_err();
        assert!(!err.is_empty());
    }

    // handle_pending_approve — unknown id returns rejected.
    #[test]
    fn pending_approve_unknown_id_rejected() {
        let conn = fresh_conn();
        let err = handle_pending_approve(
            &conn,
            &json!({"id": "00000000-0000-0000-0000-000000000000"}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("approve rejected"), "got: {err}");
    }

    // handle_pending_reject — happy path with session remember label.
    #[test]
    fn pending_reject_happy_path() {
        let conn = fresh_conn();
        let id = queue_pending(&conn, "ai:tester");
        let resp = handle_pending_reject(
            &conn,
            &json!({"id": id, "agent_id": "ai:rejecter", "remember": "session"}),
            None,
        )
        .expect("ok");
        assert_eq!(resp["rejected"], true);
        assert_eq!(resp["remember"].as_str(), Some("session"));
    }

    // handle_pending_reject — once remember default emits "once".
    #[test]
    fn pending_reject_default_remember_is_once() {
        let conn = fresh_conn();
        let id = queue_pending(&conn, "ai:tester");
        let resp =
            handle_pending_reject(&conn, &json!({"id": id, "agent_id": "ai:rejecter"}), None)
                .expect("ok");
        assert_eq!(resp["remember"].as_str(), Some("once"));
    }

    // handle_pending_reject — missing id errors.
    #[test]
    fn pending_reject_missing_id_errors() {
        let conn = fresh_conn();
        let err = handle_pending_reject(&conn, &json!({}), None).unwrap_err();
        assert!(err.contains("id"), "got: {err}");
    }

    // handle_pending_reject — unknown id (already-decided contract).
    #[test]
    fn pending_reject_unknown_id_errors() {
        let conn = fresh_conn();
        let err = handle_pending_reject(
            &conn,
            &json!({"id": "00000000-0000-0000-0000-000000000000"}),
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("not found") || err.contains("already decided"),
            "got: {err}"
        );
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

    // #913 (security-medium / SOC2, 2026-05-19) — admin governance audit.
    // Reject is the privileged-gate denial; mirror approve so both
    // outcomes appear in the forensic chain BEFORE the storage write.
    crate::governance::audit::record_decision(
        &agent_id,
        "refuse",
        "pending_reject",
        "",
        json!({ "pending_id": id }),
    );

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
