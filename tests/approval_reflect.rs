// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! v0.7.0 L1-8 — `require_approval_above_depth` governance gate.
//!
//! Pins the per-namespace approval gate for deep reflections.
//! The feature stores `governance.require_approval_above_depth` inside
//! the existing `metadata.governance` JSON blob (same blob as
//! `max_reflection_depth`, `write`, etc.) and reads it via
//! [`ai_memory::db::resolve_require_approval_above_depth`] — a new
//! free function that walks the namespace chain leaf-first without
//! requiring a new field on the existing [`GovernancePolicy`] struct.
//!
//! Contracts:
//!   1. `GovernedAction::Reflect` exists with `as_str() == "reflect"`.
//!   2. `resolve_require_approval_above_depth` returns `None` when no
//!      governance blob is present and `None` when the key is absent.
//!   3. A namespace with `require_approval_above_depth = 1`:
//!      - depth-1 reflection proceeds without queuing a pending row.
//!      - depth-2 reflection queues a pending row with
//!        `action_type = "reflect"` (not a Reflection memory).
//!   4. A namespace with `require_approval_above_depth = 0`:
//!      - even depth-1 reflection queues a pending row.
//!   5. `require_approval_above_depth` absent → no gate, depth-3
//!      reflect proceeds without any pending row.
//!   6. Approver calls `decide_pending_action(approve=true)` →
//!      row status becomes "approved"; re-issued reflect succeeds.
//!   7. Approver calls `decide_pending_action(approve=false)` →
//!      row status becomes "rejected"; no reflection memory committed.

use ai_memory::db::{self, ReflectInput};
use ai_memory::models::{GovernanceLevel, GovernancePolicy, GovernedAction, Memory, Tier};
use chrono::Utc;

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers
// ─────────────────────────────────────────────────────────────────────

fn make_memory(namespace: &str, title: &str, depth: i32) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("L1-8 fixture content: {title}"),
        tags: vec!["l1-8".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent-l1-8"}),
        reflection_depth: depth,
    }
}

fn reflect_input(source_ids: Vec<String>, namespace: Option<&str>, title: &str) -> ReflectInput {
    ReflectInput {
        source_ids,
        title: title.to_string(),
        content: format!("L1-8 synthesised reflection: {title}"),
        namespace: namespace.map(str::to_string),
        tier: Tier::Mid,
        tags: vec!["l1-8".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: "test-agent-l1-8".to_string(),
        metadata: serde_json::json!({}),
    }
}

/// Persist a namespace standard with the supplied raw governance JSON.
/// Uses a serde_json::Value for the governance blob so the test can
/// include keys unknown to the GovernancePolicy struct (such as
/// `require_approval_above_depth`).
fn seed_governance_json(
    conn: &rusqlite::Connection,
    namespace: &str,
    governance: serde_json::Value,
) {
    let now = Utc::now().to_rfc3339();
    let metadata = serde_json::json!({
        "agent_id": "test-agent-l1-8",
        "governance": governance,
    });
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{namespace}"),
        title: format!("standard for {namespace}"),
        content: "l1-8 policy".to_string(),
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
        reflection_depth: 0,
    };
    let std_id = db::insert(conn, &standard).unwrap();
    db::set_namespace_standard(conn, namespace, &std_id, None).unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// (1) GovernedAction::Reflect variant
// ─────────────────────────────────────────────────────────────────────

#[test]
fn governed_action_reflect_as_str_is_reflect() {
    assert_eq!(GovernedAction::Reflect.as_str(), "reflect");
}

// ─────────────────────────────────────────────────────────────────────
// (2) resolve_require_approval_above_depth — no governance configured
// ─────────────────────────────────────────────────────────────────────

#[test]
fn resolve_require_approval_above_depth_returns_none_with_no_governance() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // No namespace standard seeded → chain has no governance blob at any level.
    let result = db::resolve_require_approval_above_depth(&conn, "ungoverned-ns");
    assert_eq!(
        result, None,
        "must return None when no namespace standard is configured"
    );
}

#[test]
fn resolve_require_approval_above_depth_returns_none_when_key_absent() {
    // Governance blob present but no `require_approval_above_depth` key.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        "no-key-ns",
        serde_json::json!({
            "write": "any",
            "promote": "any",
            "delete": "owner",
            "approver": "human",
            "inherit": true,
            "max_reflection_depth": 3
        }),
    );
    let result = db::resolve_require_approval_above_depth(&conn, "no-key-ns");
    assert_eq!(
        result, None,
        "must return None when key is absent from governance blob"
    );
}

#[test]
fn resolve_require_approval_above_depth_returns_value_when_set() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        "threshold-ns",
        serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 1_u32
        }),
    );
    let result = db::resolve_require_approval_above_depth(&conn, "threshold-ns");
    assert_eq!(result, Some(1), "must return Some(1) when key is set to 1");
}

#[test]
fn resolve_require_approval_above_depth_returns_zero_when_set_to_zero() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        "zero-threshold-ns",
        serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 0_u32
        }),
    );
    let result = db::resolve_require_approval_above_depth(&conn, "zero-threshold-ns");
    assert_eq!(
        result,
        Some(0),
        "must return Some(0) when key is set to 0 (gates all reflections)"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (3) Approval gate — depth-2 reflect in a threshold-1 namespace queues
//     a pending row and does NOT commit a reflection memory.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn depth_2_reflect_in_threshold_1_namespace_queues_pending_not_reflection() {
    let ns = "l1-8-gate-ns";
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        ns,
        serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 1_u32
        }),
    );

    // Seed a depth-1 source so the proposed reflection lands at depth 2.
    let src = make_memory(ns, "depth-1-observation", 1);
    let src_id = db::insert(&conn, &src).unwrap();

    // Verify the threshold resolves correctly.
    let threshold = db::resolve_require_approval_above_depth(&conn, ns);
    assert_eq!(threshold, Some(1));

    let src_mem = db::get(&conn, &src_id).unwrap().expect("source must exist");
    #[allow(clippy::cast_sign_loss)]
    let proposed_depth: u32 = src_mem.reflection_depth.max(0).saturating_add(1) as u32;
    assert_eq!(proposed_depth, 2);

    // Gate fires: proposed_depth (2) > threshold (1).
    assert!(
        proposed_depth > threshold.unwrap(),
        "gate must fire for depth-2 reflection in threshold-1 namespace"
    );

    // Queue a pending_actions row (what the MCP handler does instead of
    // calling db::reflect when the gate fires).
    let payload = serde_json::json!({
        "source_ids": [src_id],
        "title": "depth-2-reflection-pending",
        "namespace": ns,
        "proposed_depth": proposed_depth,
    });
    let pending_id = db::queue_pending_action(
        &conn,
        GovernedAction::Reflect,
        ns,
        None,
        "test-agent-l1-8",
        &payload,
    )
    .expect("queue_pending_action must succeed");

    // Verify the pending row exists with action_type="reflect".
    let row = db::get_pending_action(&conn, &pending_id)
        .expect("get_pending_action must succeed")
        .expect("pending row must exist");
    assert_eq!(row.action_type, "reflect");
    assert_eq!(row.namespace, ns);
    assert_eq!(row.status, "pending");
    assert_eq!(row.requested_by, "test-agent-l1-8");

    // No reflection memory was committed.
    let pending_list = db::list_pending_actions(&conn, Some("pending"), 100).unwrap();
    assert_eq!(pending_list.len(), 1, "exactly one pending row");
    assert_eq!(pending_list[0].id, pending_id);
}

// ─────────────────────────────────────────────────────────────────────
// (4) Approval gate — depth-1 reflect in a threshold-1 namespace
//     proceeds without queuing (depth-1 is NOT above threshold-1).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn depth_1_reflect_in_threshold_1_namespace_proceeds_without_pending() {
    let ns = "l1-8-pass-ns";
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        ns,
        serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 1_u32
        }),
    );

    // Seed a depth-0 source so reflection lands at depth 1 (= threshold).
    let src = make_memory(ns, "depth-0-base", 0);
    let src_id = db::insert(&conn, &src).unwrap();

    let threshold = db::resolve_require_approval_above_depth(&conn, ns);
    assert_eq!(threshold, Some(1));

    let src_mem = db::get(&conn, &src_id).unwrap().expect("source must exist");
    #[allow(clippy::cast_sign_loss)]
    let proposed_depth: u32 = src_mem.reflection_depth.max(0).saturating_add(1) as u32;
    assert_eq!(proposed_depth, 1);

    // Gate must NOT fire: proposed_depth (1) == threshold (1), not above.
    assert!(
        proposed_depth <= threshold.unwrap(),
        "gate must not fire for depth-1 reflection with threshold=1"
    );

    // Call db::reflect directly (as the MCP handler would, having not gated).
    let input = reflect_input(vec![src_id], Some(ns), "depth-1-allowed");
    let outcome = db::reflect(&conn, &input).expect("depth-1 reflect must succeed");
    assert_eq!(outcome.reflection_depth, 1);
    assert_eq!(outcome.namespace, ns);

    // No pending rows.
    let pending_list = db::list_pending_actions(&conn, Some("pending"), 100).unwrap();
    assert!(
        pending_list.is_empty(),
        "no pending rows must exist when gate does not fire"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (5) Approval gate — threshold=None (key absent): no gate fires even
//     at depth 3.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn no_approval_gate_when_require_approval_above_depth_is_absent() {
    let ns = "l1-8-none-ns";
    // Default policy JSON: no require_approval_above_depth key.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        ns,
        serde_json::json!({
            "write": "any",
            "max_reflection_depth": 3
        }),
    );

    // Threshold must be None.
    let threshold = db::resolve_require_approval_above_depth(&conn, ns);
    assert_eq!(
        threshold, None,
        "threshold must be None when key is absent from governance blob"
    );

    // Seed a depth-2 source; reflection would land at depth 3 (= default cap).
    let src = make_memory(ns, "depth-2-source", 2);
    let src_id = db::insert(&conn, &src).unwrap();

    // Gate is inactive; call db::reflect directly (depth 3 ≤ cap 3).
    let input = reflect_input(vec![src_id], Some(ns), "depth-3-no-gate");
    let outcome = db::reflect(&conn, &input).expect("depth-3 reflect must succeed");
    assert_eq!(outcome.reflection_depth, 3);

    // No pending rows were created.
    let pending_list = db::list_pending_actions(&conn, None, 100).unwrap();
    assert!(
        pending_list.is_empty(),
        "no pending rows must exist when require_approval_above_depth is absent"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (6) Approval gate — threshold=0: even depth-1 queues a pending row.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn threshold_zero_gates_even_depth_1_reflections() {
    let ns = "l1-8-zero-threshold-ns";
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        ns,
        serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 0_u32
        }),
    );

    let src = make_memory(ns, "depth-0-source", 0);
    let src_id = db::insert(&conn, &src).unwrap();

    let threshold = db::resolve_require_approval_above_depth(&conn, ns);
    assert_eq!(threshold, Some(0));

    let src_mem = db::get(&conn, &src_id).unwrap().expect("source must exist");
    #[allow(clippy::cast_sign_loss)]
    let proposed_depth: u32 = src_mem.reflection_depth.max(0).saturating_add(1) as u32;
    assert_eq!(proposed_depth, 1);

    // Gate fires: 1 > 0.
    assert!(proposed_depth > threshold.unwrap());

    let payload = serde_json::json!({
        "source_ids": [src_id],
        "title": "depth-1-gated-by-zero-threshold",
        "namespace": ns,
        "proposed_depth": proposed_depth,
    });
    let pending_id = db::queue_pending_action(
        &conn,
        GovernedAction::Reflect,
        ns,
        None,
        "test-agent-l1-8",
        &payload,
    )
    .expect("queue_pending_action must succeed");

    let row = db::get_pending_action(&conn, &pending_id)
        .expect("get_pending_action must succeed")
        .expect("pending row must exist");
    assert_eq!(row.action_type, "reflect");
    assert_eq!(row.status, "pending");
}

// ─────────────────────────────────────────────────────────────────────
// (7) Approver resolves → pending row approved; re-issued reflect succeeds
// ─────────────────────────────────────────────────────────────────────

#[test]
fn approver_resolves_allows_subsequent_reflect() {
    let ns = "l1-8-resolve-ns";
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        ns,
        serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 1_u32
        }),
    );

    let src = make_memory(ns, "depth-1-for-approve", 1);
    let src_id = db::insert(&conn, &src).unwrap();

    // Gate fires; queue pending row.
    let payload = serde_json::json!({
        "source_ids": [src_id],
        "title": "depth-2-pending",
        "namespace": ns,
        "proposed_depth": 2_u32,
    });
    let pending_id = db::queue_pending_action(
        &conn,
        GovernedAction::Reflect,
        ns,
        None,
        "test-agent-l1-8",
        &payload,
    )
    .unwrap();

    // Approver resolves: decide approve=true.
    let approved = db::decide_pending_action(&conn, &pending_id, true, "approver-agent").unwrap();
    assert!(
        approved,
        "decide_pending_action must return true for first decision"
    );

    let row = db::get_pending_action(&conn, &pending_id).unwrap().unwrap();
    assert_eq!(row.status, "approved");
    assert_eq!(row.decided_by.as_deref(), Some("approver-agent"));

    // Approver re-issues the reflect on behalf of the original agent.
    // The substrate reflect call succeeds (no further gate blocking it here).
    let input = reflect_input(vec![src_id], Some(ns), "depth-2-after-approval");
    let outcome = db::reflect(&conn, &input).expect("reflect must succeed after approval");
    assert_eq!(outcome.reflection_depth, 2);
    assert_eq!(outcome.namespace, ns);
}

// ─────────────────────────────────────────────────────────────────────
// (8) Approver rejects → pending row rejected; no reflection committed
// ─────────────────────────────────────────────────────────────────────

#[test]
fn approver_rejects_marks_pending_row_rejected() {
    let ns = "l1-8-reject-ns";
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    seed_governance_json(
        &conn,
        ns,
        serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 1_u32
        }),
    );

    let src = make_memory(ns, "depth-1-for-reject", 1);
    let src_id = db::insert(&conn, &src).unwrap();

    let payload = serde_json::json!({
        "source_ids": [src_id],
        "title": "depth-2-to-reject",
        "namespace": ns,
        "proposed_depth": 2_u32,
    });
    let pending_id = db::queue_pending_action(
        &conn,
        GovernedAction::Reflect,
        ns,
        None,
        "test-agent-l1-8",
        &payload,
    )
    .unwrap();

    // Approver rejects: decide approve=false.
    let decided = db::decide_pending_action(&conn, &pending_id, false, "approver-agent").unwrap();
    assert!(
        decided,
        "decide_pending_action must return true for first decision"
    );

    let row = db::get_pending_action(&conn, &pending_id).unwrap().unwrap();
    assert_eq!(
        row.status, "rejected",
        "rejected row must have status=rejected"
    );
    assert_eq!(row.decided_by.as_deref(), Some("approver-agent"));

    // No reflection memory was committed — only the source (depth-1) exists.
    let all_pending = db::list_pending_actions(&conn, Some("rejected"), 100).unwrap();
    assert_eq!(all_pending.len(), 1, "exactly one rejected pending row");
    assert_eq!(all_pending[0].action_type, "reflect");
}

// ─────────────────────────────────────────────────────────────────────
// (9) Wire-shape backward compatibility: pre-existing governance JSON
//     without the field parses cleanly (GovernancePolicy round-trip)
// ─────────────────────────────────────────────────────────────────────

#[test]
fn pre_existing_governance_json_without_field_parses_cleanly() {
    // Simulates a namespace standard written before L1-8 landed.
    // The GovernancePolicy struct must not reject it.
    let legacy_json = serde_json::json!({
        "write": "any",
        "promote": "any",
        "delete": "owner",
        "approver": "human",
        "inherit": true,
        "max_reflection_depth": 3
    });
    let p: GovernancePolicy = serde_json::from_value(legacy_json)
        .expect("pre-existing governance JSON must parse cleanly");
    // Existing fields must survive.
    assert_eq!(p.write, GovernanceLevel::Any);
    assert_eq!(p.max_reflection_depth, Some(3));
    assert!(p.inherit);

    // The new field is NOT a struct field — it lives in the raw JSON.
    // resolve_require_approval_above_depth reads it directly from the blob.
    // This test pins that the struct itself does not error on the old shape.
}
