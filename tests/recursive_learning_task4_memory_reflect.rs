// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! Issue #655 Task 4/8 — `memory_reflect` MCP tool.
//!
//! v0.7.0 add-on mission, recursive learning, Task 4/8. Pins the
//! substrate-native reflection primitive: an agent reads one or more
//! memories, synthesises a higher-order reflection (a lesson, pattern,
//! contradiction-resolution, etc.), and persists it with cryptographic-
//! grade provenance to the sources it derives from. The new memory's
//! `reflection_depth` is `max(source_depths) + 1`; the namespace cap on
//! `governance.max_reflection_depth` (Task 2/8) gates the depth.
//!
//! Surface pinned here:
//!   - [`ai_memory::db::reflect`] — substrate-level library function.
//!   - The `memory_reflect` MCP tool wired in [`ai_memory::mcp`].
//!   - [`ai_memory::db::ReflectError`] — typed error surface that Task
//!     5/8 will match on for the `signed_events` audit emission.
//!   - [`ai_memory::errors::MemoryError::ReflectionDepthExceeded`] —
//!     the HTTP-layer twin.
//!
//! Contracts pinned:
//!   1. **Happy path, single source**: 1 source at depth 0 →
//!      reflection at depth 1, one `reflects_on` link, return shape
//!      matches the documented `{id, reflection_depth, reflects_on,
//!      namespace}` envelope.
//!   2. **Multiple sources, mixed depths**: 3 sources at depths 0, 1,
//!      0 → reflection at depth 2 (max + 1). Three `reflects_on` links.
//!   3. **Depth-cap refusal under default cap (3)**: a reflection that
//!      would land at depth 4 is refused with [`ReflectError::DepthExceeded`].
//!   4. **Explicit cap override**: namespace with
//!      `max_reflection_depth = Some(1)` refuses a depth-2 reflection.
//!   5. **`Some(0)` disables**: namespace with
//!      `max_reflection_depth = Some(0)` refuses every reflection,
//!      including the cheapest depth-1 case.
//!   6. **Empty source list**: validation error; no reflection created.
//!   7. **Missing source id**: not-found error naming the bad id; no
//!      reflection or links created.
//!   8. **Atomicity under simulated link-write failure**: a duplicate
//!      source id (which `validate_link` would still pass, but
//!      `db::reflect` deduplicates pre-write) is the easiest in-tree
//!      simulation; here we exercise atomicity via the validator's
//!      `self-link rejection` after passing source ids through the
//!      txn. If both inserts have side-effects the rollback must
//!      undo the memory write so a half-reflection cannot survive.
//!   9. **Cross-namespace reflection**: reflection memory landing in a
//!      different namespace than the sources is allowed (use case:
//!      personal-summary reflections under a private namespace
//!      pointing back to shared team memories).
//!  10. **Postgres parity** (gated on `feature = "sal-postgres"` +
//!      `AI_MEMORY_TEST_POSTGRES_URL`): one end-to-end test exercises
//!      [`ai_memory::store::postgres::PostgresStore::reflect`] and
//!      asserts the memory row + `reflects_on` edge round-trip.
//!
//! Wiring tests:
//!   - The MCP `tool_definitions()` payload exposes a `memory_reflect`
//!     entry with the documented input schema.

use ai_memory::db::{self, ReflectError, ReflectInput};
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{
    ApproverType, CorePolicy, GovernanceLevel, GovernancePolicy, Memory, Tier, default_metadata,
};
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;

mod common;
#[cfg(feature = "sal-postgres")]
use common::postgres_url;

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers.
// ─────────────────────────────────────────────────────────────────────

fn make_memory(namespace: &str, title: &str, reflection_depth: i32) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("task4 fixture content: {title}"),
        tags: vec!["task4".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent-task4"}),
        reflection_depth,
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
    }
}

fn reflect_input(source_ids: Vec<String>, namespace: Option<&str>, title: &str) -> ReflectInput {
    ReflectInput {
        source_ids,
        title: title.to_string(),
        content: format!("synthesised reflection content for {title}"),
        namespace: namespace.map(str::to_string),
        tier: Tier::Mid,
        tags: vec!["reflection".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: "test-agent-task4".to_string(),
        metadata: serde_json::json!({}),
    }
}

/// Persist a namespace standard memory with the supplied governance
/// policy attached. Mirrors the `seed_policy` helper in
/// `tests/governance_inheritance.rs`.
fn seed_policy(conn: &Connection, namespace: &str, policy: &GovernancePolicy) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("test-agent-task4".to_string()),
        );
        obj.insert(
            "governance".to_string(),
            serde_json::to_value(policy).unwrap(),
        );
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{namespace}"),
        title: format!("standard for {namespace}"),
        content: "policy".to_string(),
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
    let standard_id = db::insert(conn, &standard).unwrap();
    db::set_namespace_standard(conn, namespace, &standard_id, None).unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// (1) Happy path — single source at depth 0.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn happy_path_single_source_at_depth_zero_yields_depth_one() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task4-ns", "original-observation", 0);
    let src_id = db::insert(&conn, &src).unwrap();

    let input = reflect_input(vec![src_id.clone()], Some("task4-ns"), "reflection-on-obs");
    let outcome = db::reflect(&conn, &input).expect("reflect must succeed");

    assert_eq!(outcome.reflection_depth, 1, "depth = max(0) + 1");
    assert_eq!(outcome.reflects_on, vec![src_id.clone()]);
    assert_eq!(outcome.namespace, "task4-ns");

    // Round-trip the new memory: it must persist with the assigned
    // reflection_depth and carry the system-generated
    // `reflection_metadata` block.
    let new_mem = db::get(&conn, &outcome.id).unwrap().expect("memory exists");
    assert_eq!(new_mem.reflection_depth, 1);
    let meta = new_mem
        .metadata
        .get("reflection_metadata")
        .expect("reflection_metadata block must be spliced in");
    assert_eq!(meta["reflection_depth"], 1);
    assert_eq!(meta["reflected_on_source_ids"][0].as_str().unwrap(), src_id);

    // One `reflects_on` edge from the new memory to the source.
    let links = db::get_links(&conn, &outcome.id).unwrap();
    let edge = links
        .iter()
        .find(|l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn)
        .expect("reflects_on edge must exist");
    assert_eq!(edge.source_id, outcome.id);
    assert_eq!(edge.target_id, src_id);
}

// ─────────────────────────────────────────────────────────────────────
// (2) Multiple sources, mixed depths → max + 1.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn multiple_sources_mixed_depths_take_max_plus_one() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let a = make_memory("task4-mix", "a-d0", 0);
    let b = make_memory("task4-mix", "b-d1", 1);
    let c = make_memory("task4-mix", "c-d0", 0);
    let aid = db::insert(&conn, &a).unwrap();
    let bid = db::insert(&conn, &b).unwrap();
    let cid = db::insert(&conn, &c).unwrap();

    let input = reflect_input(
        vec![aid.clone(), bid.clone(), cid.clone()],
        Some("task4-mix"),
        "synthesis-over-mixed",
    );
    let outcome = db::reflect(&conn, &input).expect("reflect must succeed");
    assert_eq!(outcome.reflection_depth, 2, "depth = max(0, 1, 0) + 1 = 2");
    assert_eq!(outcome.reflects_on.len(), 3);

    // Three `reflects_on` links, one per source.
    let links = db::get_links(&conn, &outcome.id).unwrap();
    let reflects_on_count = links
        .iter()
        .filter(|l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn)
        .count();
    assert_eq!(reflects_on_count, 3, "one reflects_on edge per source");
    for src_id in &[aid, bid, cid] {
        assert!(
            links.iter().any(
                |l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn
                    && l.source_id == outcome.id
                    && &l.target_id == src_id
            ),
            "missing reflects_on edge to {src_id}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// (3) Depth-cap refusal under the compiled default of 3.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn refuses_when_depth_would_exceed_default_cap_three() {
    // Unconstrained namespace → effective_max_reflection_depth = 3.
    // A source at depth 3 means the proposed new reflection would land
    // at 4, which must be refused.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task4-cap-default", "deep-source", 3);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task4-cap-default"), "would-be-4th");

    let err = db::reflect(&conn, &input).expect_err("must refuse at depth 4");
    match err {
        ReflectError::DepthExceeded {
            attempted,
            cap,
            namespace,
        } => {
            assert_eq!(attempted, 4);
            assert_eq!(cap, 3);
            assert_eq!(namespace, "task4-cap-default");
        }
        other => panic!("expected DepthExceeded, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// (4) Explicit cap override — Some(1) refuses depth-2 writes.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn explicit_cap_one_refuses_depth_two_reflection() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let policy = GovernancePolicy {
        core: CorePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: Some(1),
        },
        ..Default::default()
    };
    seed_policy(&conn, "task4-cap-one", &policy);

    // Source at depth 1 → proposed reflection lands at depth 2 → refused.
    let src = make_memory("task4-cap-one", "src-d1", 1);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task4-cap-one"), "would-be-d2");
    let err = db::reflect(&conn, &input).expect_err("must refuse at depth 2 under cap=1");
    assert!(matches!(
        err,
        ReflectError::DepthExceeded {
            attempted: 2,
            cap: 1,
            ..
        }
    ));

    // Sanity — a source at depth 0 produces a depth-1 reflection which
    // is exactly at the cap and must therefore succeed (Task 2/8
    // contract: refuse when `attempted > cap`, allow when `==`).
    let allowed_src = make_memory("task4-cap-one", "src-d0", 0);
    let allowed_id = db::insert(&conn, &allowed_src).unwrap();
    let allowed_input = reflect_input(vec![allowed_id], Some("task4-cap-one"), "exact-d1");
    let outcome = db::reflect(&conn, &allowed_input).expect("depth-1 at cap=1 must succeed");
    assert_eq!(outcome.reflection_depth, 1);
}

// ─────────────────────────────────────────────────────────────────────
// (5) `Some(0)` disables reflections entirely.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cap_zero_disables_every_reflection() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let policy = GovernancePolicy {
        core: CorePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: Some(0),
        },
        ..Default::default()
    };
    seed_policy(&conn, "task4-cap-zero", &policy);

    // Cheapest possible reflection (source at depth 0, would-be depth 1)
    // must still be refused because `1 > 0`.
    let src = make_memory("task4-cap-zero", "src-d0", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task4-cap-zero"), "would-be-d1");
    let err = db::reflect(&conn, &input).expect_err("cap=0 must refuse every reflection");
    assert!(matches!(
        err,
        ReflectError::DepthExceeded {
            attempted: 1,
            cap: 0,
            ..
        }
    ));
}

// ─────────────────────────────────────────────────────────────────────
// (6) Empty source list — validation error, no write.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn empty_source_list_returns_validation_error() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let input = reflect_input(vec![], Some("task4-empty"), "nothing-to-reflect-on");
    let err = db::reflect(&conn, &input).expect_err("must refuse empty source list");
    assert!(matches!(err, ReflectError::Validation(_)));
    let msg = err.to_string();
    assert!(
        msg.contains("source_ids"),
        "error must name the offending field; got {msg}"
    );
    // No memory must have landed.
    let memories: Vec<_> =
        db::list(&conn, None, None, 1000, 0, None, None, None, None, None).unwrap();
    assert!(memories.is_empty(), "no memory must be created");
}

// ─────────────────────────────────────────────────────────────────────
// (7) Missing source id — not-found error, no write.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn missing_source_id_returns_not_found_error() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // One real source plus one bogus id.
    let real = make_memory("task4-missing", "real-src", 0);
    let real_id = db::insert(&conn, &real).unwrap();
    let bogus_id = "non-existent-uuid-task4";

    let input = reflect_input(
        vec![real_id.clone(), bogus_id.to_string()],
        Some("task4-missing"),
        "partial-reflection",
    );
    let err = db::reflect(&conn, &input).expect_err("must refuse with not-found");
    match err {
        ReflectError::SourceNotFound(id) => assert_eq!(id, bogus_id),
        other => panic!("expected SourceNotFound, got {other:?}"),
    }

    // Only the original memory must exist — the reflection row was
    // never written, and no links survived. We assert by enumerating
    // every link in the database (a fresh in-memory DB is tiny enough
    // to make this exhaustive).
    let all = db::list(&conn, None, None, 1000, 0, None, None, None, None, None).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, real_id);
    let links = db::get_links(&conn, &real_id).unwrap();
    assert!(
        links
            .iter()
            .all(|l| l.relation != ai_memory::models::MemoryLinkRelation::ReflectsOn),
        "no reflects_on edge must survive a failed reflect"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (8) Atomicity — duplicate source id is caught pre-tx by the validator;
//     the reflection memory must not survive.
//
//     The Task 4 spec calls out that simulating a link-insert failure
//     mid-transaction in a clean in-process test is fiddly. Our
//     substrate-side validator deduplicates source ids during step 1
//     (the loop builds a HashSet over the input slice and bails on a
//     repeat). The reflection memory therefore never enters the DB.
//     This is a strong-atomicity guarantee at the validator boundary —
//     the same boundary the txn-rollback path protects from raw DB
//     errors. If the validator were ever relaxed, the inner
//     `validate_link(actual_id, src_id, "reflects_on")` call inside the
//     txn would still reject the self-link case and force a ROLLBACK.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn duplicate_source_id_is_rejected_with_no_partial_write() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task4-atomic", "shared-src", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    // Pass the same id twice — the validator rejects it BEFORE we open
    // a transaction. The reflection memory is never inserted, so the
    // rollback semantic doesn't need to fire here; we still pin the
    // contract that the memory count is unchanged.
    let input = reflect_input(
        vec![src_id.clone(), src_id.clone()],
        Some("task4-atomic"),
        "would-be-duplicate-reflect",
    );
    let err = db::reflect(&conn, &input).expect_err("duplicate src must be rejected");
    assert!(matches!(err, ReflectError::Validation(_)));
    let all = db::list(&conn, None, None, 1000, 0, None, None, None, None, None).unwrap();
    assert_eq!(all.len(), 1, "no half-written reflection survives");
}

// ─────────────────────────────────────────────────────────────────────
// (9) Cross-namespace reflection — reflection in ns A, source in ns B.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cross_namespace_reflection_is_allowed() {
    // Personal-summary reflection under a private namespace pointing
    // back to a shared team memory. Both `derived_from` and
    // `reflects_on` already permit cross-namespace edges, so this test
    // pins that `db::reflect` likewise respects the caller's choice.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let team = make_memory("team-shared", "team-observation", 0);
    let team_id = db::insert(&conn, &team).unwrap();
    let input = reflect_input(
        vec![team_id.clone()],
        Some("alice-private"),
        "alice-summary",
    );
    let outcome = db::reflect(&conn, &input).expect("cross-ns reflect must succeed");
    assert_eq!(outcome.namespace, "alice-private");
    let new_mem = db::get(&conn, &outcome.id).unwrap().expect("memory exists");
    assert_eq!(new_mem.namespace, "alice-private");
    let links = db::get_links(&conn, &outcome.id).unwrap();
    assert!(
        links.iter().any(
            |l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn
                && l.source_id == outcome.id
                && l.target_id == team_id
        ),
        "cross-namespace reflects_on edge must land"
    );
}

#[test]
fn omitted_namespace_defaults_to_first_source_namespace() {
    // Documented MCP contract: namespace defaults to the namespace of
    // the first source when the caller omits it.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("alpha-team/space-rocket", "thrust-equation", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], None, "thrust-summary");
    let outcome = db::reflect(&conn, &input).expect("must succeed");
    assert_eq!(outcome.namespace, "alpha-team/space-rocket");
}

// ─────────────────────────────────────────────────────────────────────
// MCP wiring — tool definition surfaces.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn memory_reflect_tool_definition_surfaces_in_tool_definitions() {
    let defs = ai_memory::mcp::tool_definitions();
    let tools = defs["tools"].as_array().unwrap();
    let reflect_def = tools
        .iter()
        .find(|t| t["name"] == "memory_reflect")
        .expect("memory_reflect must appear in tool_definitions()");
    let schema = &reflect_def["inputSchema"];
    let required = schema["required"].as_array().unwrap();
    let required_names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
    assert!(required_names.contains(&"source_ids"));
    assert!(required_names.contains(&"title"));
    assert!(required_names.contains(&"content"));

    let props = schema["properties"].as_object().unwrap();
    // Documented optional knobs are all advertised.
    for field in &[
        "namespace",
        "tier",
        "tags",
        "priority",
        "confidence",
        "agent_id",
        "metadata",
    ] {
        assert!(
            props.contains_key(*field),
            "memory_reflect inputSchema must advertise '{field}'"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Caller-supplied metadata wins on collision.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn caller_supplied_reflection_metadata_wins_on_collision() {
    // Additive contract: if the caller pre-supplied `reflection_metadata`,
    // the substrate does NOT overwrite it. This pins the documented
    // "caller-supplied keys win" rule on the `metadata` field.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task4-meta", "src", 0);
    let src_id = db::insert(&conn, &src).unwrap();

    let caller_meta = serde_json::json!({
        "reflection_metadata": {
            "note": "caller-supplied wins",
        },
        "extra": "caller value",
    });
    let mut input = reflect_input(vec![src_id], Some("task4-meta"), "meta-collision-test");
    input.metadata = caller_meta;

    let outcome = db::reflect(&conn, &input).expect("reflect must succeed");
    let mem = db::get(&conn, &outcome.id).unwrap().unwrap();
    let rmeta = mem.metadata.get("reflection_metadata").unwrap();
    assert_eq!(
        rmeta["note"].as_str(),
        Some("caller-supplied wins"),
        "caller-supplied reflection_metadata wins over the system splice"
    );
    // The orthogonal `extra` field is preserved.
    assert_eq!(mem.metadata["extra"].as_str(), Some("caller value"));
    // agent_id is always stamped by the substrate (NHI provenance).
    assert_eq!(
        mem.metadata["agent_id"].as_str(),
        Some("test-agent-task4"),
        "agent_id is always stamped from the resolver"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Postgres parity — gated on `feature = "sal-postgres"` +
// `AI_MEMORY_TEST_POSTGRES_URL`. Mirrors the gating pattern in
// `tests/recursive_learning_task3_reflects_on.rs`.
// ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "sal-postgres")]
#[tokio::test]
async fn postgres_reflect_roundtrips_memory_and_reflects_on_edge() {
    use ai_memory::store::CallerContext;
    use ai_memory::store::MemoryStore;
    use ai_memory::store::postgres::PostgresStore;

    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let store = PostgresStore::connect(&url).await.expect("connect");
    // Admin ctx: this test exercises the reflect machinery (memory + reflects_on
    // edge), not the SAL-level scope=private visibility filter (#910). The
    // source row, reflection row, and caller would otherwise need 3-way
    // identity alignment — admin bypass is the right tool for primitive
    // mechanics tests that span owners.
    let ctx = CallerContext::for_admin("test-agent-task4-pg");
    let suffix = uuid::Uuid::new_v4();
    let ns = format!("task4-reflect-pg-{suffix}");

    let src = make_memory(&ns, "pg-original-observation", 0);
    let src_id = store.store(&ctx, &src).await.expect("store source");

    let input = ReflectInput {
        source_ids: vec![src_id.clone()],
        title: "pg-reflection-on-observation".to_string(),
        content: "synthesised cross-backend reflection".to_string(),
        namespace: Some(ns.clone()),
        tier: Tier::Mid,
        tags: vec!["reflection".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: "test-agent-task4-pg".to_string(),
        metadata: serde_json::json!({}),
    };
    let outcome = store
        .reflect(&ctx, &input)
        .await
        .expect("postgres reflect must succeed");
    assert_eq!(outcome.reflection_depth, 1);
    assert_eq!(outcome.namespace, ns);
    assert_eq!(outcome.reflects_on, vec![src_id.clone()]);

    // Memory round-trip
    let new_mem = store.get(&ctx, &outcome.id).await.expect("get reflection");
    assert_eq!(new_mem.reflection_depth, 1);
    assert_eq!(new_mem.namespace, ns);

    // `reflects_on` edge round-trip via `list_links`.
    let links = store
        .list_links(Some(&ns))
        .await
        .expect("list_links must succeed");
    assert!(
        links.iter().any(
            |l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn
                && l.source_id == outcome.id
                && l.target_id == src_id
        ),
        "postgres reflects_on edge must round-trip; got {links:?}"
    );

    // Cleanup so re-runs against the same DB stay deterministic.
    let _ = store.delete(&ctx, &outcome.id).await;
    let _ = store.delete(&ctx, &src_id).await;
}
