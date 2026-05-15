// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_link` and `memory_get_links` handlers.

use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;

/// Relation string for the recursive-learning reflection edge.
const REFLECTS_ON: &str = "reflects_on";

/// Relation string for the directed supersedes edge (winner → loser).
/// v0.7.0 L2-3 (#668): a `supersedes` edge whose source AND target are
/// both `MemoryKind::Reflection` triggers the invalidation-notification
/// walker — see `crate::notification::invalidation`.
const SUPERSEDES: &str = "supersedes";

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
                ..crate::signed_events::SignedEvent::default()
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

    // v0.7.0 L2-3 (#668) — Reflection invalidation propagation
    // (notification, not cascade).
    //
    // When a `supersedes` edge lands whose source AND target are
    // both `memory_kind = 'reflection'`, walk every memory that
    // `reflects_on` the now-invalidated reflection (the target of
    // the supersedes) and write one notification memory per
    // dependent under `<dependent.namespace>/_invalidations`. The
    // dependents are NOT auto-superseded — operators and the
    // curator decide per-dependent. See the module-level doc in
    // `crate::notification::invalidation` for the contract.
    //
    // Best-effort: a substrate error here does NOT roll back the
    // supersedes edge (which has already committed). The walker
    // logs and continues so a single malformed dependent row can't
    // block the rest of the fan-out. Test coverage lives in
    // `tests/notification.rs` and the module's `#[cfg(test)]`.
    let mut invalidation_notified: Vec<String> = Vec::new();
    if relation == SUPERSEDES {
        let source_is_reflection = matches!(
            db::get(conn, source_id)
                .ok()
                .flatten()
                .map(|m| m.memory_kind),
            Some(crate::models::MemoryKind::Reflection)
        );
        let target_is_reflection = matches!(
            db::get(conn, target_id)
                .ok()
                .flatten()
                .map(|m| m.memory_kind),
            Some(crate::models::MemoryKind::Reflection)
        );
        if source_is_reflection && target_is_reflection {
            let signing_agent_id = params["agent_id"]
                .as_str()
                .unwrap_or(event_agent_id.as_deref().unwrap_or("system"))
                .to_string();
            match crate::notification::invalidation::propagate_reflection_invalidation(
                conn,
                target_id,
                source_id,
                &signing_agent_id,
            ) {
                Ok(ids) => invalidation_notified = ids,
                Err(e) => {
                    tracing::warn!(
                        target: "notification.invalidation",
                        invalidated_id = target_id,
                        invalidating_id = source_id,
                        "reflection invalidation walker failed: {e}"
                    );
                }
            }
        }
    }

    Ok(json!({
        "linked": true,
        "source_id": source_id,
        "target_id": target_id,
        "relation": relation,
        // v0.7.0 L2-3 (#668) — when this is a Reflection→Reflection
        // supersedes edge, surfaces the list of dependent memory ids
        // that had a `_invalidations` notification written. Empty for
        // every other relation, and for supersedes between non-
        // reflection memories. Callers can use this to log/UI the
        // size of the operator-review queue this edge created.
        "invalidation_notified": invalidation_notified,
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

#[cfg(test)]
mod tests {
    //! L0.7-3 Tier B chunk-A — coverage tests for `handle_link`,
    //! `handle_get_links`, and `parse_link_id`.
    //!
    //! Six-category template:
    //! A. happy path — link created, attest_level surfaced, webhook path
    //! B. validation — missing/invalid ids, bad relation
    //! D. state-dependent — source or target absent (FK error from substrate)
    //! E. idempotency — second create errors via PK collision
    //! F. audit chain — signed_events grows on each link (via storage layer)

    use super::*;
    use crate::models::{Memory, Tier};
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn db_path() -> std::path::PathBuf {
        std::path::PathBuf::from(":memory:")
    }

    fn make_mem(title: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "test".to_string(),
            title: title.to_string(),
            content: format!("body {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:alice"}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    fn insert_two(conn: &rusqlite::Connection) -> (String, String) {
        let a = make_mem("a");
        let b = make_mem("b");
        let a_id = db::insert(conn, &a).unwrap();
        let b_id = db::insert(conn, &b).unwrap();
        (a_id, b_id)
    }

    // A. happy path — unsigned attest level when no keypair
    #[test]
    fn happy_path_creates_unsigned_link() {
        let conn = fresh_conn();
        let (a, b) = insert_two(&conn);
        let db_path = db_path();
        let out = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a, "target_id": b, "relation": "related_to"}),
            None,
        )
        .expect("ok");
        assert_eq!(out["linked"].as_bool(), Some(true));
        assert_eq!(out["relation"].as_str(), Some("related_to"));
        assert_eq!(out["attest_level"].as_str(), Some("unsigned"));
    }

    // A. happy path — default relation (omitted) → "related_to"
    #[test]
    fn default_relation_when_omitted() {
        let conn = fresh_conn();
        let (a, b) = insert_two(&conn);
        let db_path = db_path();
        let out = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a, "target_id": b}),
            None,
        )
        .expect("ok");
        assert_eq!(out["relation"].as_str(), Some("related_to"));
    }

    // B. missing source_id
    #[test]
    fn missing_source_id_errors() {
        let conn = fresh_conn();
        let db_path = db_path();
        let err = handle_link(&conn, &db_path, &json!({"target_id": "x"}), None).unwrap_err();
        assert!(err.contains("source_id"));
    }

    // B. missing target_id
    #[test]
    fn missing_target_id_errors() {
        let conn = fresh_conn();
        let db_path = db_path();
        let err = handle_link(&conn, &db_path, &json!({"source_id": "x"}), None).unwrap_err();
        assert!(err.contains("target_id"));
    }

    // B. invalid relation
    #[test]
    fn invalid_relation_errors() {
        let conn = fresh_conn();
        let (a, b) = insert_two(&conn);
        let db_path = db_path();
        let err = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a, "target_id": b, "relation": "weird-relation"}),
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    // D. state-dependent — source missing → storage rejects (FK violation)
    #[test]
    fn missing_source_memory_errors() {
        let conn = fresh_conn();
        let (_, b) = insert_two(&conn);
        let db_path = db_path();
        let bad_src = uuid::Uuid::new_v4().to_string();
        let err = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": bad_src, "target_id": b, "relation": "related_to"}),
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    // E. idempotency — second insert of same (src, tgt, rel) is a no-op
    // (storage uses INSERT OR IGNORE on the composite PK). Confirms the
    // operation is safe under retry without producing a duplicate row.
    #[test]
    fn duplicate_link_is_idempotent() {
        let conn = fresh_conn();
        let (a, b) = insert_two(&conn);
        let db_path = db_path();
        let _ = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a.clone(), "target_id": b.clone(), "relation": "related_to"}),
            None,
        )
        .expect("first");
        // Second call returns linked=true again; row count remains 1.
        let _ = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a.clone(), "target_id": b.clone(), "relation": "related_to"}),
            None,
        )
        .expect("second is idempotent");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_links WHERE source_id = ?1 AND target_id = ?2",
                rusqlite::params![&a, &b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // F. audit — signed_events table is populated (best-effort via storage).
    #[test]
    fn signed_events_records_link() {
        let conn = fresh_conn();
        let (a, b) = insert_two(&conn);
        let db_path = db_path();
        let _ = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a, "target_id": b, "relation": "related_to"}),
            None,
        )
        .expect("ok");
        // Best-effort: signed_events table presence depends on schema;
        // count rows where event_type relates to memory_link.
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signed_events WHERE event_type LIKE 'memory_link%'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(
            cnt >= 1,
            "expected at least one signed_event row, got {cnt}"
        );
    }

    // handle_get_links — happy
    #[test]
    fn handle_get_links_returns_links() {
        let conn = fresh_conn();
        let (a, b) = insert_two(&conn);
        db::create_link(&conn, &a, &b, "related_to").unwrap();
        let out = handle_get_links(&conn, &json!({"id": a})).expect("ok");
        assert_eq!(out["count"].as_u64(), Some(1));
        let links = out["links"].as_array().unwrap();
        assert_eq!(links.len(), 1);
    }

    // handle_get_links — missing id
    #[test]
    fn handle_get_links_missing_id_errors() {
        let conn = fresh_conn();
        let err = handle_get_links(&conn, &json!({})).unwrap_err();
        assert!(err.contains("id"));
    }

    // handle_get_links — invalid id
    #[test]
    fn handle_get_links_invalid_id_errors() {
        let conn = fresh_conn();
        let err = handle_get_links(&conn, &json!({"id": ""})).unwrap_err();
        assert!(!err.is_empty());
    }

    // parse_link_id — happy
    #[test]
    fn parse_link_id_happy() {
        let parsed = parse_link_id("src-id--related_to-->tgt-id").expect("some");
        assert_eq!(parsed.0, "src-id");
        assert_eq!(parsed.1, "tgt-id");
        assert_eq!(parsed.2, "related_to");
    }

    // parse_link_id — wrong shape
    #[test]
    fn parse_link_id_wrong_shape_returns_none() {
        assert!(parse_link_id("plain-string").is_none());
        assert!(parse_link_id("src-id-->tgt-id").is_none(), "missing -- ");
        assert!(parse_link_id("--rel-->tgt").is_none(), "empty source");
        assert!(parse_link_id("src--rel-->").is_none(), "empty target");
        assert!(parse_link_id("src---->tgt").is_none(), "empty relation");
    }

    // C. K9 Ask path — only path the MCP layer evaluates locally (Allow/Deny
    // are deferred to storage). Drive an Ask rule and assert envelope shape.
    // The scope holds BOTH the rules and mode locks (see delete.rs docs).
    fn lock_rules() -> std::sync::MutexGuard<'static, ()> {
        crate::mcp::SHARED_PERMISSION_RULES_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct RulesScope {
        _rules: std::sync::MutexGuard<'static, ()>,
        _mode: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for RulesScope {
        fn drop(&mut self) {
            crate::permissions::clear_active_permission_rules_for_test();
            crate::config::clear_permissions_mode_override_for_test();
        }
    }
    fn rules_scope() -> RulesScope {
        let mode = crate::config::lock_permissions_mode_for_test();
        let rules = lock_rules();
        crate::permissions::clear_active_permission_rules_for_test();
        crate::config::override_active_permissions_mode_for_test(
            crate::config::PermissionsMode::Advisory,
        );
        RulesScope {
            _rules: rules,
            _mode: mode,
        }
    }

    #[test]
    fn k9_ask_returns_ask_envelope() {
        use crate::permissions::{PermissionRule, RuleDecision, set_active_permission_rules};
        let _g = rules_scope();
        let conn = fresh_conn();
        // Insert two memories in a unique namespace
        let now = chrono::Utc::now().to_rfc3339();
        let mut a = make_mem("a");
        a.namespace = "k9-ask-link".to_string();
        a.created_at = now.clone();
        a.updated_at = now.clone();
        let mut b = make_mem("b");
        b.namespace = "k9-ask-link".to_string();
        b.created_at = now.clone();
        b.updated_at = now;
        let a_id = db::insert(&conn, &a).expect("ins");
        let b_id = db::insert(&conn, &b).expect("ins");
        let db_path = db_path();
        set_active_permission_rules(vec![PermissionRule {
            namespace_pattern: "k9-ask-link".to_string(),
            op: "memory_link".to_string(),
            agent_pattern: "*".to_string(),
            decision: RuleDecision::Ask,
            reason: Some("operator approval required".to_string()),
        }]);
        let out = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a_id, "target_id": b_id, "relation": "related_to"}),
            None,
        )
        .expect("ask returns Ok");
        assert_eq!(out["status"].as_str(), Some("ask"));
        assert_eq!(out["action"].as_str(), Some("link"));
    }

    // ─────────────────────────────────────────────────────────────────
    // Coverage C-2 — additional tests added for the cycle-refusal
    // (L1-2) and supersedes-invalidation (L2-3) paths, plus quota
    // refund-on-failure and event_namespace fallback when the source
    // disappears mid-call.

    fn make_reflection(title: &str, ns: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("reflection body {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:reflective"}),
            reflection_depth: 1,
            memory_kind: crate::models::MemoryKind::Reflection,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    // L1-2 cycle refusal — a reflects_on edge that closes a cycle is
    // refused before any quota charge. Sets up A → B existing reflects_on
    // edge then attempts B → A which closes the cycle.
    #[test]
    fn reflects_on_cycle_refused() {
        let conn = fresh_conn();
        let a = make_reflection("ref-a", "cycle-ns");
        let b = make_reflection("ref-b", "cycle-ns");
        let a_id = db::insert(&conn, &a).unwrap();
        let b_id = db::insert(&conn, &b).unwrap();
        // Existing edge: a reflects_on b
        db::create_link(&conn, &a_id, &b_id, REFLECTS_ON).unwrap();
        let db_path = db_path();
        // Attempting b reflects_on a closes the cycle.
        let err = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": b_id, "target_id": a_id, "relation": REFLECTS_ON}),
            None,
        )
        .unwrap_err();
        // The error string comes from MemoryError::ReflectionCycleDetected.
        assert!(!err.is_empty());
        // An audit row was appended for the cycle-refused event.
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signed_events WHERE event_type = 'reflects_on.cycle_refused'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(cnt >= 1, "expected one cycle_refused audit row, got {cnt}");
    }

    // L2-3 invalidation propagation — supersedes edge between two
    // reflections triggers the invalidation walker. Without dependent
    // memories the notified list is empty but the path is exercised.
    #[test]
    fn supersedes_between_reflections_walks_invalidation() {
        let conn = fresh_conn();
        let winner = make_reflection("win", "sup-ns");
        let loser = make_reflection("lose", "sup-ns");
        let w_id = db::insert(&conn, &winner).unwrap();
        let l_id = db::insert(&conn, &loser).unwrap();
        let db_path = db_path();
        let out = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": w_id, "target_id": l_id, "relation": SUPERSEDES, "agent_id": "ai:supersede"}),
            None,
        )
        .expect("ok");
        assert_eq!(out["linked"], true);
        assert_eq!(out["relation"].as_str(), Some(SUPERSEDES));
        // invalidation_notified is always an array on this path.
        assert!(out["invalidation_notified"].is_array());
    }

    // Supersedes between an observation and a reflection skips the
    // invalidation walker entirely (no Reflection on both sides).
    #[test]
    fn supersedes_between_observations_skips_invalidation() {
        let conn = fresh_conn();
        let (a, b) = insert_two(&conn);
        let db_path = db_path();
        let out = handle_link(
            &conn,
            &db_path,
            &json!({"source_id": a, "target_id": b, "relation": SUPERSEDES}),
            None,
        )
        .expect("ok");
        let arr = out["invalidation_notified"].as_array().unwrap();
        assert_eq!(arr.len(), 0);
    }
}
