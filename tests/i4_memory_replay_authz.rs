// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 #628 I4 (review blocker H6) — `memory_replay` authorisation.
//!
//! Before this fix `memory_replay` fetched and decompressed transcript
//! content with no permission check, leaking verbatim chat content
//! across tenant boundaries on a multi-agent daemon. The fix routes
//! the read through the K9 unified evaluator
//! ([`ai_memory::permissions::Permissions::evaluate`]) using the new
//! [`ai_memory::permissions::Op::MemoryReplay`] variant; a Deny
//! decision short-circuits before the BLOB ever leaves SQLite.
//!
//! Scenario covered:
//!
//! * Agent A stores a transcript in namespace `tenant-a/`. Agent B
//!   issues `memory_replay` against the memory linked to it. The K9
//!   ruleset denies cross-tenant reads; the test asserts B receives
//!   an error AND no transcript content reaches B's response payload.

use ai_memory::db;
use ai_memory::mcp;
use ai_memory::permissions::{
    self, PermissionRule, RuleDecision, clear_active_permission_rules_for_test,
    set_active_permission_rules,
};
use ai_memory::transcripts;
use rusqlite::params;
use serde_json::json;
use std::sync::Mutex;

/// Process-wide gate so the rules registry mutations don't race
/// against other integration tests that also seed `[[permissions.rules]]`.
/// Mirrors the pattern in `tests/identity_e2e.rs`.
static RULES_GUARD: Mutex<()> = Mutex::new(());

/// Insert a stub `memories` row in the given namespace so the I2 join
/// can be satisfied without dragging in the full store pipeline.
fn insert_memory(conn: &rusqlite::Connection, id: &str, namespace: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories (
            id, tier, namespace, title, content, created_at, updated_at
         ) VALUES (?1, 'short_term', ?2, ?3, 'body', ?4, ?4)",
        params![id, namespace, format!("title-{id}"), now],
    )
    .unwrap();
}

#[test]
fn agent_b_cannot_replay_agent_a_transcript_when_rule_denies() {
    let _g = RULES_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = db::open(std::path::Path::new(":memory:")).unwrap();

    // Agent A's tenant namespace + a memory + a sensitive transcript.
    insert_memory(&conn, "mem-a", "tenant-a/notes");
    let secret = "[user] Agent A's confidential strategy doc — do not leak.";
    let t = transcripts::store(&conn, "tenant-a/notes", secret, None).unwrap();
    transcripts::link_transcript(&conn, "mem-a", &t.id, None, None).unwrap();

    // K9 rule: deny any agent NOT named `agent-a` from replaying
    // `tenant-a/**`. The agent_pattern `*` matches everything; pair
    // with namespace_pattern `tenant-a/**` so only this namespace's
    // reads are gated.
    set_active_permission_rules(vec![PermissionRule {
        namespace_pattern: "tenant-a/**".to_string(),
        op: "memory_replay".to_string(),
        agent_pattern: "agent-b".to_string(),
        decision: RuleDecision::Deny,
        reason: Some("agent-b cannot read tenant-a transcripts".to_string()),
    }]);

    // Sanity: the new Op variant round-trips through the wire string.
    assert_eq!(
        permissions::Op::from_str("memory_replay"),
        Some(permissions::Op::MemoryReplay),
        "new MemoryReplay variant must be wired into the wire matcher"
    );

    // Agent B issues the replay. The handler is `pub` for this test;
    // we pass `agent_id=agent-b` directly so the resolver doesn't
    // fall through to the host-process default (which would not
    // match the rule).
    let result = mcp::handle_replay(
        &conn,
        &json!({
            "memory_id": "mem-a",
            "agent_id": "agent-b",
        }),
        None,
    );
    let err = result.expect_err("denied tenant must produce an error");
    assert!(
        err.contains("denied") || err.contains("permission"),
        "error must mention denial / permission, got: {err}"
    );
    assert!(
        !err.contains(secret),
        "the secret transcript content must not leak into the error message: {err}"
    );

    clear_active_permission_rules_for_test();
}

#[test]
fn agent_a_can_still_replay_own_namespace_with_same_rule_loaded() {
    let _g = RULES_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    insert_memory(&conn, "mem-a", "tenant-a/notes");
    let body = "[user] Agent A's own conversation.";
    let t = transcripts::store(&conn, "tenant-a/notes", body, None).unwrap();
    transcripts::link_transcript(&conn, "mem-a", &t.id, None, None).unwrap();

    // Same rule shape as above: only `agent-b` is denied. `agent-a`
    // (the owner) must not be incidentally locked out by the gate.
    set_active_permission_rules(vec![PermissionRule {
        namespace_pattern: "tenant-a/**".to_string(),
        op: "memory_replay".to_string(),
        agent_pattern: "agent-b".to_string(),
        decision: RuleDecision::Deny,
        reason: Some("agent-b cannot read tenant-a transcripts".to_string()),
    }]);

    let payload = mcp::handle_replay(
        &conn,
        &json!({
            "memory_id": "mem-a",
            "agent_id": "agent-a",
        }),
        None,
    )
    .expect("agent-a must be allowed");
    assert_eq!(payload["count"], 1);
    let transcripts_arr = payload["transcripts"].as_array().unwrap();
    assert_eq!(transcripts_arr.len(), 1);
    assert_eq!(transcripts_arr[0]["content"], body);

    clear_active_permission_rules_for_test();
}
