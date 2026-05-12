// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_promote` handler.

use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;
pub(super) fn handle_promote(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    // Resolve prefix if exact ID not found; capture the memory so governance
    // has owner context (Task 1.9).
    let target = if let Some(m) = db::get(conn, id).map_err(|e| e.to_string())? {
        m
    } else if let Some(m) = db::get_by_prefix(conn, id).map_err(|e| e.to_string())? {
        m
    } else {
        return Err("memory not found".into());
    };
    let resolved_id = target.id.clone();
    // P5 (G9): snapshot fields needed for the post-success webhook.
    let snapshot_namespace = target.namespace.clone();
    let snapshot_owner: Option<String> = target
        .metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Task 1.9: governance enforcement (promote-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
            .map_err(|e| e.to_string())?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({
            "id": resolved_id,
            "to_namespace": params["to_namespace"].as_str(),
        });
        match db::enforce_governance(
            conn,
            GovernedAction::Promote,
            &target.namespace,
            &agent_id,
            Some(&resolved_id),
            mem_owner.as_deref(),
            &payload,
        )
        .map_err(|e| e.to_string())?
        {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                return Err(format!("promote denied by governance: {reason}"));
            }
            GovernanceDecision::Pending(pending_id) => {
                // v0.7.0 K4 — see the store-side companion call.
                crate::subscriptions::dispatch_approval_requested(conn, &pending_id, db_path);
                return Ok(json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": "promote",
                    "memory_id": resolved_id,
                }));
            }
        }
    }

    // Task 1.7: optional vertical promotion to an ancestor namespace.
    // When `to_namespace` is supplied, clone (don't move) the memory to the
    // target and link clone → source with `derived_from`. Original is
    // untouched; tier is NOT changed by this path.
    if let Some(to_ns) = params["to_namespace"].as_str() {
        validate::validate_namespace(to_ns).map_err(|e| e.to_string())?;
        let clone_id =
            db::promote_to_namespace(conn, &resolved_id, to_ns).map_err(|e| e.to_string())?;
        // P5 (G9): fire `memory_promote` webhook for vertical mode AFTER
        // the clone commits. memory_id = source id (subscribers can
        // distinguish via `mode` and `clone_id` in the details block).
        let details = serde_json::to_value(crate::subscriptions::PromoteEventDetails {
            mode: "vertical".to_string(),
            tier: None,
            to_namespace: Some(to_ns.to_string()),
            clone_id: Some(clone_id.clone()),
        })
        .ok();
        crate::subscriptions::dispatch_event_with_details(
            conn,
            "memory_promote",
            &resolved_id,
            &snapshot_namespace,
            snapshot_owner.as_deref(),
            db_path,
            details,
        );
        return Ok(json!({
            "promoted": true,
            "mode": "vertical",
            "source_id": resolved_id,
            "clone_id": clone_id,
            "to_namespace": to_ns,
        }));
    }

    // Default: tier promotion to long (historical behavior).
    let (found, _) = db::update(
        conn,
        &resolved_id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        Some(""), // empty string clears expires_at
        None,
    )
    .map_err(|e| e.to_string())?;
    if !found {
        return Err("memory not found".into());
    }
    // P5 (G9): fire `memory_promote` webhook for the default tier-upgrade
    // path AFTER the update commits.
    let details = serde_json::to_value(crate::subscriptions::PromoteEventDetails {
        mode: "tier".to_string(),
        tier: Some("long".to_string()),
        to_namespace: None,
        clone_id: None,
    })
    .ok();
    crate::subscriptions::dispatch_event_with_details(
        conn,
        "memory_promote",
        &resolved_id,
        &snapshot_namespace,
        snapshot_owner.as_deref(),
        db_path,
        details,
    );
    Ok(json!({"promoted": true, "mode": "tier", "id": resolved_id, "tier": "long"}))
}
