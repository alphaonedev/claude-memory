// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_kg_invalidate` handler.

use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;
/// SEC-2 / COV-8 (Cluster D, issue #767) — `pub` so the integration
/// test fleet can drive the handler directly. The function is still
/// only registered as the MCP `memory_kg_invalidate` tool; visibility
/// is the only thing that changed.
pub fn handle_kg_invalidate(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    let target_id = params["target_id"]
        .as_str()
        .ok_or("target_id is required")?;
    let relation = params["relation"].as_str().ok_or("relation is required")?;
    validate::validate_link(source_id, target_id, relation).map_err(|e| e.to_string())?;
    let valid_until = params["valid_until"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ts) = valid_until {
        validate::validate_expires_at_format(ts).map_err(|e| e.to_string())?;
    }

    // v0.7.0 K9 (#628 H5/H6/I1 follow-up): the unified permission
    // pipeline must gate `kg_invalidate` symmetrically with
    // `handle_link`. Without this gate, a cross-tenant call could
    // NULL another tenant's signed-link signature (H5 supersession
    // semantic clears the signature row). Scope evaluation by the
    // *source* memory's namespace — same convention as `handle_link`
    // — so the same `[permissions.rules] action_type = "link"` rule
    // applies to both create-link and invalidate-link.
    {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let link_ns = match db::get(conn, source_id) {
            Ok(Some(m)) => m.namespace,
            _ => "global".to_string(),
        };
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), None)
            .map_err(|e| e.to_string())?;
        let ctx = PermissionContext {
            op: Op::MemoryLink,
            namespace: link_ns,
            agent_id,
            payload: json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": relation,
                "operation": "invalidate",
            }),
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("kg_invalidate denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "kg_invalidate",
                    "source_id": source_id,
                    "target_id": target_id,
                }));
            }
        }
    }

    match db::invalidate_link(conn, source_id, target_id, relation, valid_until)
        .map_err(|e| e.to_string())?
    {
        Some(res) => {
            // v0.7 J4 / G14 — emit `memory_link_invalidated` webhook
            // event AFTER the supersession is persisted. Mirrors the
            // `memory_link_created` dispatch in `handle_link`: pull
            // namespace + agent_id from the source memory so
            // subscribers see the canonical envelope, then flatten
            // the supersession-specific details (target/relation +
            // both timestamps) into the payload. This is the G14
            // audit-edge pattern — every invalidation surfaces as a
            // replayable event without requiring a separate audit
            // table on the SQLite path.
            let (event_namespace, event_agent_id) = match db::get(conn, source_id) {
                Ok(Some(mem)) => {
                    let owner = mem
                        .metadata
                        .get("agent_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    (mem.namespace, owner)
                }
                _ => ("global".to_string(), None),
            };
            let details = serde_json::to_value(crate::subscriptions::LinkInvalidatedEventDetails {
                target_id: target_id.to_string(),
                relation: relation.to_string(),
                valid_until: res.valid_until.clone(),
                previous_valid_until: res.previous_valid_until.clone(),
            })
            .ok();
            crate::subscriptions::dispatch_event_with_details(
                conn,
                "memory_link_invalidated",
                source_id,
                &event_namespace,
                event_agent_id.as_deref(),
                db_path,
                details,
            );

            Ok(json!({
                "found": true,
                "source_id": source_id,
                "target_id": target_id,
                "relation": relation,
                "valid_until": res.valid_until,
                "previous_valid_until": res.previous_valid_until,
            }))
        }
        None => Ok(json!({
            "found": false,
            "source_id": source_id,
            "target_id": target_id,
            "relation": relation,
        })),
    }
}
