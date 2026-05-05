// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.6.3.1 (P4, audit G1) — SHIP-GATE Phase 1 functional scenarios.
//
// These tests **must touch the real gate** (`db::enforce_governance`),
// not just the resolver. The audit's complaint was that the gate
// itself bypassed inheritance — so the ship-gate proof has to drive
// the same code path the production store/delete/promote handlers do.
//
// Each scenario:
//   1. Seeds a parent (and optionally child) namespace with a real
//      policy via the same path operators use.
//   2. Calls `db::enforce_governance(...)` with a `GovernedAction`
//      that the policy should gate.
//   3. Asserts on the `GovernanceDecision` shape — Allow / Deny /
//      Pending(id) — and (where applicable) confirms the
//      pending_actions row was queued so the approver pipeline can
//      pick it up.
//
// CUTLINE-PROTECTED: even if every other v0.6.3.1 phase slips, this
// scenario file ships green. Failures here are release blockers.

use ai_memory::config::{
    PermissionsMode, lock_permissions_mode_for_test, override_active_permissions_mode_for_test,
};
use ai_memory::db;
use ai_memory::models::{
    ApproverType, GovernanceDecision, GovernanceLevel, GovernancePolicy, GovernedAction, Memory,
    Tier, default_metadata,
};
use rusqlite::Connection;

/// K3: the ship-gate matrix asserts the gate's *blocking* semantics
/// (Pending / Deny). v0.7.0 K3 made the gate consult
/// `permissions.mode`, with the v0.7.0 process default being
/// `advisory` (log + Allow). Pin the K1 cutline to Enforce so the
/// ship-gate scenarios still drive the blocking path. Returns the
/// process-wide gate-mode Mutex guard so concurrent scenarios in
/// this binary cannot flip the atomic mid-test.
fn pin_enforce_mode() -> std::sync::MutexGuard<'static, ()> {
    let guard = lock_permissions_mode_for_test();
    override_active_permissions_mode_for_test(PermissionsMode::Enforce);
    guard
}

fn seed_policy(conn: &Connection, namespace: &str, policy: GovernancePolicy, owner_agent_id: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(owner_agent_id.to_string()),
        );
        obj.insert(
            "governance".to_string(),
            serde_json::to_value(&policy).unwrap(),
        );
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{namespace}"),
        title: format!("standard for {namespace}"),
        content: "policy".to_string(),
        tags: vec![],
        priority: 9,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
    };
    let standard_id = db::insert(conn, &standard).unwrap();
    db::set_namespace_standard(conn, namespace, &standard_id, None).unwrap();
}

fn approve_write_policy() -> GovernancePolicy {
    GovernancePolicy {
        write: GovernanceLevel::Approve,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
    }
}

fn any_policy_no_inherit() -> GovernancePolicy {
    GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: false,
    }
}

// ---------------------------------------------------------------------------
// Ship-gate Phase 1 functional scenarios.
// ---------------------------------------------------------------------------

/// G1 #1 — depth-5 chain inherits Approve to the leaf, store is queued.
#[test]
fn shipgate_inherit_default_governance_chain_5_deep_requires_approval_at_leaf() {
    let _gate = pin_enforce_mode();
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone", approve_write_policy(), "alice");

    let leaf = "alphaone/secure/team-a/svc/agent-1";
    let payload = serde_json::json!({"title": "leak-test", "tier": "long"});
    let decision = db::enforce_governance(
        &conn,
        GovernedAction::Store,
        leaf,
        "bob",
        None,
        None,
        &payload,
    )
    .expect("enforce_governance must not error on a valid policy");

    match decision {
        GovernanceDecision::Pending(pid) => {
            assert!(!pid.is_empty(), "pending_id is queued by the gate");
            // Confirm the row landed in pending_actions so the approver
            // pipeline can pick it up.
            let listed = db::list_pending_actions(&conn, Some("pending"), 10).unwrap();
            assert!(
                listed.iter().any(|pa| pa.id == pid && pa.namespace == leaf),
                "pending_actions row must reflect the leaf namespace, not the parent"
            );
        }
        other => panic!("expected Pending(_) at the leaf, got {other:?}"),
    }
}

/// G1 #2 — child with inherit=false stops the walk; child's permissive
/// policy applies and the parent's Approve does NOT bubble through.
#[test]
fn shipgate_inherit_false_at_child_blocks_parent_policy() {
    let _gate = pin_enforce_mode();
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone/secure", approve_write_policy(), "alice");
    seed_policy(
        &conn,
        "alphaone/secure/team-a",
        any_policy_no_inherit(),
        "alice",
    );

    let payload = serde_json::json!({"title": "override-allowed"});
    let decision = db::enforce_governance(
        &conn,
        GovernedAction::Store,
        "alphaone/secure/team-a",
        "bob",
        None,
        None,
        &payload,
    )
    .expect("policy resolves cleanly");

    assert!(
        matches!(decision, GovernanceDecision::Allow),
        "child's inherit=false Any policy must allow, parent's Approve is bypassed: got {decision:?}"
    );
}

/// G1 #3 — both levels set, the most-specific (child) policy wins.
#[test]
fn shipgate_most_specific_policy_wins_when_both_set() {
    let _gate = pin_enforce_mode();
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone/secure", approve_write_policy(), "alice");
    seed_policy(
        &conn,
        "alphaone/secure/team-a",
        GovernancePolicy::default(), // write=Any, inherit=true
        "alice",
    );

    let payload = serde_json::json!({"title": "child-allows"});
    let decision = db::enforce_governance(
        &conn,
        GovernedAction::Store,
        "alphaone/secure/team-a",
        "bob",
        None,
        None,
        &payload,
    )
    .expect("policy resolves cleanly");

    assert!(
        matches!(decision, GovernanceDecision::Allow),
        "child's Any beats parent's Approve, write must Allow"
    );
}

/// G1 #4 — child with NO policy at all inherits the parent's policy
/// through the gate (the original audit reproducer).
#[test]
fn shipgate_child_with_no_policy_inherits_parent_policy() {
    let _gate = pin_enforce_mode();
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone/secure", approve_write_policy(), "alice");
    // NB: alphaone/secure/team-a has no standard.

    let payload = serde_json::json!({"title": "leak-attempt"});
    let decision = db::enforce_governance(
        &conn,
        GovernedAction::Store,
        "alphaone/secure/team-a",
        "bob",
        None,
        None,
        &payload,
    )
    .expect("inherited policy must resolve");

    assert!(
        matches!(decision, GovernanceDecision::Pending(_)),
        "child with no policy MUST inherit parent's Approve and queue: got {decision:?}"
    );
}

/// G1 #5 — pre-fix v0.6.3 silently bypassed governance for any
/// child of a governed parent. This scenario pins that the new gate
/// closes the bypass: a leaf with no policy under a governed parent
/// cannot perform a Deny-eligible action without going through the
/// approver pipeline.
#[test]
fn shipgate_audit_no_silent_bypass_in_v063_compatibility_path() {
    let _gate = pin_enforce_mode();
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();

    // Replicate the audit scenario verbatim: alphaone/secure has an
    // Approve-write policy; the unprivileged subtree must NOT be able
    // to bypass it just by writing under a child namespace that
    // wasn't explicitly governed.
    seed_policy(&conn, "alphaone/secure", approve_write_policy(), "alice");

    for child in [
        "alphaone/secure/team-a",
        "alphaone/secure/team-a/agent",
        "alphaone/secure/team-b/svc/worker-1",
        "alphaone/secure/extra/extra/extra/extra/leaf", // depth=8 stress
    ] {
        let payload = serde_json::json!({"title": format!("leak-{child}")});
        let decision = db::enforce_governance(
            &conn,
            GovernedAction::Store,
            child,
            "mallory",
            None,
            None,
            &payload,
        )
        .unwrap_or_else(|e| panic!("gate errored on {child}: {e}"));
        assert!(
            matches!(decision, GovernanceDecision::Pending(_)),
            "v0.6.3 silent-bypass regression: {child} must inherit parent governance, got {decision:?}"
        );
    }

    // Sanity inverse: a sibling subtree with no governance at all
    // (and no governed ancestor) is still opt-in/Allow.
    let decision = db::enforce_governance(
        &conn,
        GovernedAction::Store,
        "betatwo/free",
        "mallory",
        None,
        None,
        &serde_json::json!({}),
    )
    .unwrap();
    assert!(
        matches!(decision, GovernanceDecision::Allow),
        "ungoverned subtrees remain opt-in (compatibility preserved)"
    );
}

/// Capabilities surface — operators must be able to read
/// `governance.inheritance = "enforced"` so deployment scripts can
/// verify the fix landed.
#[test]
fn shipgate_capabilities_reports_inheritance_enforced() {
    let caps = ai_memory::config::FeatureTier::Keyword
        .config()
        .capabilities();
    let v: serde_json::Value = serde_json::to_value(&caps).unwrap();
    assert_eq!(
        v["permissions"]["inheritance"], "enforced",
        "capabilities v2 must surface the fix posture"
    );
}
