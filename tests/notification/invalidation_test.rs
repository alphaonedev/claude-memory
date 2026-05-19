// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]
//! v0.7.0 L2-3 (issue #668) — Reflection invalidation propagation
//! acceptance tests.
//!
//! Issue #668 scope (verbatim from the spec):
//!
//! 1. `R1 sources M1, M2, M3` — meaning M1/M2/M3 each carry a
//!    `reflects_on` edge pointing at the reflection `R1`.
//! 2. `R2 supersedes = R1` — a new reflection `R2` lands a
//!    `supersedes` edge against `R1`, which is itself a reflection.
//! 3. Verify invalidation notifications are written for every
//!    memory that `reflects_on R1`.
//! 4. Verify the new MCP tool `memory_dependents_of_invalidated`
//!    returns the correct dependent set.
//!
//! All three are exercised below as end-to-end tests through the
//! MCP `handle_link` and `handle_dependents_of_invalidated` entry
//! points.

use ai_memory::db;
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, MemoryKind, Tier};
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

/// Build an in-memory SQLite connection with the full v0.7.0 schema
/// applied. We open via `db::open` so every migration runs (the
/// walker reads `memory_kind` from `memories`, which only exists at
/// schema v30+).
fn fresh_conn_and_path() -> (Connection, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("ai-memory.db");
    let conn = db::open(&path).expect("db::open");
    (conn, dir)
}

fn make_mem(title: &str, namespace: &str, kind: MemoryKind) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
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
        metadata: json!({"agent_id": "ai:tester"}),
        reflection_depth: i32::from(matches!(kind, MemoryKind::Reflection)),
        memory_kind: kind,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

/// `R1 sources M1, M2, M3` + `R2 supersedes R1` → expect three
/// invalidation notifications and the `memory_dependents_of_invalidated`
/// MCP tool to return the three dependents.
///
/// This is the canonical acceptance test from issue #668. The
/// MCP-side calls use the library's public `handle_*` re-exports.
#[test]
fn r1_sourced_by_m1_m2_m3_r2_supersedes_r1_notifies_three_dependents() {
    let (conn, _dir) = fresh_conn_and_path();
    let db_path = Path::new(":memory:");

    // Reflections R1 and R2 in namespace "task".
    let r1 = make_mem("R1-reflection", "task", MemoryKind::Reflection);
    let r2 = make_mem("R2-reflection", "task", MemoryKind::Reflection);
    let m1 = make_mem("M1", "task", MemoryKind::Observation);
    let m2 = make_mem("M2", "task", MemoryKind::Observation);
    let m3 = make_mem("M3", "task", MemoryKind::Observation);

    let r1_id = db::insert(&conn, &r1).expect("insert r1");
    let r2_id = db::insert(&conn, &r2).expect("insert r2");
    let m1_id = db::insert(&conn, &m1).expect("insert m1");
    let m2_id = db::insert(&conn, &m2).expect("insert m2");
    let m3_id = db::insert(&conn, &m3).expect("insert m3");

    // M1/M2/M3 each reflect on R1.
    db::create_link(&conn, &m1_id, &r1_id, "reflects_on").expect("m1→r1");
    db::create_link(&conn, &m2_id, &r1_id, "reflects_on").expect("m2→r1");
    db::create_link(&conn, &m3_id, &r1_id, "reflects_on").expect("m3→r1");

    // Sanity: before R2 supersedes R1, no _invalidations exist.
    let pre: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace LIKE '%_invalidations'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pre, 0);

    // R2 supersedes R1 via the MCP entry point — this is the wire
    // path the operator and curator both use, so the test pins the
    // contract end-to-end.
    let resp = ai_memory::mcp::dispatch_handle_link_for_test(
        &conn,
        db_path,
        &json!({
            "source_id": r2_id,
            "target_id": r1_id,
            "relation": "supersedes",
            "agent_id": "ai:tester",
        }),
        None,
    )
    .expect("handle_link supersedes");

    // Wire response surfaces the dependents that were notified.
    assert_eq!(resp["linked"].as_bool(), Some(true));
    let notified = resp["invalidation_notified"]
        .as_array()
        .expect("invalidation_notified is an array");
    assert_eq!(
        notified.len(),
        3,
        "expected three dependents notified, got {notified:?}"
    );
    let notified_ids: Vec<&str> = notified.iter().filter_map(|v| v.as_str()).collect();
    for id in [&m1_id, &m2_id, &m3_id] {
        assert!(
            notified_ids.contains(&id.as_str()),
            "dependent {id} missing from {notified_ids:?}"
        );
    }

    // Notification memories landed in `task/_invalidations`. M1/M2/M3
    // all share the same namespace so all three land in the same
    // hierarchical bucket.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
            rusqlite::params!["task/_invalidations"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 3,
        "expected 3 notification memories in task/_invalidations"
    );

    // Each notification carries the four-tuple `{dependent_id,
    // invalidated_id, invalidating_id, timestamp}`.
    let mut stmt = conn
        .prepare("SELECT metadata FROM memories WHERE namespace = 'task/_invalidations'")
        .unwrap();
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 3);
    for meta_str in &rows {
        let meta: serde_json::Value = serde_json::from_str(meta_str).unwrap();
        assert_eq!(meta["invalidated_id"].as_str(), Some(r1_id.as_str()));
        assert_eq!(meta["invalidating_id"].as_str(), Some(r2_id.as_str()));
        assert_eq!(
            meta["notification_kind"].as_str(),
            Some("reflection_invalidation")
        );
        assert!(meta["timestamp"].is_string());
    }

    // `signed_events` carries one row per notification.
    let signed_cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signed_events WHERE event_type = 'reflection.invalidation_notified'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    assert_eq!(signed_cnt, 3);

    // Acceptance gate 4: `memory_dependents_of_invalidated` returns
    // the correct dependent list — used by curator / operator
    // tooling to drive the review queue.
    let dependents_resp =
        ai_memory::mcp::dispatch_handle_dependents_for_test(&conn, &json!({"memory_id": r1_id}))
            .expect("dependents handler");
    assert_eq!(dependents_resp["count"].as_u64(), Some(3));
    let deps = dependents_resp["dependents"].as_array().unwrap();
    let dep_ids: Vec<&str> = deps.iter().filter_map(|d| d["id"].as_str()).collect();
    for id in [&m1_id, &m2_id, &m3_id] {
        assert!(
            dep_ids.contains(&id.as_str()),
            "dependent {id} missing from MCP tool response {dep_ids:?}"
        );
    }
}

/// Cross-namespace fan-out: when dependents live in DIFFERENT
/// namespaces, each notification lands under the dependent's OWN
/// `/_invalidations` sub-namespace — the parent scope owns the
/// notification, not the reflection that was invalidated.
#[test]
fn cross_namespace_dependents_land_in_their_own_invalidations_subns() {
    let (conn, _dir) = fresh_conn_and_path();
    let db_path = Path::new(":memory:");

    let r1 = make_mem("R1", "shared", MemoryKind::Reflection);
    let r2 = make_mem("R2", "shared", MemoryKind::Reflection);
    let m_a = make_mem("M-a", "team/alpha", MemoryKind::Observation);
    let m_b = make_mem("M-b", "team/beta", MemoryKind::Observation);
    let r1_id = db::insert(&conn, &r1).expect("ins r1");
    let r2_id = db::insert(&conn, &r2).expect("ins r2");
    let a_id = db::insert(&conn, &m_a).expect("ins m_a");
    let b_id = db::insert(&conn, &m_b).expect("ins m_b");
    db::create_link(&conn, &a_id, &r1_id, "reflects_on").unwrap();
    db::create_link(&conn, &b_id, &r1_id, "reflects_on").unwrap();

    let _ = ai_memory::mcp::dispatch_handle_link_for_test(
        &conn,
        db_path,
        &json!({
            "source_id": r2_id,
            "target_id": r1_id,
            "relation": "supersedes",
        }),
        None,
    )
    .expect("link");

    let count_alpha: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
            rusqlite::params!["team/alpha/_invalidations"],
            |r| r.get(0),
        )
        .unwrap();
    let count_beta: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
            rusqlite::params!["team/beta/_invalidations"],
            |r| r.get(0),
        )
        .unwrap();
    let count_shared: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
            rusqlite::params!["shared/_invalidations"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_alpha, 1);
    assert_eq!(count_beta, 1);
    assert_eq!(
        count_shared, 0,
        "reflection's own namespace should NOT get a notification"
    );
}

/// Supersedes between Observation rows MUST NOT trigger the walker
/// — invalidation propagation is strictly a Reflection→Reflection
/// concern. This pins the negative case so an over-eager refactor
/// doesn't broaden the trigger.
#[test]
fn observation_to_observation_supersedes_does_not_propagate() {
    let (conn, _dir) = fresh_conn_and_path();
    let db_path = Path::new(":memory:");

    let m_winner = make_mem("winner", "ns", MemoryKind::Observation);
    let m_loser = make_mem("loser", "ns", MemoryKind::Observation);
    let m_dep = make_mem("dep", "ns", MemoryKind::Observation);
    let winner_id = db::insert(&conn, &m_winner).unwrap();
    let loser_id = db::insert(&conn, &m_loser).unwrap();
    let dep_id = db::insert(&conn, &m_dep).unwrap();
    // The dependent has a reflects_on edge to the loser (unusual but
    // possible substrate-wise — the walker should still NOT fire
    // because the loser is not a reflection).
    db::create_link(&conn, &dep_id, &loser_id, "reflects_on").unwrap();

    let resp = ai_memory::mcp::dispatch_handle_link_for_test(
        &conn,
        db_path,
        &json!({
            "source_id": winner_id,
            "target_id": loser_id,
            "relation": "supersedes",
        }),
        None,
    )
    .expect("link");
    let notified = resp["invalidation_notified"].as_array().unwrap();
    assert!(
        notified.is_empty(),
        "observation-on-observation supersedes must not trigger walker"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace LIKE '%_invalidations'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

/// `related_to` between two reflections must NOT trigger the
/// walker — only `supersedes` qualifies as an invalidation event.
#[test]
fn related_to_between_reflections_does_not_propagate() {
    let (conn, _dir) = fresh_conn_and_path();
    let db_path = Path::new(":memory:");
    let r1 = make_mem("R1", "ns", MemoryKind::Reflection);
    let r2 = make_mem("R2", "ns", MemoryKind::Reflection);
    let m_dep = make_mem("dep", "ns", MemoryKind::Observation);
    let r1_id = db::insert(&conn, &r1).unwrap();
    let r2_id = db::insert(&conn, &r2).unwrap();
    let dep_id = db::insert(&conn, &m_dep).unwrap();
    db::create_link(&conn, &dep_id, &r1_id, "reflects_on").unwrap();

    let resp = ai_memory::mcp::dispatch_handle_link_for_test(
        &conn,
        db_path,
        &json!({
            "source_id": r2_id,
            "target_id": r1_id,
            "relation": "related_to",
        }),
        None,
    )
    .expect("link");
    let notified = resp["invalidation_notified"].as_array().unwrap();
    assert!(notified.is_empty());
}

/// MCP read tool returns empty for unknown ids and for reflections
/// with no inbound `reflects_on` edges. Pins the well-formed-zero
/// envelope shape so callers don't have to special-case missing
/// rows.
#[test]
fn dependents_of_invalidated_handles_unknown_and_zero_dependent_cases() {
    let (conn, _dir) = fresh_conn_and_path();
    let r1 = make_mem("R1-alone", "ns", MemoryKind::Reflection);
    let r1_id = db::insert(&conn, &r1).unwrap();

    let zero =
        ai_memory::mcp::dispatch_handle_dependents_for_test(&conn, &json!({"memory_id": r1_id}))
            .expect("zero deps");
    assert_eq!(zero["count"].as_u64(), Some(0));
    assert!(zero["dependents"].as_array().unwrap().is_empty());

    let unknown = ai_memory::mcp::dispatch_handle_dependents_for_test(
        &conn,
        &json!({"memory_id": "definitely-not-an-id"}),
    )
    .expect("unknown id returns empty envelope");
    assert_eq!(unknown["count"].as_u64(), Some(0));
}
