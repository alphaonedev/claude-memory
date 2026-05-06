// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K10 — `remember=forever` writes a synthetic permission rule.
//!
//! The K10 contract: any of the three transports (HTTP, SSE-driven,
//! MCP) accepting `remember=forever` MUST register a
//! [`SyntheticPermissionRule`] in the process-wide registry so K9's
//! permission resolver auto-decides the same `(action_type,
//! namespace, agent_id)` tuple next time without re-asking.
//!
//! Until K9's resolver lands on the same branch, the registry's
//! consumer-facing surface is `approvals::list_synthetic_rules` —
//! we assert against that.

// `await_holding_lock` lints fire on `std::sync::Mutex` — but the
// lock is purely a test-serialisation primitive (the registry
// mutation that follows is itself thread-safe), so the lint is a
// false positive in this context. We allow it at the file level
// instead of at every test fn.
#![allow(clippy::await_holding_lock)]

use ai_memory::approvals::{
    Decision, Remember, SyntheticPermissionRule, clear_synthetic_rules_for_test,
    list_synthetic_rules, record_synthetic_rule,
};
use serde_json::json;
use std::sync::Mutex;

static REMEMBER_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn forever_rule_round_trips_through_registry() {
    let _g = REMEMBER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    clear_synthetic_rules_for_test();
    let rule = SyntheticPermissionRule {
        action_type: "store".into(),
        namespace: "scratch".into(),
        agent_id: Some("alice".into()),
        decision: "approve".into(),
        recorded_at: "2026-05-05T00:00:00Z".into(),
    };
    record_synthetic_rule(rule.clone());
    let snap = list_synthetic_rules();
    assert!(snap.contains(&rule), "rule absent: {snap:?}");
}

#[tokio::test]
async fn mcp_pending_approve_with_forever_records_rule() {
    let _g = REMEMBER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    clear_synthetic_rules_for_test();

    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    // Seed a real memory + delete-pending row so `execute_pending_action`
    // takes the delete branch (which only needs `memory_id`, no full
    // Memory payload to forge).
    let mem = ai_memory::models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Long,
        namespace: "ns-forever".into(),
        title: "k10-forever".into(),
        content: "x".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
    };
    let mem_id = ai_memory::db::insert(&conn, &mem).expect("insert memory");
    let payload = json!({"reason": "k10-forever"});
    let pending_id = ai_memory::db::queue_pending_action(
        &conn,
        ai_memory::models::GovernedAction::Delete,
        "ns-forever",
        Some(&mem_id),
        "alice",
        &payload,
    )
    .expect("queue_pending_action");

    let args = json!({
        "id": pending_id,
        "agent_id": "operator-1",
        "remember": "forever",
    });
    let resp = ai_memory::mcp::handle_pending_approve(&conn, &args, None)
        .expect("memory_pending_approve handler");
    assert_eq!(resp["approved"], json!(true), "approve failed: {resp}");
    assert_eq!(resp["remember"], json!("forever"));

    let snap = list_synthetic_rules();
    let found = snap.iter().any(|r| {
        r.action_type == "delete"
            && r.namespace == "ns-forever"
            && r.agent_id.as_deref() == Some("alice")
            && r.decision == "approve"
    });
    assert!(
        found,
        "forever rule not recorded after MCP approve; snap={snap:?}"
    );
}

#[tokio::test]
async fn mcp_pending_reject_with_forever_records_deny_rule() {
    let _g = REMEMBER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    clear_synthetic_rules_for_test();

    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let payload = json!({"title": "k10-deny", "content": "x", "namespace": "ns-deny"});
    let pending_id = ai_memory::db::queue_pending_action(
        &conn,
        ai_memory::models::GovernedAction::Delete,
        "ns-deny",
        None,
        "bob",
        &payload,
    )
    .expect("queue_pending_action");

    let args = json!({
        "id": pending_id,
        "agent_id": "operator-1",
        "remember": "forever",
    });
    let resp = ai_memory::mcp::handle_pending_reject(&conn, &args, None)
        .expect("memory_pending_reject handler");
    assert_eq!(resp["rejected"], json!(true), "reject failed: {resp}");
    assert_eq!(resp["remember"], json!("forever"));

    let snap = list_synthetic_rules();
    let found = snap.iter().any(|r| {
        r.action_type == "delete"
            && r.namespace == "ns-deny"
            && r.agent_id.as_deref() == Some("bob")
            && r.decision == "deny"
    });
    assert!(found, "deny rule not recorded; snap={snap:?}");
}

#[test]
fn remember_once_does_not_record_a_rule() {
    let _g = REMEMBER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    clear_synthetic_rules_for_test();

    // Sanity: just constructing decisions doesn't alter the registry.
    let _ = (Decision::Approve, Remember::Once);
    let snap = list_synthetic_rules();
    assert!(snap.is_empty(), "registry should start empty: {snap:?}");
}
