// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_link` and `memory_get_links` handlers.

use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;

/// Relation string for the recursive-learning reflection edge.
const REFLECTS_ON: &str = "reflects_on";

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

    // v0.7.0 K9 — unified permission pipeline (link-side).
    // Link evaluation uses the *source* memory's namespace (the
    // originating end of the relation) so policies can scope by
    // who is allowed to outbound-link from a given namespace.
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
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("link denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "link",
                    "source_id": source_id,
                    "target_id": target_id,
                }));
            }
        }
    }

    // v0.7.0 L1-2 (#659) — anti-cycle guard for `reflects_on` edges.
    //
    // Adding a `reflects_on` edge that closes a cycle in the reflection
    // graph is a logical contradiction (A derived from B which was derived
    // from A) and is refused here before any quota is charged.  The cycle
    // check walks backward from `target_id` via existing `reflects_on`
    // edges, bounded by `max_reflection_depth` so it can't spin forever
    // on a pathological graph.  On hit, a refusal row is appended to
    // `signed_events` (audit-chain obligation) before returning the error.
    if relation == REFLECTS_ON {
        use crate::kg::cycle_check::would_create_reflection_cycle;
        use crate::models::GovernancePolicy;

        let source_ns = match db::get(conn, source_id) {
            Ok(Some(m)) => m.namespace,
            _ => "global".to_string(),
        };
        let policy = db::resolve_governance_policy(conn, &source_ns)
            .unwrap_or_else(GovernancePolicy::default);
        let max_depth = policy.effective_max_reflection_depth();

        let check = would_create_reflection_cycle(conn, source_id, target_id, max_depth);
        if check.would_cycle {
            // Append refusal to signed_events (best-effort; log on failure).
            let refusal_payload = serde_json::json!({
                "event": "reflects_on.cycle_refused",
                "source_id": source_id,
                "target_id": target_id,
                "cycle_path": check.cycle_path,
            });
            let cbor_bytes = refusal_payload.to_string().into_bytes();
            let audit_event = crate::signed_events::SignedEvent {
                id: uuid::Uuid::new_v4().to_string(),
                agent_id: params["agent_id"]
                    .as_str()
                    .unwrap_or("anonymous")
                    .to_string(),
                event_type: "reflects_on.cycle_refused".to_string(),
                payload_hash: crate::signed_events::payload_hash(&cbor_bytes),
                signature: None,
                attest_level: "unsigned".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            };
            if let Err(e) = crate::signed_events::append_signed_event(conn, &audit_event) {
                tracing::warn!(
                    target: "signed_events",
                    source_id, target_id,
                    "failed to append reflects_on.cycle_refused audit row: {e}"
                );
            }

            let err = crate::errors::MemoryError::ReflectionCycleDetected {
                source: source_id.to_string(),
                target: target_id.to_string(),
                cycle_path: check.cycle_path,
            };
            return Err(err.message());
        }
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
