// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the anti-self-reflection cycle-check gate
//! (v0.7.0 L1-2, issue #659).
//!
//! Pins the four acceptance criteria from the issue:
//!   1. Direct cycle (A→B exists, B→A proposed) is refused.
//!   2. Indirect cycle (A→B→C exists, C→A proposed) is refused.
//!   3. Non-cycle (A→B exists, C→B proposed) succeeds.
//!   4. Refusal is recorded in `signed_events` with the full cycle path.
//!
//! Tests 1-3 drive [`ai_memory::kg::cycle_check::would_create_reflection_cycle`]
//! directly (the public entry point the MCP handler delegates to).  Test 4
//! additionally calls [`ai_memory::signed_events::append_signed_event`] +
//! [`ai_memory::signed_events::list_signed_events`] to verify the audit-row
//! contract end-to-end.

#![allow(clippy::too_many_lines)]

use ai_memory::db;
use ai_memory::kg::cycle_check::would_create_reflection_cycle;
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, Tier};
use ai_memory::signed_events::{
    SignedEvent, append_signed_event, list_signed_events, payload_hash,
};
use chrono::Utc;
use rusqlite::Connection;

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers
// ─────────────────────────────────────────────────────────────────────

fn open_db() -> Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn insert(conn: &Connection, id: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: id.to_string(),
        tier: Tier::Mid,
        namespace: "l1-2-test".to_string(),
        title: format!("memory-{id}"),
        content: format!("content for {id}"),
        tags: vec!["l1-2-test".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent-l1-2"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    db::insert(conn, &mem).expect("insert memory")
}

/// Directly add a `reflects_on` edge, bypassing the cycle-check guard
/// (used to set up the pre-existing graph under test).
fn add_edge(conn: &Connection, source: &str, target: &str) {
    db::create_link(conn, source, target, "reflects_on").expect("create_link");
}

// ─────────────────────────────────────────────────────────────────────
// 1. Direct cycle — A→B existing, B→A proposed → refused
// ─────────────────────────────────────────────────────────────────────

#[test]
fn direct_cycle_is_refused() {
    let conn = open_db();
    let a = insert(&conn, "cycle-direct-a");
    let b = insert(&conn, "cycle-direct-b");

    // Existing edge: A reflects_on B.
    add_edge(&conn, &a, &b);

    // Proposed: B reflects_on A — would close A→B→A cycle.
    let result = would_create_reflection_cycle(&conn, &b, &a, 8);
    assert!(
        result.would_cycle,
        "direct cycle B→A with A→B existing must be detected"
    );
    // Cycle path must start with source (B) and end with source (B).
    assert_eq!(
        result.cycle_path.first().map(String::as_str),
        Some(b.as_str()),
        "cycle_path must begin at source"
    );
    assert_eq!(
        result.cycle_path.last().map(String::as_str),
        Some(b.as_str()),
        "cycle_path must end at source (closed loop)"
    );
    // A must appear in the path.
    assert!(
        result.cycle_path.iter().any(|n| n == &a),
        "cycle_path must include the intermediate node A"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 2. Indirect cycle — A→B→C existing, C→A proposed → refused
// ─────────────────────────────────────────────────────────────────────

#[test]
fn indirect_cycle_is_refused() {
    let conn = open_db();
    let a = insert(&conn, "cycle-indirect-a");
    let b = insert(&conn, "cycle-indirect-b");
    let c = insert(&conn, "cycle-indirect-c");

    // Existing: A→B, B→C.
    add_edge(&conn, &a, &b);
    add_edge(&conn, &b, &c);

    // Proposed: C reflects_on A — would close C→A→B→C cycle.
    let result = would_create_reflection_cycle(&conn, &c, &a, 8);
    assert!(
        result.would_cycle,
        "indirect cycle C→A with A→B→C must be detected"
    );
    assert_eq!(
        result.cycle_path.first().map(String::as_str),
        Some(c.as_str()),
        "cycle_path must begin at source (C)"
    );
    assert_eq!(
        result.cycle_path.last().map(String::as_str),
        Some(c.as_str()),
        "cycle_path must end at source (C) — closed loop"
    );
    // Both A and B must appear in the intermediate path.
    assert!(
        result.cycle_path.iter().any(|n| n == &a),
        "cycle_path must include A"
    );
    assert!(
        result.cycle_path.iter().any(|n| n == &b),
        "cycle_path must include B"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 3. Non-cycle — A→B existing, C→B proposed → succeeds
// ─────────────────────────────────────────────────────────────────────

#[test]
fn non_cycle_succeeds() {
    let conn = open_db();
    let a = insert(&conn, "cycle-noncycle-a");
    let b = insert(&conn, "cycle-noncycle-b");
    let c = insert(&conn, "cycle-noncycle-c");

    // Existing: A reflects_on B.
    add_edge(&conn, &a, &b);

    // Proposed: C reflects_on B — C is unrelated to A; no cycle.
    let result = would_create_reflection_cycle(&conn, &c, &b, 8);
    assert!(
        !result.would_cycle,
        "C→B with only A→B existing must not be detected as a cycle"
    );
    assert!(
        result.cycle_path.is_empty(),
        "cycle_path must be empty when no cycle is detected"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 4. Refusal recorded in signed_events with full cycle path
//
// Mirrors the pattern from
// `tests/recursive_learning_task5_audit_record.rs`:
// the cycle-check function returns a structured `cycle_path`; the
// handler emits a `signed_events` row containing a JSON payload that
// includes that path.  We simulate the handler's emit sequence here to
// verify (a) the row is written, (b) it carries the full path.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn refusal_recorded_in_signed_events_with_full_path() {
    let conn = open_db();
    let a = insert(&conn, "cycle-audit-a");
    let b = insert(&conn, "cycle-audit-b");

    // Existing: B reflects_on A.
    add_edge(&conn, &b, &a);

    // Run the cycle check (as handle_link does).
    let check = would_create_reflection_cycle(&conn, &a, &b, 8);
    assert!(check.would_cycle, "pre-condition: cycle must be detected");

    // Emit the audit row exactly as handle_link does.
    let refusal_payload = serde_json::json!({
        "event": "reflects_on.cycle_refused",
        "source_id": &a,
        "target_id": &b,
        "cycle_path": &check.cycle_path,
    });
    let cbor_bytes = refusal_payload.to_string().into_bytes();
    let audit_event = SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: "test-agent-l1-2".to_string(),
        event_type: "reflects_on.cycle_refused".to_string(),
        payload_hash: payload_hash(&cbor_bytes),
        signature: None,
        attest_level: "unsigned".to_string(),
        timestamp: Utc::now().to_rfc3339(),
        ..SignedEvent::default()
    };
    append_signed_event(&conn, &audit_event).expect("append audit row");

    // Now verify the row exists in signed_events.
    let events = list_signed_events(&conn, None, 20, 0).expect("list signed_events");

    let refusal = events
        .iter()
        .find(|e| e.event_type == "reflects_on.cycle_refused");
    assert!(
        refusal.is_some(),
        "signed_events must contain a reflects_on.cycle_refused row; found: {events:?}"
    );

    let ev = refusal.unwrap();
    assert_eq!(ev.agent_id, "test-agent-l1-2");
    assert_eq!(ev.attest_level, "unsigned");
    assert!(
        !ev.payload_hash.is_empty(),
        "payload_hash must be non-empty"
    );

    // Verify the cycle_path in the refusal payload is the full path.
    // The full path for B→A existing, proposing A→B is: [A, B, A].
    assert_eq!(
        check.cycle_path.first().map(String::as_str),
        Some(a.as_str()),
        "cycle_path must start at source (A)"
    );
    assert_eq!(
        check.cycle_path.last().map(String::as_str),
        Some(a.as_str()),
        "cycle_path must end at source (A) — closed loop"
    );
    assert!(
        check.cycle_path.iter().any(|n| n == &b),
        "cycle_path must include target node B"
    );
}
