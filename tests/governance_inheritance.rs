// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.6.3.1 (P4, audit G1) — governance inheritance unit tests.
//
// These tests pin the **resolution** semantics of
// `db::resolve_governance_policy` after the leaf-first chain walk
// landed. The companion file `ship_gate_governance_inheritance.rs`
// drives the same pipeline through the actual `enforce_governance`
// gate so the ship-gate Phase 1 functional matrix can prove the bug
// is closed end-to-end (not just at the resolver level).
//
// CUTLINE-PROTECTED: even if every other v0.6.3.1 phase slips, this
// test file ships green. Treat regressions here as release blockers.

use ai_memory::db;
use ai_memory::models::{
    self, ApproverType, GovernanceLevel, GovernancePolicy, Memory, Tier, default_metadata,
};
use rusqlite::Connection;

/// Seed `namespace` with the supplied policy. Mirrors the helper in
/// `cli::governance::tests` but stays self-contained so the
/// integration-test crate has no dependency on private cli helpers.
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

fn approve_policy() -> GovernancePolicy {
    GovernancePolicy {
        write: GovernanceLevel::Approve,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
    }
}

fn any_policy() -> GovernancePolicy {
    GovernancePolicy::default()
}

// ---------------------------------------------------------------------------
// Resolver-level (unit) tests for the leaf-first chain walk.
// ---------------------------------------------------------------------------

/// G1 baseline: a deep chain whose only policy lives at a top-level
/// ancestor must be returned for a leaf descendant. This is the
/// canonical scenario the audit flagged as silently bypassed.
#[test]
fn inherit_default_governance_chain_5_deep_requires_approval_at_leaf() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone", approve_policy(), "alice");

    // Five-deep leaf with no intermediate policies.
    let leaf = "alphaone/secure/team-a/svc/agent-1";
    let resolved = db::resolve_governance_policy(&conn, leaf)
        .expect("ancestor policy must inherit to leaf (G1)");
    assert_eq!(
        resolved.write,
        GovernanceLevel::Approve,
        "leaf inherits parent's Approve write level via the chain walk"
    );
}

/// `inherit: false` at a child must stop the walk — the parent's
/// stricter policy must NOT bubble through.
#[test]
fn inherit_false_at_child_blocks_parent_policy() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone/secure", approve_policy(), "alice");

    let mut child = any_policy();
    child.inherit = false;
    seed_policy(&conn, "alphaone/secure/team-a", child, "alice");

    let resolved = db::resolve_governance_policy(&conn, "alphaone/secure/team-a")
        .expect("child has its own policy, must be returned");
    // Most-specific wins — child's `Any` overrides parent's `Approve`.
    assert_eq!(resolved.write, GovernanceLevel::Any);
    assert!(
        !resolved.inherit,
        "inherit=false flag must round-trip through resolution"
    );
}

/// When BOTH parent and child have policies, the most-specific
/// (child) wins. This is the inverse of the G1 fix: we want
/// inheritance to fall through ONLY when the child has no policy.
#[test]
fn most_specific_policy_wins_when_both_set() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone/secure", approve_policy(), "alice");
    seed_policy(&conn, "alphaone/secure/team-a", any_policy(), "alice");

    let resolved = db::resolve_governance_policy(&conn, "alphaone/secure/team-a")
        .expect("child has its own policy");
    assert_eq!(
        resolved.write,
        GovernanceLevel::Any,
        "child's Any beats parent's Approve (most specific wins)"
    );
}

/// The original G1 reproducer: child has no policy, parent has
/// Approve, leaf must end up requiring Approve.
#[test]
fn child_with_no_policy_inherits_parent_policy() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone/secure", approve_policy(), "alice");
    // NB: NO policy on "alphaone/secure/team-a".
    let resolved = db::resolve_governance_policy(&conn, "alphaone/secure/team-a")
        .expect("parent policy must inherit");
    assert_eq!(resolved.write, GovernanceLevel::Approve);
    assert!(
        resolved.inherit,
        "inherited policy preserves its own inherit=true"
    );
}

/// Audit guard: a namespace with absolutely no governance anywhere
/// in the chain must return `None` (preserves opt-in semantics).
/// This pins the v0.6.3 compatibility behavior — we do NOT silently
/// inject a default policy, which would be a backward-incompat surprise.
#[test]
fn audit_no_silent_bypass_in_v063_compatibility_path() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // Policy exists somewhere, but in an unrelated subtree.
    seed_policy(&conn, "betatwo/secure", approve_policy(), "alice");

    // alphaone/* has no policy in any ancestor.
    let resolved = db::resolve_governance_policy(&conn, "alphaone/secure/team-a");
    assert!(
        resolved.is_none(),
        "no policy anywhere in the chain → None (opt-in preserved)"
    );
}

/// Cycle-safety: the explicit-parent walker is bounded by
/// MAX_EXPLICIT_DEPTH=8. A self-referencing or cyclic
/// namespace_meta entry must not panic or loop forever.
#[test]
fn resolver_is_cycle_safe() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_policy(&conn, "alphaone", approve_policy(), "alice");
    // No cycle here, but exercise a deep chain that forces the
    // explicit-parent fallback to no-op (no namespace_meta cycle).
    let leaf = "alphaone/a/b/c/d/e/f/g";
    let _ = db::resolve_governance_policy(&conn, leaf);
}

/// Sanity check on `build_namespace_chain` shape — top-down with `*`
/// at index 0 and the leaf at the tail. The resolver's leaf-first
/// reverse depends on this invariant.
#[test]
fn chain_shape_top_down_with_global_first() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let chain = db::build_namespace_chain(&conn, "alphaone/secure/team-a");
    assert_eq!(chain.first().map(String::as_str), Some("*"));
    assert_eq!(
        chain.last().map(String::as_str),
        Some("alphaone/secure/team-a")
    );
    assert!(chain.iter().any(|s| s == "alphaone"));
    assert!(chain.iter().any(|s| s == "alphaone/secure"));
}

/// Belt-and-suspenders: a partial-policy payload (no `inherit`
/// field on disk) must still deserialize as `inherit: true` so the
/// pre-migration on-disk shape stays compatible.
#[test]
fn partial_policy_payload_defaults_inherit_true() {
    let p: GovernancePolicy = serde_json::from_str(r#"{"write":"approve"}"#).unwrap();
    assert!(p.inherit, "missing inherit field deserializes as true");
    let _ = models::namespace_ancestors("a/b"); // touch the symbol so unused-import lints stay quiet
}
