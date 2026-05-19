// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F14 — `memory_smart_load` router under-weights underscore
//! tokens.
//!
//! v0.7.0 Round-2 fixes the keyword fallback to:
//!   1. Tokenize tool names on underscore boundaries
//!      (`memory_notify` → `memory`, `notify`) AND keep the full
//!      identifier as a single token.
//!   2. Boost tool-name overlaps 2x descriptor-token overlaps so a
//!      family whose tool names directly mention an intent keyword
//!      out-scores a family that only matches via a generic
//!      descriptor word.
//!
//! Pinned scenarios:
//! - "send a notification to another agent" → `other` (because
//!   `memory_notify` lives there). Pre-fix this routed to `meta`.
//! - "expand a query and find related memories" → `power` (because
//!   `memory_expand_query` lives there). Pre-fix this tied with
//!   graph (which has `memory_kg_query`) and lost the tiebreaker.
//! - The 8 originally-passing intents must still route correctly:
//!   debug a flaky test → graph, delete and forget → lifecycle,
//!   etc.

use ai_memory::db;
use ai_memory::mcp::handle_smart_load;
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, Tier};
use chrono::Utc;
use serde_json::{Value, json};

fn open_db() -> rusqlite::Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn seed_family(conn: &rusqlite::Connection, family: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: "ns".to_string(),
        title: format!("{family}-mem"),
        content: format!("seeded for {family}"),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "import".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"family": family}),
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
    db::insert(conn, &mem).expect("db::insert")
}

fn route(intent: &str) -> Value {
    let conn = open_db();
    // Seed every family so the routing decision is authoritative —
    // any family that wins a route returns at least one row.
    for fam in [
        "core",
        "lifecycle",
        "graph",
        "governance",
        "power",
        "meta",
        "archive",
        "other",
    ] {
        seed_family(&conn, fam);
    }
    handle_smart_load(&conn, &json!({"intent": intent}), None).expect("smart_load must succeed")
}

// ---------------------------------------------------------------------------
// F14 case 1: "send a notification" → other (because memory_notify).
// Pre-fix: routed to meta.
// ---------------------------------------------------------------------------
#[test]
fn f14_notify_intent_routes_to_other_family() {
    let resp = route("I want to send a notification to another agent");
    assert_eq!(
        resp["chosen_family"], "other",
        "intent='send a notification to another agent' must route to other \
         (where memory_notify lives); got: {resp}"
    );
    assert_eq!(resp["chosen_family_source"], "keyword");
}

// ---------------------------------------------------------------------------
// F14 case 2: "expand a query" → power (because memory_expand_query).
// Pre-fix: tied with graph and lost the tiebreaker.
// ---------------------------------------------------------------------------
#[test]
fn f14_expand_query_intent_routes_to_power_family() {
    let resp = route("I want to expand a query and find related memories");
    assert_eq!(
        resp["chosen_family"], "power",
        "intent='expand a query' must route to power (where memory_expand_query \
         lives); got: {resp}"
    );
    assert_eq!(resp["chosen_family_source"], "keyword");
}

// ---------------------------------------------------------------------------
// F14 regression-guard: the 8 originally-passing intents still route
// correctly after the tokenization/weight changes.
// ---------------------------------------------------------------------------
#[test]
fn f14_originally_passing_intents_still_route_correctly() {
    let cases: &[(&str, &str)] = &[
        // Lifecycle — delete/forget/promote vocabulary.
        (
            "delete and forget the stale memories then promote the survivors",
            "lifecycle",
        ),
        // Graph — debug a flaky test (graph descriptor has the
        // canonical "debug flaky test investigate trace" terms).
        ("I'm about to debug a flaky test", "graph"),
        // Graph — knowledge-graph query.
        ("query the knowledge graph for entity timeline", "graph"),
        // Governance — approve/reject/pending vocabulary.
        ("approve the pending governance review", "governance"),
        // Power — consolidate/contradiction/duplicate.
        (
            "consolidate duplicate memories that contradict each other",
            "power",
        ),
        // Archive — backup/restore/old/historical.
        ("restore an archived backup of old memories", "archive"),
        // Meta — capabilities/agent/session.
        ("register a new agent and start a session", "meta"),
        // Core — search/store/recall.
        ("recall and search for stored memories", "core"),
    ];

    for (intent, expected_family) in cases {
        let resp = route(intent);
        assert_eq!(
            resp["chosen_family"], *expected_family,
            "intent={intent:?} expected to route to {expected_family}; got: {resp}"
        );
    }
}

// ---------------------------------------------------------------------------
// F14 unit-level: tool-name tokens are matched (memory_notify intent
// matches Other family on the full identifier).
// ---------------------------------------------------------------------------
#[test]
fn f14_full_tool_identifier_matches_via_underscore_preserved_token() {
    let resp = route("call memory_notify on the other agent");
    assert_eq!(
        resp["chosen_family"], "other",
        "an intent that names memory_notify verbatim must route to other; got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// F14 unit-level: empty intent still falls back to core (no regression
// from the tokenization change).
// ---------------------------------------------------------------------------
#[test]
fn f14_empty_intent_still_falls_back_to_core() {
    let resp = route("   ");
    assert_eq!(resp["chosen_family"], "core");
    assert_eq!(resp["chosen_family_source"], "fallback");
}
