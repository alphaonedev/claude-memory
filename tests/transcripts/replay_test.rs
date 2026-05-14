// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-4 (issue #669) — acceptance tests for the reflection-union
//! `memory_replay`.
//!
//! These tests drive the MCP handler at `mcp::handle_replay` end-to-end:
//! schema migrations applied via `db::open`, real `memory_transcripts`
//! storage via `transcripts::store`, real `memory_links` rows for the
//! `reflects_on` adjacency, and real `memory_transcript_links` rows for
//! the I2 provenance edges. The substrate unit tests in
//! `src/transcripts/replay.rs::tests` already pin the walker invariants
//! (BFS, depth cap, cycle safety, dedup, dangling drop); these
//! integration tests pin the wire shape the MCP handler emits so a
//! future refactor of the JSON envelope is caught by a failing
//! assertion rather than a silent regression.

#![allow(clippy::doc_markdown)]

use ai_memory::{db, mcp, transcripts};
use chrono::Utc;
use rusqlite::params;
use serde_json::{Value, json};

/// Insert a memory row with the given id, namespace, and
/// `memory_kind`. Minimal column set — the handler only reads
/// `memory_kind` (to dispatch reflection vs observation) plus the
/// link tables. `created_at` is "now" so the substrate CHECK
/// triggers accept it.
fn insert_memory(conn: &rusqlite::Connection, id: &str, namespace: &str, kind: &str) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories (
            id, tier, namespace, title, content, created_at, updated_at, memory_kind
         ) VALUES (?1, 'short', ?2, ?3, 'body', ?4, ?4, ?5)",
        params![id, namespace, format!("title-{id}"), now, kind],
    )
    .expect("insert test memory");
}

/// Write a `reflects_on` edge directly via SQL. The substrate-level
/// link helpers carry K9/H3 signature / cycle-guard machinery that the
/// replay reader is meant to be agnostic to; we want the test fixture
/// to seed the raw adjacency without invoking that pipeline.
fn link_reflects_on(conn: &rusqlite::Connection, source: &str, target: &str) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_links (source_id, target_id, relation, created_at, valid_from)
         VALUES (?1, ?2, 'reflects_on', ?3, ?3)",
        params![source, target, now],
    )
    .expect("insert reflects_on link");
}

/// Backdate a transcript's `created_at` so the chronological ordering
/// in the handler's output is anchored on a fixture-controlled value
/// rather than the wall-clock instant `transcripts::store` stamps.
fn backdate_transcript(conn: &rusqlite::Connection, id: &str, ts: &str) {
    conn.execute(
        "UPDATE memory_transcripts SET created_at = ?1 WHERE id = ?2",
        params![ts, id],
    )
    .expect("backdate transcript created_at");
}

/// Helper: open a fresh `:memory:` DB with the full production schema.
fn fresh_db() -> rusqlite::Connection {
    db::open(std::path::Path::new(":memory:")).expect("db::open in-memory")
}

/// L2-4 §Acceptance #1 — "Reflection 3 sources, each with transcript:
/// returns union of 4 transcripts."
#[test]
fn reflection_with_three_sources_returns_union_of_four_transcripts() {
    let conn = fresh_db();

    // Three observation memories, each with one backdated transcript.
    for (id, ts) in [
        ("obs-a", "2026-01-01T00:00:00Z"),
        ("obs-b", "2026-01-02T00:00:00Z"),
        ("obs-c", "2026-01-03T00:00:00Z"),
    ] {
        insert_memory(&conn, id, "team/eng", "observation");
        let t = transcripts::store(&conn, "team/eng", &format!("body-{id}"), None).unwrap();
        backdate_transcript(&conn, &t.id, ts);
        transcripts::link_transcript(&conn, id, &t.id, None, None).unwrap();
    }

    // Reflection memory with its own transcript and reflects_on edges
    // to each source.
    insert_memory(&conn, "ref-1", "team/eng", "reflection");
    let t_ref = transcripts::store(&conn, "team/eng", "reflection-body", None).unwrap();
    backdate_transcript(&conn, &t_ref.id, "2026-01-04T00:00:00Z");
    transcripts::link_transcript(&conn, "ref-1", &t_ref.id, None, None).unwrap();
    for src in ["obs-a", "obs-b", "obs-c"] {
        link_reflects_on(&conn, "ref-1", src);
    }

    let payload = mcp::handle_replay(&conn, &json!({"memory_id": "ref-1"}), None)
        .expect("replay against reflection succeeds");
    assert_eq!(payload["count"], 4, "self + 3 source transcripts");

    let arr = payload["transcripts"].as_array().unwrap();
    assert_eq!(arr.len(), 4);

    // Chronological ordering — the backdated created_at columns drive
    // a deterministic 1, 2, 3, 4 sequence.
    let timestamps: Vec<&str> = arr
        .iter()
        .map(|e| e["created_at"].as_str().unwrap())
        .collect();
    assert_eq!(
        timestamps,
        vec![
            "2026-01-01T00:00:00Z",
            "2026-01-02T00:00:00Z",
            "2026-01-03T00:00:00Z",
            "2026-01-04T00:00:00Z",
        ]
    );

    // Each entry surfaces its anchor memory id so callers can render
    // "which source contributed which transcript".
    let anchors: Vec<&str> = arr
        .iter()
        .map(|e| e["source_memory_id"].as_str().unwrap())
        .collect();
    assert!(anchors.contains(&"obs-a"));
    assert!(anchors.contains(&"obs-b"));
    assert!(anchors.contains(&"obs-c"));
    assert!(anchors.contains(&"ref-1"));
}

/// L2-4 §Acceptance #2 — "Reflection at depth 2: --depth=full returns
/// all transcripts in chain." Plus the depth-cap surface (`depth=0`
/// degenerates to the pre-L2-4 I4 shape; `depth=1` stops at the first
/// hop).
#[test]
fn depth_cap_bounds_chain_walk_via_handler() {
    let conn = fresh_db();

    insert_memory(&conn, "obs-leaf", "team/eng", "observation");
    let t_leaf = transcripts::store(&conn, "team/eng", "leaf", None).unwrap();
    backdate_transcript(&conn, &t_leaf.id, "2026-01-01T00:00:00Z");
    transcripts::link_transcript(&conn, "obs-leaf", &t_leaf.id, None, None).unwrap();

    insert_memory(&conn, "ref-mid", "team/eng", "reflection");
    let t_mid = transcripts::store(&conn, "team/eng", "mid", None).unwrap();
    backdate_transcript(&conn, &t_mid.id, "2026-01-02T00:00:00Z");
    transcripts::link_transcript(&conn, "ref-mid", &t_mid.id, None, None).unwrap();
    link_reflects_on(&conn, "ref-mid", "obs-leaf");

    insert_memory(&conn, "ref-top", "team/eng", "reflection");
    let t_top = transcripts::store(&conn, "team/eng", "top", None).unwrap();
    backdate_transcript(&conn, &t_top.id, "2026-01-03T00:00:00Z");
    transcripts::link_transcript(&conn, "ref-top", &t_top.id, None, None).unwrap();
    link_reflects_on(&conn, "ref-top", "ref-mid");

    // depth=null → full chain (3 transcripts).
    let full = mcp::handle_replay(&conn, &json!({"memory_id": "ref-top"}), None).unwrap();
    assert_eq!(full["count"], 3);

    // depth=2 → still full chain (no further ancestors after obs-leaf).
    let depth2 =
        mcp::handle_replay(&conn, &json!({"memory_id": "ref-top", "depth": 2}), None).unwrap();
    assert_eq!(depth2["count"], 3);

    // depth=1 → self + one hop = ref-top + ref-mid.
    let depth1 =
        mcp::handle_replay(&conn, &json!({"memory_id": "ref-top", "depth": 1}), None).unwrap();
    assert_eq!(depth1["count"], 2);
    let ids: Vec<&str> = depth1["transcripts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&t_top.id.as_str()));
    assert!(ids.contains(&t_mid.id.as_str()));
    assert!(!ids.contains(&t_leaf.id.as_str()));

    // depth=0 → self only (pre-L2-4 I4 shape).
    let depth0 =
        mcp::handle_replay(&conn, &json!({"memory_id": "ref-top", "depth": 0}), None).unwrap();
    assert_eq!(depth0["count"], 1);
    assert_eq!(
        depth0["transcripts"][0]["id"].as_str().unwrap(),
        t_top.id.as_str()
    );
}

/// L2-4 §Acceptance #3 — "Existing memory_replay for non-reflection
/// memories MUST be unchanged." An observation with its own transcript
/// plus a sibling observation linked to a different transcript returns
/// exactly the input memory's transcripts, ignores `depth`, and emits a
/// `source_memory_id` equal to the input memory id.
#[test]
fn non_reflection_replay_shape_unchanged_by_l2_4() {
    let conn = fresh_db();
    insert_memory(&conn, "obs-1", "team/eng", "observation");
    let t1 = transcripts::store(&conn, "team/eng", "body-1", None).unwrap();
    transcripts::link_transcript(&conn, "obs-1", &t1.id, None, None).unwrap();

    insert_memory(&conn, "obs-2", "team/eng", "observation");
    let t2 = transcripts::store(&conn, "team/eng", "body-2", None).unwrap();
    transcripts::link_transcript(&conn, "obs-2", &t2.id, None, None).unwrap();

    let payload = mcp::handle_replay(&conn, &json!({"memory_id": "obs-1"}), None).unwrap();
    assert_eq!(payload["count"], 1);
    let arr = payload["transcripts"].as_array().unwrap();
    assert_eq!(arr[0]["id"].as_str().unwrap(), t1.id.as_str());
    assert_eq!(arr[0]["source_memory_id"].as_str().unwrap(), "obs-1");
    assert_eq!(arr[0]["content"].as_str().unwrap(), "body-1");

    // `depth` is ignored on a non-reflection input.
    let payload =
        mcp::handle_replay(&conn, &json!({"memory_id": "obs-1", "depth": 99}), None).unwrap();
    assert_eq!(payload["count"], 1);
}

/// A reflection whose ancestor chain contains a hand-written cycle
/// (`a -> b -> a`) MUST NOT loop. The MCP handler returns a bounded
/// union with each transcript appearing once.
#[test]
fn cycle_in_reflects_on_does_not_loop_forever_via_handler() {
    let conn = fresh_db();

    insert_memory(&conn, "ref-a", "team/eng", "reflection");
    let t_a = transcripts::store(&conn, "team/eng", "a", None).unwrap();
    transcripts::link_transcript(&conn, "ref-a", &t_a.id, None, None).unwrap();

    insert_memory(&conn, "ref-b", "team/eng", "reflection");
    let t_b = transcripts::store(&conn, "team/eng", "b", None).unwrap();
    transcripts::link_transcript(&conn, "ref-b", &t_b.id, None, None).unwrap();

    link_reflects_on(&conn, "ref-a", "ref-b");
    link_reflects_on(&conn, "ref-b", "ref-a");

    let payload = mcp::handle_replay(&conn, &json!({"memory_id": "ref-a"}), None).unwrap();
    assert_eq!(payload["count"], 2, "cycle does not inflate dedup count");
    let ids: Vec<&str> = payload["transcripts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&t_a.id.as_str()));
    assert!(ids.contains(&t_b.id.as_str()));
}

/// Negative-typed `depth` is clamped to 0 (self-only) rather than
/// rejected. Defensive shape — a sloppy client doesn't need to special-
/// case the floor.
#[test]
fn negative_depth_clamps_to_self_only() {
    let conn = fresh_db();
    insert_memory(&conn, "obs-leaf", "team/eng", "observation");
    let t_leaf = transcripts::store(&conn, "team/eng", "leaf", None).unwrap();
    transcripts::link_transcript(&conn, "obs-leaf", &t_leaf.id, None, None).unwrap();

    insert_memory(&conn, "ref-top", "team/eng", "reflection");
    let t_top = transcripts::store(&conn, "team/eng", "top", None).unwrap();
    transcripts::link_transcript(&conn, "ref-top", &t_top.id, None, None).unwrap();
    link_reflects_on(&conn, "ref-top", "obs-leaf");

    let payload =
        mcp::handle_replay(&conn, &json!({"memory_id": "ref-top", "depth": -3}), None).unwrap();
    assert_eq!(payload["count"], 1, "negative depth clamps to 0");
    assert_eq!(
        payload["transcripts"][0]["id"].as_str().unwrap(),
        t_top.id.as_str()
    );
}

/// Non-integer `depth` (e.g. a string) is a structured error, not a
/// silent fallback. Pins the input-validation contract.
#[test]
fn non_integer_depth_is_a_typed_error() {
    let conn = fresh_db();
    insert_memory(&conn, "obs-1", "team/eng", "observation");

    let err = mcp::handle_replay(&conn, &json!({"memory_id": "obs-1", "depth": "many"}), None)
        .expect_err("non-integer depth must error");
    assert!(err.contains("depth must be an integer"), "got: {err}");
}

/// L2-4 — verbose truncation rule still applies on a reflection union.
/// A union with one >100KB transcript has its content omitted by
/// default; passing `verbose=true` opts back into the full content.
#[test]
fn verbose_truncation_applies_on_reflection_union() {
    let conn = fresh_db();
    insert_memory(&conn, "obs-big", "team/eng", "observation");
    // 120 KB body — exceeds the 100 KB threshold.
    let big = "x".repeat(120 * 1024);
    let t_big = transcripts::store(&conn, "team/eng", &big, None).unwrap();
    transcripts::link_transcript(&conn, "obs-big", &t_big.id, None, None).unwrap();

    insert_memory(&conn, "ref-top", "team/eng", "reflection");
    link_reflects_on(&conn, "ref-top", "obs-big");

    let payload = mcp::handle_replay(&conn, &json!({"memory_id": "ref-top"}), None).unwrap();
    assert_eq!(payload["count"], 1);
    let entry = &payload["transcripts"][0];
    assert_eq!(entry["truncated"], Value::Bool(true));
    assert!(entry["content"].is_null() || entry.get("content").is_none());

    let payload = mcp::handle_replay(
        &conn,
        &json!({"memory_id": "ref-top", "verbose": true}),
        None,
    )
    .unwrap();
    let entry = &payload["transcripts"][0];
    assert_eq!(entry["content"].as_str().unwrap().len(), 120 * 1024);
}
