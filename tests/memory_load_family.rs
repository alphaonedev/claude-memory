// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 Track B1 — `memory_load_family` MCP tool integration tests.
//!
//! B1 ships an always-on alternative to `memory_recall` for the case
//! where the agent already knows the `Family` taxonomy bucket it cares
//! about. The handler filters memories by `metadata.family` (one of the
//! eight enum names) and returns the top-k recent + high-priority
//! matches inside the requested namespace.
//!
//! Scenarios pinned here:
//! 1. Family filter is exact — three memories with different families
//!    seeded; loading one family returns only its match.
//! 2. Namespace filter narrows the result set (cross-namespace
//!    bleed-through is a regression).
//! 3. `k` defaults to 20 and caps at 100 (silent clamp, not error).
//! 4. Unknown family returns the canonical `UnknownFamily` diagnostic.

use ai_memory::db;
use ai_memory::mcp::handle_load_family;
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{self, Memory, Tier};
use chrono::Utc;
use serde_json::{Value, json};

fn open_db() -> rusqlite::Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

/// Seed a memory tagged with `metadata.family = <family>` and the given
/// namespace. The B1 handler walks `json_extract(metadata, '$.family')`,
/// so the family-string must land inside the metadata JSON.
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
    let _ = models::default_metadata(); // keep the import live for parity with sibling tests
    db::insert(conn, &mem).expect("db::insert")
}

// ---------------------------------------------------------------------------
// 1. Family filter is exact — only the matching family is returned.
// ---------------------------------------------------------------------------
#[test]
fn load_family_returns_only_matching_family() {
    let conn = open_db();

    // Three memories under the same namespace but with three different
    // family tags. memory_load_family(family=core) must return only the
    // first one.
    let core_id = seed_family_memory(&conn, "core-mem", "ns-a", "core", 5);
    let _graph_id = seed_family_memory(&conn, "graph-mem", "ns-a", "graph", 5);
    let _power_id = seed_family_memory(&conn, "power-mem", "ns-a", "power", 5);

    let resp: Value = handle_load_family(&conn, &json!({"family": "core", "namespace": "ns-a"}))
        .expect("memory_load_family must succeed");

    assert_eq!(resp["family"], "core");
    assert_eq!(resp["namespace"], "ns-a");
    assert_eq!(
        resp["count"], 1,
        "only the core memory must come back; got: {resp}"
    );
    let memories = resp["memories"].as_array().expect("memories array");
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["id"], core_id);
    assert_eq!(memories[0]["title"], "core-mem");
}

// ---------------------------------------------------------------------------
// 2. Namespace filter narrows the result set — cross-namespace
//    bleed-through is a regression.
// ---------------------------------------------------------------------------
#[test]
fn load_family_namespace_filter_narrows_result_set() {
    let conn = open_db();

    // Two `core` memories in two different namespaces.
    let here_id = seed_family_memory(&conn, "here-core", "ns-a", "core", 5);
    let _there_id = seed_family_memory(&conn, "there-core", "ns-b", "core", 5);

    let resp: Value = handle_load_family(&conn, &json!({"family": "core", "namespace": "ns-a"}))
        .expect("memory_load_family must succeed");

    assert_eq!(
        resp["count"], 1,
        "namespace must filter cross-ns rows out; got: {resp}"
    );
    let memories = resp["memories"].as_array().expect("memories array");
    assert_eq!(memories[0]["id"], here_id);

    // Without the namespace filter, both rows must come back.
    let resp_all: Value =
        handle_load_family(&conn, &json!({"family": "core"})).expect("must succeed without ns");
    assert_eq!(
        resp_all["count"], 2,
        "no-namespace must span all; got: {resp_all}"
    );
}

// ---------------------------------------------------------------------------
// 3. Priority + recency ordering — higher-priority rows come first;
//    `k` caps the response.
// ---------------------------------------------------------------------------
#[test]
fn load_family_orders_by_priority_then_recency_and_caps_k() {
    let conn = open_db();

    // Seed three core memories with descending priorities.
    let _low = seed_family_memory(&conn, "low", "ns", "core", 1);
    let _mid = seed_family_memory(&conn, "mid", "ns", "core", 5);
    let _hi = seed_family_memory(&conn, "hi", "ns", "core", 9);

    let resp: Value = handle_load_family(&conn, &json!({"family": "core", "k": 2})).unwrap();
    assert_eq!(resp["count"], 2, "k=2 must cap to two rows");
    let memories = resp["memories"].as_array().unwrap();
    assert_eq!(memories[0]["title"], "hi");
    assert_eq!(memories[1]["title"], "mid");
    assert_eq!(resp["k"], 2);

    // k clamps silently at 100 — passing 9999 is treated as 100, not an
    // error. We only have 3 rows so the count is 3 either way; the
    // payload's reported `k` is the clamped value.
    let resp_big: Value = handle_load_family(&conn, &json!({"family": "core", "k": 9999})).unwrap();
    assert_eq!(resp_big["k"], 100, "k must clamp to 100");
    assert_eq!(resp_big["count"], 3);
}

// ---------------------------------------------------------------------------
// 4. Unknown family → canonical UnknownFamily diagnostic (lists valid
//    family names so the caller can self-correct).
// ---------------------------------------------------------------------------
#[test]
fn load_family_unknown_family_yields_diagnostic_error() {
    let conn = open_db();
    let err = handle_load_family(&conn, &json!({"family": "xyz"})).unwrap_err();
    assert!(
        err.contains("xyz"),
        "diagnostic must echo the bad value; got: {err}"
    );
    assert!(
        err.contains("core"),
        "diagnostic must list valid families; got: {err}"
    );
    assert!(
        err.contains("graph"),
        "diagnostic must list valid families; got: {err}"
    );
}

// ---------------------------------------------------------------------------
// 5. Missing required `family` arg → handler returns Err.
// ---------------------------------------------------------------------------
#[test]
fn load_family_missing_family_arg_errors() {
    let conn = open_db();
    let err = handle_load_family(&conn, &json!({})).unwrap_err();
    assert!(
        err.contains("family"),
        "error must mention missing arg; got: {err}"
    );
}
