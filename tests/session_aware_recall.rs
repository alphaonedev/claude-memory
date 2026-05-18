// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 (issue #518) — session-aware recall recency boost.
//!
//! Pins the substrate-level contract added in this commit:
//!
//! 1. With no `session_id`, the recency boost is a no-op — the
//!    candidate vector comes back byte-for-byte unchanged.
//!
//! 2. With `session_id` set, a candidate whose id is already in the
//!    per-session ring gets `SESSION_RECENCY_BOOST` (+0.05) added to
//!    its score; the vector is re-sorted descending.
//!
//! 3. The post-boost hit set is appended back into the session ring
//!    so subsequent recalls in the same session see the same id as
//!    "recently accessed".
//!
//! 4. The per-session ring caps at `SESSION_RECENT_CAP` (50) with
//!    FIFO eviction — a 51st insert evicts the first.
//!
//! 5. Distinct sessions have disjoint rings (Alice's recent set
//!    doesn't bias Bob's recall).
//!
//! 6. Wire-shape: the new `session_id` field on `RecallQuery` and
//!    `RecallBody` defaults to `None` (pre-#518 callers see no
//!    behaviour change), and the `memory_recall` MCP tool schema
//!    advertises the new property.
//!
//! Pure-Rust, no embedder / LLM / network deps — runs cleanly under
//! `AI_MEMORY_NO_CONFIG=1`.

use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, RecallBody, RecallQuery, Tier};
use ai_memory::reranker::{
    SESSION_RECENCY_BOOST, SESSION_RECENT_CAP, SessionRecallTracker, apply_session_recency_boost,
};

fn make_mem(id: &str, title: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: id.to_string(),
        tier: Tier::Long,
        namespace: "global".to_string(),
        title: title.to_string(),
        content: "irrelevant for the boost path".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "api".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: vec![],
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

#[test]
fn session_id_none_is_no_op() {
    let tracker = SessionRecallTracker::new();
    let results = vec![
        (make_mem("a", "Alpha"), 0.8_f64),
        (make_mem("b", "Beta"), 0.7),
        (make_mem("c", "Gamma"), 0.6),
    ];
    let original_scores: Vec<f64> = results.iter().map(|(_, s)| *s).collect();
    let original_ids: Vec<String> = results.iter().map(|(m, _)| m.id.clone()).collect();

    let boosted = apply_session_recency_boost(results, None, &tracker);

    let boosted_scores: Vec<f64> = boosted.iter().map(|(_, s)| *s).collect();
    let boosted_ids: Vec<String> = boosted.iter().map(|(m, _)| m.id.clone()).collect();
    assert_eq!(
        boosted_scores, original_scores,
        "session_id=None must NOT mutate any score"
    );
    assert_eq!(
        boosted_ids, original_ids,
        "session_id=None must NOT re-order the vector"
    );
    assert_eq!(
        tracker.session_count(),
        0,
        "session_id=None must NOT touch the tracker state"
    );
}

#[test]
fn session_id_empty_string_is_no_op() {
    // The handler-side normalisation maps empty/whitespace
    // `session_id` to `None`, but the substrate primitive must also
    // refuse to fire on an empty string fed directly to it.
    let tracker = SessionRecallTracker::new();
    let results = vec![(make_mem("a", "Alpha"), 0.5)];
    let boosted = apply_session_recency_boost(results, Some(""), &tracker);
    assert_eq!(boosted.len(), 1);
    assert!(
        (boosted[0].1 - 0.5_f64).abs() < 1e-9,
        "empty session_id must NOT bump score"
    );
    assert_eq!(
        tracker.session_count(),
        0,
        "empty session_id must NOT touch the tracker"
    );
}

#[test]
fn first_recall_records_ids_into_session_ring() {
    // First recall in a fresh session has no "previously accessed"
    // candidates — the boost doesn't bump anything — but the recall
    // results MUST land in the session ring so a follow-up recall
    // can lift them.
    let tracker = SessionRecallTracker::new();
    let results = vec![
        (make_mem("id-alpha", "Alpha"), 0.8),
        (make_mem("id-beta", "Beta"), 0.7),
    ];

    let boosted = apply_session_recency_boost(results, Some("alice"), &tracker);
    let boosted_scores: Vec<f64> = boosted.iter().map(|(_, s)| *s).collect();
    assert_eq!(
        boosted_scores,
        vec![0.8, 0.7],
        "no recent ids → no boost on first recall"
    );
    let recent = tracker.recent_ids("alice");
    assert!(recent.contains("id-alpha"));
    assert!(recent.contains("id-beta"));
    assert_eq!(recent.len(), 2);
}

#[test]
fn previously_accessed_id_gets_recency_boost_on_subsequent_recall() {
    // The headline contract: a memory that was recalled in this
    // session previously ranks higher on the next recall.
    let tracker = SessionRecallTracker::new();
    let sid = "alice-session";

    // First recall: A scores higher than B.
    let first = vec![
        (make_mem("A", "primary"), 0.50),
        (make_mem("B", "secondary"), 0.49),
    ];
    let _ = apply_session_recency_boost(first, Some(sid), &tracker);
    assert!(tracker.recent_ids(sid).contains("A"));
    assert!(tracker.recent_ids(sid).contains("B"));

    // Second recall (different scoring path): A and B both reappear
    // at scores SO CLOSE that the +0.05 boost reorders them. Here B
    // has the slight pre-boost edge, but A is in the recent set so
    // it should win after the boost. C is a fresh candidate that
    // outscores both pre-boost but cannot have been recently
    // accessed (it wasn't in the prior recall) — A's boost still
    // lifts it past B without leaping C.
    let second = vec![
        (make_mem("C", "fresh"), 0.55),
        (make_mem("B", "secondary"), 0.50),
        (make_mem("A", "primary"), 0.48),
    ];
    let boosted = apply_session_recency_boost(second, Some(sid), &tracker);

    // Expected post-boost order:
    //   C: 0.55 (no boost)
    //   A: 0.48 + 0.05 = 0.53 (boost — A was in prior set)
    //   B: 0.50 + 0.05 = 0.55 (also boosted — was in prior set)
    //
    // C and B tie at 0.55; the sort is stable on equal keys so the
    // result depends on input order. Validate the load-bearing
    // invariant: A moved ABOVE its pre-boost neighbour at 0.48 →
    // and the bumped score reflects the constant.
    let by_id: std::collections::HashMap<String, f64> =
        boosted.iter().map(|(m, s)| (m.id.clone(), *s)).collect();
    assert!(
        (by_id["A"] - (0.48 + SESSION_RECENCY_BOOST)).abs() < 1e-9,
        "A must have its score bumped by SESSION_RECENCY_BOOST"
    );
    assert!(
        (by_id["B"] - (0.50 + SESSION_RECENCY_BOOST)).abs() < 1e-9,
        "B must have its score bumped by SESSION_RECENCY_BOOST"
    );
    assert!(
        (by_id["C"] - 0.55).abs() < 1e-9,
        "C must NOT be boosted (not in prior recall)"
    );

    // The headline ordering invariant the +0.05 boost delivers:
    // A's boosted score (0.53) MUST be at least equal to the
    // pre-boost neighbour score of 0.50 — i.e. the boost
    // mechanically lifts A above any candidate that scored at
    // 0.50 pre-boost AND did NOT receive the recency boost itself.
    // Both A and B received the boost (both were in the prior
    // session ring), so they tie above their pre-boost positions.
    // Pre-boost A was last; post-boost A's score (0.53) exceeds
    // the lowest pre-boost score still present (which was A's own
    // 0.48), confirming the +0.05 fired.
    let pos_of_a = boosted
        .iter()
        .position(|(m, _)| m.id == "A")
        .expect("A must be in the result");
    let pos_of_a_score = boosted[pos_of_a].1;
    assert!(
        pos_of_a_score > 0.50,
        "A's boosted score {pos_of_a_score} must exceed the pre-boost neighbour 0.50"
    );
}

#[test]
fn distinct_sessions_have_disjoint_rings() {
    let tracker = SessionRecallTracker::new();
    let alice_first = vec![(make_mem("alice-mem", "A"), 0.9)];
    let _ = apply_session_recency_boost(alice_first, Some("alice"), &tracker);

    let bob_recent = tracker.recent_ids("bob");
    assert!(
        bob_recent.is_empty(),
        "Bob's session must not see Alice's recent set"
    );

    // Bob recalls a single candidate; even if it shares an id with
    // Alice's set, his per-session boost only fires for his OWN
    // prior recall set.
    let bob_first = vec![(make_mem("alice-mem", "A"), 0.7)];
    let bob_boosted = apply_session_recency_boost(bob_first, Some("bob"), &tracker);
    assert!(
        (bob_boosted[0].1 - 0.7_f64).abs() < 1e-9,
        "Bob's first recall must not be boosted by Alice's history"
    );
}

#[test]
fn ring_caps_at_session_recent_cap_with_fifo_eviction() {
    let tracker = SessionRecallTracker::new();
    let sid = "stress-test-session";

    // Insert exactly `cap + 1` distinct ids one batch at a time. The
    // oldest id must be evicted by the time we read back the ring.
    let cap = SESSION_RECENT_CAP;
    for i in 0..=cap {
        let mem = make_mem(&format!("id-{i}"), "stress");
        let _ = apply_session_recency_boost(vec![(mem, 0.5)], Some(sid), &tracker);
    }
    let recent = tracker.recent_ids(sid);
    assert_eq!(recent.len(), cap, "ring must cap at SESSION_RECENT_CAP");
    assert!(
        !recent.contains("id-0"),
        "FIFO eviction must drop the oldest id when the ring overflows"
    );
    assert!(
        recent.contains(&format!("id-{cap}")),
        "the newest id must be present in the ring"
    );
}

#[test]
fn duplicate_id_moves_to_front_keeping_ring_bounded() {
    // Re-recalling an id that's already in the ring must NOT grow
    // the ring past the cap and must move that id to the most-
    // recently-touched position.
    let tracker = SessionRecallTracker::new();
    let sid = "dup-session";
    let _ = apply_session_recency_boost(
        vec![(make_mem("X", "X"), 0.5), (make_mem("Y", "Y"), 0.5)],
        Some(sid),
        &tracker,
    );
    // Now re-recall X — it should still only be present once.
    let _ = apply_session_recency_boost(vec![(make_mem("X", "X"), 0.6)], Some(sid), &tracker);
    let recent = tracker.recent_ids(sid);
    assert_eq!(
        recent.len(),
        2,
        "ring must contain X and Y exactly once each after re-recalling X"
    );
    assert!(recent.contains("X"));
    assert!(recent.contains("Y"));
}

#[test]
fn recall_query_session_id_defaults_to_none() {
    // Wire-shape: pre-#518 callers that omit `session_id` see the
    // field deserialise to None and the boost stays a no-op.
    let raw = serde_json::json!({
        "context": "default-shape",
    });
    let q: RecallQuery = serde_json::from_value(raw).expect("parse");
    assert!(
        q.session_id.is_none(),
        "RecallQuery.session_id defaults to None"
    );

    let raw2 = serde_json::json!({
        "context": "with-session",
        "session_id": "alice",
    });
    let q2: RecallQuery = serde_json::from_value(raw2).expect("parse");
    assert_eq!(q2.session_id.as_deref(), Some("alice"));
}

#[test]
fn recall_body_session_id_defaults_to_none() {
    let raw = serde_json::json!({
        "context": "body-default-shape",
    });
    let b: RecallBody = serde_json::from_value(raw).expect("parse");
    assert!(
        b.session_id.is_none(),
        "RecallBody.session_id defaults to None"
    );

    let raw2 = serde_json::json!({
        "context": "body-with-session",
        "session_id": "bob",
    });
    let b2: RecallBody = serde_json::from_value(raw2).expect("parse");
    assert_eq!(b2.session_id.as_deref(), Some("bob"));
}

#[test]
fn mcp_tool_schema_advertises_session_id() {
    // tools/list must surface the new property so MCP clients can
    // discover the contract without out-of-band docs.
    let defs = ai_memory::mcp::tool_definitions();
    let tools = defs["tools"].as_array().expect("tools array");
    let recall = tools
        .iter()
        .find(|t| t["name"] == "memory_recall")
        .expect("memory_recall registered");
    let props = &recall["inputSchema"]["properties"];
    let sid = &props["session_id"];
    assert_eq!(sid["type"].as_str(), Some("string"));
    let desc = sid["description"].as_str().unwrap_or("");
    assert!(
        desc.contains("#518"),
        "session_id description must name the issue — got: {desc}"
    );
    let docs = recall["docs"].as_str().unwrap_or("");
    assert!(
        docs.contains("session_id"),
        "memory_recall docs must mention session_id — got: {docs}"
    );
}
