// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 Track B2 — `memory_smart_load` MCP tool integration tests.
//!
//! B2 ships an always-on intent-routed front door over
//! `memory_load_family`. The agent passes a free-text intent (e.g.
//! "I'm about to debug a flaky test"), the handler picks the best
//! `Family` via the family-descriptor scorer (B3 embedding cache when
//! wired through; deterministic keyword fallback otherwise), and
//! forwards to `memory_load_family` for the underlying DB query.
//!
//! Scenarios pinned here:
//! 1. Intent matching `Family::Graph` → returns graph-family
//!    memories with `chosen_family == "graph"`.
//! 2. Intent matching `Family::Lifecycle` → returns lifecycle-family
//!    memories with `chosen_family == "lifecycle"`.
//! 3. Empty intent → returns `Family::Core` fallback with
//!    `chosen_family_source == "fallback"`.
//! 4. Embedder unavailable (None passed) → keyword fallback drives
//!    routing; ambiguous intent still picks Core via the no-overlap
//!    fallback path.
//!
//! The B3 embedding cache is not wired through `AppState` yet, so all
//! four scenarios run against the deterministic keyword scorer — which
//! is the hard-fallback path the spec calls out as
//! `chosen_family_source: "fallback"` / `"keyword"`. When B3 lands the
//! same scenarios light up against `state.best_family_match(intent)`
//! without changes to the wire shape pinned here.

use ai_memory::db;
use ai_memory::mcp::handle_smart_load;
use ai_memory::models::{Memory, Tier};
use chrono::Utc;
use serde_json::{Value, json};

fn open_db() -> rusqlite::Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

/// Seed a memory tagged with `metadata.family = <family>` so the
/// underlying `memory_load_family` query (which `handle_smart_load`
/// forwards to) returns a row.
fn seed_family_memory(
    conn: &rusqlite::Connection,
    title: &str,
    namespace: &str,
    family: &str,
    priority: i32,
) -> String {
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("seeded for {family}"),
        tags: vec![],
        priority,
        confidence: 1.0,
        source: "import".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"family": family}),
    };
    db::insert(conn, &mem).expect("db::insert")
}

// ---------------------------------------------------------------------------
// 1. Intent that pattern-matches Family::Graph routes to graph-family
//    memories. The keyword fallback descriptor for graph carries
//    "debug", "flaky", "test", "investigate", "trace" so the canonical
//    "I'm about to debug a flaky test" intent lands on graph.
// ---------------------------------------------------------------------------
#[test]
fn smart_load_intent_matching_graph_routes_to_graph_family() {
    let conn = open_db();

    // Seed one memory per family so we can confirm the chosen family's
    // memory comes back and others don't.
    let graph_id = seed_family_memory(&conn, "graph-mem", "ns", "graph", 5);
    let _core_id = seed_family_memory(&conn, "core-mem", "ns", "core", 5);
    let _life_id = seed_family_memory(&conn, "life-mem", "ns", "lifecycle", 5);

    let resp: Value = handle_smart_load(
        &conn,
        &json!({"intent": "I'm about to debug a flaky test"}),
        None,
    )
    .expect("memory_smart_load must succeed");

    assert_eq!(
        resp["chosen_family"], "graph",
        "intent='debug a flaky test' must route to graph; got: {resp}"
    );
    assert_eq!(
        resp["chosen_family_source"], "keyword",
        "no embedder => keyword path; got: {resp}"
    );
    assert!(
        resp["score"].as_f64().unwrap() > 0.0,
        "keyword overlap must surface a positive score; got: {resp}"
    );
    assert_eq!(resp["count"], 1, "only the graph memory is returned");
    let memories = resp["memories"].as_array().expect("memories array");
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["id"], graph_id);
    assert_eq!(memories[0]["title"], "graph-mem");
}

// ---------------------------------------------------------------------------
// 2. Intent that pattern-matches Family::Lifecycle routes to lifecycle
//    memories. The lifecycle descriptor carries "update", "edit",
//    "delete", "forget", "promote" so an upgrade-flavoured intent
//    lands on lifecycle.
// ---------------------------------------------------------------------------
#[test]
fn smart_load_intent_matching_lifecycle_routes_to_lifecycle_family() {
    let conn = open_db();

    let life_id = seed_family_memory(&conn, "life-mem", "ns", "lifecycle", 5);
    let _core_id = seed_family_memory(&conn, "core-mem", "ns", "core", 5);

    let resp: Value = handle_smart_load(
        &conn,
        &json!({"intent": "delete and forget the stale memories then promote the survivors"}),
        None,
    )
    .expect("memory_smart_load must succeed");

    assert_eq!(
        resp["chosen_family"], "lifecycle",
        "intent must route to lifecycle; got: {resp}"
    );
    assert_eq!(resp["chosen_family_source"], "keyword");
    assert_eq!(resp["count"], 1);
    let memories = resp["memories"].as_array().expect("memories array");
    assert_eq!(memories[0]["id"], life_id);
}

// ---------------------------------------------------------------------------
// 3. Empty intent returns the canonical Core fallback. The handler
//    treats "" (or whitespace) as "no signal", routes to Family::Core,
//    and reports `chosen_family_source: "fallback"` so the caller can
//    detect the no-signal branch instead of silently picking core.
// ---------------------------------------------------------------------------
#[test]
fn smart_load_empty_intent_returns_core_fallback() {
    let conn = open_db();

    let core_id = seed_family_memory(&conn, "core-mem", "ns", "core", 5);

    let resp: Value =
        handle_smart_load(&conn, &json!({"intent": "   "}), None).expect("must succeed");

    assert_eq!(resp["chosen_family"], "core");
    assert_eq!(
        resp["chosen_family_source"], "fallback",
        "empty intent must surface the no-signal fallback; got: {resp}"
    );
    assert_eq!(resp["score"], 0.0);
    assert_eq!(resp["count"], 1);
    let memories = resp["memories"].as_array().expect("memories array");
    assert_eq!(memories[0]["id"], core_id);
}

// ---------------------------------------------------------------------------
// 4. Embedder unavailable (None) + intent that doesn't overlap any
//    family descriptor → keyword scorer falls back to Family::Core
//    with `chosen_family_source: "fallback"`. This is the spec's
//    "embedder not yet ready" branch — the handler must not silently
//    pick a default family without surfacing the no-signal flag.
// ---------------------------------------------------------------------------
#[test]
fn smart_load_embedder_unavailable_falls_back_to_core() {
    let conn = open_db();

    let core_id = seed_family_memory(&conn, "core-mem", "ns", "core", 5);

    // "blortzfribblequx" doesn't appear in any family descriptor, so
    // the keyword scorer reports zero overlap → fallback to Core.
    let resp: Value = handle_smart_load(
        &conn,
        &json!({"intent": "blortzfribblequx zarflargle"}),
        None,
    )
    .expect("must succeed even with no embedder + no descriptor match");

    assert_eq!(
        resp["chosen_family"], "core",
        "no descriptor overlap must fall back to core; got: {resp}"
    );
    assert_eq!(resp["chosen_family_source"], "fallback");
    assert_eq!(resp["score"], 0.0);
    assert_eq!(resp["count"], 1);
    let memories = resp["memories"].as_array().expect("memories array");
    assert_eq!(memories[0]["id"], core_id);
}

// ---------------------------------------------------------------------------
// 5. Bonus — missing required `intent` arg yields a clean diagnostic.
//    Pinned alongside the four spec scenarios so the smoke matrix and
//    the handler stay in sync on the required-arg contract.
// ---------------------------------------------------------------------------
#[test]
fn smart_load_missing_intent_arg_errors() {
    let conn = open_db();
    let err = handle_smart_load(&conn, &json!({}), None).unwrap_err();
    assert!(
        err.contains("intent"),
        "error must mention missing arg; got: {err}"
    );
}
