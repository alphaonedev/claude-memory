// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_link` and `memory_get_links` handlers.

use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;
pub(super) fn handle_link(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    let target_id = params["target_id"]
        .as_str()
        .ok_or("target_id is required")?;
    let relation = params["relation"].as_str().unwrap_or("related_to");

    validate::validate_link(source_id, target_id, relation).map_err(|e| e.to_string())?;

    // v0.7.0 K9 — unified permission pipeline (link-side), Ask
    // short-circuit only.
    //
    // v0.7.0 fix-campaign A3 (LINK-PARITY, #690): the Allow/Deny gate
    // has migrated to `storage::validate_link_pre_create` so the
    // HTTP, SAL, and federation-receive paths enforce the same K9
    // rules the MCP path does — closing the S5-H2 finding. The MCP
    // path retains a thin pre-call evaluate here for ONE reason: it
    // is the only entry point with a structured `Ask` channel back
    // to the operator (the `{"status":"ask", ...}` envelope). The
    // storage helper has no Ask channel and would surface Ask as
    // Deny; doing the Ask translation here keeps the MCP wire
    // contract unchanged. Allow / Deny outcomes ALSO get enforced
    // again by the storage layer, which is idempotent under the
    // registry's deny-first semantics.
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
            }),
        };
        if let crate::permissions::Decision::Ask(prompt) = Permissions::evaluate(&ctx, &[]) {
            return Ok(json!({
                "status": "ask",
                "reason": prompt,
                "action": "link",
                "source_id": source_id,
                "target_id": target_id,
            }));
        }
        // Allow / Deny / Modify fall through; the storage layer
        // (via create_link_signed → validate_link_pre_create) is the
        // authoritative gate for those outcomes.
    }

    // v0.7 K8 — per-agent quota gate. The link is charged against the
    // SOURCE memory's owner so a single agent fanning out links from
    // their own memories pays for them. If we can't resolve the owner
    // (source memory not found) the quota check is skipped:
    // db::create_link_signed will surface its own FK error in that
    // case, which is the more actionable failure.
    let link_agent_id = db::get(conn, source_id).ok().flatten().and_then(|mem| {
        mem.metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });
    // H12 (#628 blocker): combine the link quota check + counter
    // increment in a single atomic transaction. The check + record
    // pair was previously a TOCTOU window; `check_and_record` closes
    // it.
    if let Some(ref aid) = link_agent_id {
        if let Err(e) = crate::quotas::check_and_record(conn, aid, crate::quotas::QuotaOp::Link) {
            return Err(e.to_string());
        }
    }

    // v0.7 H2 — sign with active keypair when present; falls through
    // to attest_level="unsigned" otherwise. The chosen attest_level is
    // surfaced in the wire response so callers can tell signed vs
    // unsigned without re-querying.
    let attest_level =
        match db::create_link_signed(conn, source_id, target_id, relation, active_keypair) {
            Ok(v) => v,
            Err(e) => {
                // Refund the link counter we already committed: insert
                // failed downstream of the quota commit.
                if let Some(ref aid) = link_agent_id {
                    if let Err(re) =
                        crate::quotas::refund_op(conn, aid, crate::quotas::QuotaOp::Link)
                    {
                        tracing::warn!("quota refund_op failed for agent {}: {}", aid, re);
                    }
                }
                return Err(e.to_string());
            }
        };

    // P5 (G9): fire `memory_link_created` webhook AFTER the link is
    // persisted. Resolve the source memory to populate `namespace` /
    // `agent_id` for the dispatch envelope; if it's somehow gone (race
    // with delete) fall back to "global"/None and let the webhook
    // reflect the link metadata only.
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
    let details = serde_json::to_value(crate::subscriptions::LinkCreatedEventDetails {
        target_id: target_id.to_string(),
        relation: relation.to_string(),
    })
    .ok();
    crate::subscriptions::dispatch_event_with_details(
        conn,
        "memory_link_created",
        source_id,
        &event_namespace,
        event_agent_id.as_deref(),
        db_path,
        details,
    );

    Ok(json!({
        "linked": true,
        "source_id": source_id,
        "target_id": target_id,
        "relation": relation,
        // v0.7 H2 — wire-level visibility into whether the link was
        // signed by an Ed25519 keypair on this writer. "self_signed"
        // when active_keypair was Some + signing succeeded;
        // "unsigned" when no keypair was loaded.
        "attest_level": attest_level,
    }))
}

pub(super) fn handle_get_links(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let links = db::get_links(conn, id).map_err(|e| e.to_string())?;
    Ok(json!({"links": links, "count": links.len()}))
}

/// v0.7 H4 — parse the composite link_id form
/// `"<source_id>--<relation>-->\<target_id>"` into the three components
/// the SQL composite primary key uses. Returns `None` if the shape does
/// not match — callers fall back to the explicit `source_id`/`target_id`
/// parameter form.
///
/// Why this shape: `memory_links` has no synthetic surrogate key (the PK
/// is the composite tuple). H4's MCP tool needs *some* string-shaped
/// link identifier so a caller can name a link in one argument; this
/// form reads naturally in logs and is unambiguous because `--` and
/// `-->` are not valid characters inside a memory id (memory ids are
/// validated by `validate::validate_id`).
pub(super) fn parse_link_id(s: &str) -> Option<(String, String, String)> {
    // Returns `(source_id, target_id, relation)` to match the
    // destructuring shape `handle_verify` uses below.
    //
    // Split on the relation marker first (the only multi-char arrow in
    // the form) so a relation containing `--` would still parse — none
    // of the four valid relations contain it, but we keep the parser
    // permissive against future relation additions.
    let (left, target) = s.split_once("-->")?;
    let (source, relation) = left.split_once("--")?;
    if source.is_empty() || target.is_empty() || relation.is_empty() {
        return None;
    }
    Some((source.to_string(), target.to_string(), relation.to_string()))
}
