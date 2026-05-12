// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_delete` handler.

use crate::mcp::VectorIndex;
use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;
pub(super) fn handle_delete(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    vector_index: Option<&VectorIndex>,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;

    // Resolve the memory first so governance has owner context.
    let target = if let Some(m) = db::get(conn, id).map_err(|e| e.to_string())? {
        Some(m)
    } else {
        db::get_by_prefix(conn, id).map_err(|e| e.to_string())?
    };
    let Some(target) = target else {
        return Err("memory not found".into());
    };

    // P5 (G9): snapshot fields the dispatcher needs BEFORE delete frees
    // the row. The dispatch itself is fire-and-forget after the DELETE
    // commits, but the payload is built from this owned snapshot.
    let snapshot_namespace = target.namespace.clone();
    let snapshot_title = target.title.clone();
    let snapshot_tier = target.tier.as_str().to_string();
    let snapshot_owner: Option<String> = target
        .metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // v0.7.0 K9 — unified permission pipeline (delete-side).
    {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
            .map_err(|e| e.to_string())?;
        let payload = json!({"id": target.id, "title": target.title});
        let ctx = PermissionContext {
            op: Op::MemoryDelete,
            namespace: target.namespace.clone(),
            agent_id,
            payload,
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("delete denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "delete",
                    "memory_id": target.id,
                }));
            }
        }
    }

    // Task 1.9: governance enforcement (delete-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
            .map_err(|e| e.to_string())?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({"id": target.id, "title": target.title});
        match db::enforce_governance(
            conn,
            GovernedAction::Delete,
            &target.namespace,
            &agent_id,
            Some(&target.id),
            mem_owner.as_deref(),
            &payload,
        )
        .map_err(|e| e.to_string())?
        {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                return Err(format!("delete denied by governance: {reason}"));
            }
            GovernanceDecision::Pending(pending_id) => {
                // v0.7.0 K4 — see the store-side companion call.
                crate::subscriptions::dispatch_approval_requested(conn, &pending_id, db_path);
                return Ok(json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": "delete",
                    "memory_id": target.id,
                }));
            }
        }
    }

    let deleted = db::delete(conn, &target.id).map_err(|e| e.to_string())?;
    if deleted {
        if let Some(idx) = vector_index {
            idx.remove(&target.id);
        }
        // PR-5 (issue #487): security audit trail. No-op when disabled.
        crate::audit::emit(crate::audit::EventBuilder::new(
            crate::audit::AuditAction::Delete,
            crate::audit::actor(
                snapshot_owner
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                mcp_client.map_or("host_fallback", |_| "mcp_client_info"),
                None,
            ),
            crate::audit::target_memory(
                target.id.clone(),
                snapshot_namespace.clone(),
                Some(snapshot_title.clone()),
                Some(snapshot_tier.clone()),
                None,
            ),
        ));
        // P5 (G9): fire `memory_delete` webhook AFTER the row is gone
        // (best-effort, fire-and-forget — same pattern as memory_store).
        let details = serde_json::to_value(crate::subscriptions::DeleteEventDetails {
            title: snapshot_title,
            tier: snapshot_tier,
        })
        .ok();
        crate::subscriptions::dispatch_event_with_details(
            conn,
            "memory_delete",
            &target.id,
            &snapshot_namespace,
            snapshot_owner.as_deref(),
            db_path,
            details,
        );
        Ok(json!({"deleted": true}))
    } else {
        Err("memory not found".into())
    }
}
