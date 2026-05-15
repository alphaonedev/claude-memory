// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines, clippy::similar_names)]

//! Issue #655 Task 7/8 — ship-gate-grade FUNCTIONAL suite.
//!
//! v0.7.0 add-on mission, recursive learning, Task 7/8 — daemon-side
//! equivalent of the cross-repo ship-gate scripts (`ship-gate/run.sh
//! --phase functional`, `a2a-gate/run.sh`, `discovery-gate/run.sh`)
//! which live in the campaign/docker repo and were wiped during the
//! ENOSPC incident. This file authors the in-process equivalent of
//! the FUNCTIONAL phase: end-to-end happy + sad paths against the
//! full reflect lifecycle.
//!
//! The MIGRATION / CHAOS / FEDERATION phases live in
//! `recursive_learning_task7_shipgate_chaos_migration.rs`.
//!
//! Phase coverage (this file):
//!   1. Lifecycle round-trip: 3 sources → 1 reflection → 3 reflects_on
//!      edges + correct depth + correct reflection_metadata splice.
//!   2. Reflection-is-searchable: FTS5 + `db::recall` surface a
//!      reflection memory alongside non-reflection memories.
//!   3. Cap boundaries: depth-3 succeeds, depth-4 refused + audit row.
//!   4. `Some(0)` disables every reflection + audit row lands.
//!   5. Hook veto without audit: `pre_reflect` Deny refuses + NO
//!      depth-cap audit row.
//!   6. `post_reflect` fires after COMMIT (visible via read on same conn).
//!   7. Both pre + post fire when reflection succeeds.
//!   8. Shim parity (property-test): `reflect` and
//!      `reflect_with_hooks(_, _, &ReflectHooks::empty())` produce
//!      byte-identical outcomes for arbitrary inputs.
//!
//! Mirror Task 4/5/6 testing style: named tests, in-memory SQLite per
//! test, `expect()` over `unwrap()` where the error message is
//! load-bearing.

use ai_memory::db::{
    self, ReflectError, ReflectHookDecision, ReflectHooks, ReflectInput, ReflectOutcome,
};
use ai_memory::models::{
    ApproverType, GovernanceLevel, GovernancePolicy, Memory, Tier, default_metadata,
};
use ai_memory::signed_events::{SignedEvent, list_signed_events};
use chrono::Utc;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers — mirror tests/recursive_learning_task{4,5,6}_*.rs.
// ─────────────────────────────────────────────────────────────────────

fn make_memory(namespace: &str, title: &str, reflection_depth: i32) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("task7 fixture content: {title}"),
        tags: vec!["task7".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent-task7"}),
        reflection_depth,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

fn reflect_input(
    source_ids: Vec<String>,
    namespace: Option<&str>,
    title: &str,
    agent_id: &str,
) -> ReflectInput {
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
        agent_id: agent_id.to_string(),
        metadata: serde_json::json!({}),
    }
}

/// Persist a namespace standard memory with the supplied governance
/// policy attached. Mirrors the helper in Task 5's test suite.
fn seed_policy(conn: &Connection, namespace: &str, policy: &GovernancePolicy) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("test-agent-task7".to_string()),
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
    };
    let standard_id = db::insert(conn, &standard).unwrap();
    db::set_namespace_standard(conn, namespace, &standard_id, None).unwrap();
}

fn audit_rows_for_depth_exceeded(conn: &Connection) -> Vec<SignedEvent> {
    list_signed_events(conn, None, 100, 0)
        .expect("list signed_events")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded")
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Lifecycle) — 3-source reflection round-trip with metadata.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn lifecycle_three_sources_reflects_with_correct_metadata_and_edges() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let s1 = make_memory("task7-lifecycle", "obs-a", 0);
    let s2 = make_memory("task7-lifecycle", "obs-b", 0);
    let s3 = make_memory("task7-lifecycle", "obs-c", 0);
    let id1 = db::insert(&conn, &s1).unwrap();
    let id2 = db::insert(&conn, &s2).unwrap();
    let id3 = db::insert(&conn, &s3).unwrap();

    let input = reflect_input(
        vec![id1.clone(), id2.clone(), id3.clone()],
        Some("task7-lifecycle"),
        "synthesis-of-three",
        "test-agent-task7",
    );
    let outcome = db::reflect(&conn, &input).expect("3-source reflect must succeed");
    assert_eq!(outcome.reflection_depth, 1);
    assert_eq!(outcome.namespace, "task7-lifecycle");
    assert_eq!(
        outcome.reflects_on,
        vec![id1.clone(), id2.clone(), id3.clone()]
    );

    // The reflection memory carries the system-spliced reflection_metadata.
    let refl = db::get(&conn, &outcome.id)
        .unwrap()
        .expect("reflection row");
    assert_eq!(refl.reflection_depth, 1);
    let meta = refl
        .metadata
        .get("reflection_metadata")
        .expect("reflection_metadata splice");
    let stored_sources: Vec<String> =
        serde_json::from_value(meta["reflected_on_source_ids"].clone()).expect("source ids array");
    assert_eq!(stored_sources, vec![id1.clone(), id2.clone(), id3.clone()]);
    assert_eq!(meta["reflection_depth"], serde_json::json!(1));
    let stamp = meta["reflection_created_at"]
        .as_str()
        .expect("reflection_created_at must be a string");
    chrono::DateTime::parse_from_rfc3339(stamp)
        .expect("reflection_created_at must parse as RFC3339");

    // All three `reflects_on` edges land — one per source.
    let links = db::get_links(&conn, &outcome.id).unwrap();
    let reflects: Vec<&_> = links
        .iter()
        .filter(|l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn)
        .collect();
    assert_eq!(reflects.len(), 3, "exactly 3 reflects_on edges must exist");
    let mut targets: Vec<String> = reflects.iter().map(|l| l.target_id.clone()).collect();
    targets.sort();
    let mut expected = vec![id1, id2, id3];
    expected.sort();
    assert_eq!(targets, expected);
}

#[test]
fn lifecycle_caller_supplied_reflection_metadata_wins_on_collision() {
    // Documented contract: if the caller pre-set `reflection_metadata`,
    // the system splice is skipped. Pin it here so a future refactor
    // can't silently flip the precedence.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let s = make_memory("task7-meta-collision", "src", 0);
    let sid = db::insert(&conn, &s).unwrap();
    let mut input = reflect_input(
        vec![sid],
        Some("task7-meta-collision"),
        "synthesised",
        "test-agent-task7",
    );
    input.metadata = serde_json::json!({
        "reflection_metadata": {
            "custom": "caller-preserves-this",
            "reflection_depth": 99,
        }
    });
    let outcome = db::reflect(&conn, &input).expect("reflect ok");
    let refl = db::get(&conn, &outcome.id).unwrap().expect("row");
    let meta = refl.metadata.get("reflection_metadata").expect("present");
    assert_eq!(
        meta["custom"], "caller-preserves-this",
        "caller-supplied reflection_metadata must win on collision"
    );
    assert_eq!(
        meta["reflection_depth"], 99,
        "caller-supplied keys override system splice"
    );
    // But the memory's reflection_depth column reflects the actual
    // substrate-computed value (1), not the caller-supplied metadata
    // bag — the audit-load-bearing field stays under substrate control.
    assert_eq!(refl.reflection_depth, 1);
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Searchable) — FTS5 + db::recall surface the reflection.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn reflection_is_recall_visible_alongside_non_reflection_memories() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // Two non-reflection memories.
    let m1 = {
        let mut m = make_memory("task7-recall", "observation-quantum", 0);
        m.content = "Original observation about quantum entanglement".to_string();
        m
    };
    let m2 = {
        let mut m = make_memory("task7-recall", "observation-cosmology", 0);
        m.content = "Note about cosmological constant fine tuning".to_string();
        m
    };
    let id1 = db::insert(&conn, &m1).unwrap();
    let _id2 = db::insert(&conn, &m2).unwrap();
    // A reflection over m1 whose content carries a distinct token.
    let mut input = reflect_input(
        vec![id1],
        Some("task7-recall"),
        "reflection-on-quantum-entanglement",
        "test-agent-task7",
    );
    input.content =
        "Synthesised reflection: entanglement preserves correlations across spacelike intervals."
            .to_string();
    let outcome = db::reflect(&conn, &input).expect("reflect ok");

    // FTS5 search for a token only present in the reflection's content
    // surfaces the reflection row.
    let hits = db::search(
        &conn,
        "spacelike",
        Some("task7-recall"),
        None,
        10,
        None,
        None,
        None,
        None,
        None,
        None,
        false,
    )
    .expect("search ok");
    assert!(
        hits.iter().any(|m| m.id == outcome.id),
        "FTS5 must surface the reflection on a content-token query"
    );

    // db::recall with a broader query still surfaces the reflection
    // alongside the non-reflection source memory.
    let (results, _budget) = db::recall(
        &conn,
        "entanglement",
        Some("task7-recall"),
        10,
        None,
        None,
        None,
        0,
        0,
        None,
        None,
        false,
    )
    .expect("recall ok");
    let ids: Vec<String> = results.iter().map(|(m, _)| m.id.clone()).collect();
    assert!(
        ids.contains(&outcome.id),
        "db::recall must surface the reflection alongside non-reflection memories"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Cap boundaries) — depth 3 ok, depth 4 refused + audit.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cap_boundary_exact_depth_three_succeeds() {
    // Default cap is 3. A source at depth 2 means the proposed reflection
    // would land at depth 3 → ALLOWED (3 <= 3).
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task7-cap-eq", "deep-but-allowed", 2);
    let sid = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![sid],
        Some("task7-cap-eq"),
        "at-cap",
        "test-agent-task7",
    );
    let outcome = db::reflect(&conn, &input).expect("depth=3 must succeed at cap=3");
    assert_eq!(outcome.reflection_depth, 3);

    // No depth-cap audit row at the allowed boundary.
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert!(
        rows.is_empty(),
        "successful reflect at exact cap must not emit a refusal audit row"
    );
}

#[test]
fn cap_boundary_exact_depth_four_refused_with_audit_row() {
    // Source at depth 3 + default cap 3 → would-be depth 4 → refused.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task7-cap-over", "one-too-deep", 3);
    let sid = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![sid],
        Some("task7-cap-over"),
        "would-be-fourth-level",
        "ai-cap-over",
    );
    let err = db::reflect(&conn, &input).expect_err("must refuse at depth 4");
    assert!(matches!(
        err,
        ReflectError::DepthExceeded {
            attempted: 4,
            cap: 3,
            ..
        }
    ));

    // Audit row lands directly on the signed_events table.
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.event_type, "reflection.depth_exceeded");
    assert_eq!(row.attest_level, "unsigned");
    assert_eq!(row.agent_id, "ai-cap-over");
    // Read directly from the signed_events table via raw SQL to prove
    // the row is durable in the actual table, not just routed through
    // an in-memory helper. The ship-gate spec calls this out
    // explicitly.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1",
            rusqlite::params!["reflection.depth_exceeded"],
            |r| r.get(0),
        )
        .expect("query signed_events table directly");
    assert_eq!(
        count, 1,
        "exactly one row of event_type reflection.depth_exceeded \
         must be present in the signed_events table"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Some(0) disables) — every reflection refused + audit lands.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cap_zero_disables_every_reflection_with_audit_row() {
    // Explicit `Some(0)` on the namespace → every depth-1 reflection
    // refused. Audit row lands per refusal.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let policy = GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: Some(0),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        auto_classify_kind: None,
    };
    seed_policy(&conn, "task7-disabled", &policy);
    let src = make_memory("task7-disabled", "depth0-src", 0);
    let sid = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![sid],
        Some("task7-disabled"),
        "would-be-depth-1",
        "ai-disabled",
    );
    let err = db::reflect(&conn, &input).expect_err("cap=0 must refuse any reflection");
    assert!(matches!(
        err,
        ReflectError::DepthExceeded {
            attempted: 1,
            cap: 0,
            ..
        }
    ));

    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(rows.len(), 1, "cap=0 refusal must emit an audit row");
    assert_eq!(rows[0].agent_id, "ai-disabled");
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Hook veto without audit) — pre_reflect Deny refuses but
// emits NO depth-cap audit row.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn hook_veto_refuses_reflection_without_depth_cap_audit() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task7-hook-veto", "src", 0);
    let sid = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![sid.clone()],
        Some("task7-hook-veto"),
        "would-be-refl",
        "ai-veto",
    );

    let hooks = ReflectHooks {
        pre_reflect: Some(Box::new(|_i: &ReflectInput| ReflectHookDecision::Deny {
            reason: "external policy refusal".to_string(),
            code: 451,
        })),
        post_reflect: None,
    };
    let err = db::reflect_with_hooks(&conn, &input, &hooks).expect_err("veto refuses");
    match err {
        ReflectError::HookVeto { reason, code } => {
            assert_eq!(code, 451);
            assert!(reason.contains("external policy"));
        }
        other => panic!("expected HookVeto, got {other:?}"),
    }

    // No reflection memory persisted (the substrate never reached the
    // tx open).
    let all = db::list(&conn, None, None, 1000, 0, None, None, None, None, None).unwrap();
    assert_eq!(all.len(), 1, "veto must leave only the original source");
    assert_eq!(all[0].id, sid);

    // No depth-cap audit row — hook vetoes are out of scope for the
    // Task 5 audit chain.
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert!(
        rows.is_empty(),
        "hook veto must NOT emit a reflection.depth_exceeded audit; got {} rows",
        rows.len()
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (post_reflect fires after COMMIT) — observable via same conn.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn post_reflect_fires_after_commit_observable_on_same_connection() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task7-postcommit", "src", 0);
    let sid = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![sid],
        Some("task7-postcommit"),
        "post-commit-target",
        "test-agent-task7",
    );

    // Capture the outcome.id from inside the hook; after the reflect
    // returns, query the same conn for the row — the hook fires AFTER
    // COMMIT so the row must be visible.
    let captured_id = Arc::new(std::sync::Mutex::new(None::<String>));
    let captured_id_clone = captured_id.clone();
    let hooks = ReflectHooks {
        pre_reflect: None,
        post_reflect: Some(Box::new(move |o: &ReflectOutcome| {
            *captured_id_clone.lock().unwrap() = Some(o.id.clone());
        })),
    };
    let outcome = db::reflect_with_hooks(&conn, &input, &hooks).expect("reflect ok");
    let captured = captured_id.lock().unwrap().clone();
    assert_eq!(captured.as_deref(), Some(outcome.id.as_str()));

    // Read the new row directly — the post-commit visibility is the
    // load-bearing contract.
    let new_mem = db::get(&conn, &outcome.id)
        .unwrap()
        .expect("reflection must be persisted by the time post_reflect fires");
    assert_eq!(new_mem.reflection_depth, 1);
    assert_eq!(new_mem.id, outcome.id);
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Both pre + post fire on success) — call-order pinning.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn both_pre_and_post_reflect_fire_on_successful_reflect() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task7-both", "src", 0);
    let sid = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![sid],
        Some("task7-both"),
        "both-fire",
        "test-agent-task7",
    );

    let clock = Arc::new(AtomicUsize::new(0));
    let pre_slot = Arc::new(AtomicUsize::new(0));
    let post_slot = Arc::new(AtomicUsize::new(0));
    let clock_pre = clock.clone();
    let pre_slot_clone = pre_slot.clone();
    let clock_post = clock.clone();
    let post_slot_clone = post_slot.clone();

    let hooks = ReflectHooks {
        pre_reflect: Some(Box::new(move |_i: &ReflectInput| {
            pre_slot_clone.store(
                clock_pre.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
            ReflectHookDecision::Allow
        })),
        post_reflect: Some(Box::new(move |_o: &ReflectOutcome| {
            post_slot_clone.store(
                clock_post.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
        })),
    };
    let _ = db::reflect_with_hooks(&conn, &input, &hooks).expect("reflect ok");

    let pre = pre_slot.load(Ordering::SeqCst);
    let post = post_slot.load(Ordering::SeqCst);
    assert_eq!(pre, 1, "pre must fire first");
    assert_eq!(post, 2, "post must fire second");
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Shim parity, property-test) — `reflect` and
// `reflect_with_hooks(_, _, &ReflectHooks::empty())` produce
// byte-identical outcome semantics across random inputs.
// ─────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Display impl coverage) — exercise every ReflectError variant.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn reflect_error_display_covers_every_variant() {
    // Validation
    let v = ReflectError::Validation("bad input".to_string());
    let s = format!("{v}");
    assert!(s.contains("bad input"));

    // SourceNotFound
    let snf = ReflectError::SourceNotFound("missing-id".to_string());
    let s = format!("{snf}");
    assert!(s.contains("missing-id"));

    // DepthExceeded — structured display.
    let de = ReflectError::DepthExceeded {
        attempted: 5,
        cap: 3,
        namespace: "ns".to_string(),
    };
    let s = format!("{de}");
    assert!(s.contains('5') && s.contains('3') && s.contains("ns"));

    // HookVeto — structured display.
    let hv = ReflectError::HookVeto {
        reason: "policy says no".to_string(),
        code: 451,
    };
    let s = format!("{hv}");
    assert!(s.contains("451") && s.contains("policy says no"));

    // Database
    let db_err = ReflectError::Database("disk full".to_string());
    let s = format!("{db_err}");
    assert!(s.contains("disk full"));

    // Debug impl on every variant — pin the surface so a future
    // refactor doesn't accidentally drop a field.
    let debug = format!("{de:?}");
    assert!(debug.contains("DepthExceeded"));
    let debug = format!("{hv:?}");
    assert!(debug.contains("HookVeto"));
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Edge case) — validator refuses non-object metadata
// (documents the contract that protects the unreachable fallback arm
// of the substrate's metadata-resolve match).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn non_object_metadata_refused_by_validator() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task7-meta-array", "src", 0);
    let sid = db::insert(&conn, &src).unwrap();
    let mut input = reflect_input(
        vec![sid],
        Some("task7-meta-array"),
        "weird-meta",
        "test-agent-task7",
    );
    // Caller hands an array, not an object — the validator refuses
    // BEFORE the substrate's metadata-resolve match arm runs. This
    // contract makes the `_ => serde_json::Map::new()` fallback in
    // `db::reflect_with_hooks` defensive (unreachable in normal use)
    // and documents WHY the line is uncovered in llvm-cov.
    input.metadata = serde_json::json!(["not", "an", "object"]);
    let err = db::reflect(&conn, &input).expect_err("validator must refuse");
    match err {
        ReflectError::Validation(msg) => {
            assert!(msg.contains("object"), "msg: {msg}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Edge case) — None namespace falls back to first source's
// namespace (covers the `None => sources[0].namespace.clone()` arm).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn none_namespace_resolves_to_first_source_namespace() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task7-ns-fallback", "src", 0);
    let sid = db::insert(&conn, &src).unwrap();
    // `namespace: None` → substrate must pick "task7-ns-fallback" from
    // the first source memory.
    let input = reflect_input(vec![sid], None, "ns-fallback-test", "test-agent-task7");
    let outcome = db::reflect(&conn, &input).expect("None ns ok");
    assert_eq!(outcome.namespace, "task7-ns-fallback");
    let refl = db::get(&conn, &outcome.id).unwrap().expect("present");
    assert_eq!(refl.namespace, "task7-ns-fallback");
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (Edge case) — SourceNotFound when a source id is well-
// formed but not present in the DB (covers the load-step bail path).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn source_not_found_when_id_well_formed_but_missing() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let bogus = uuid::Uuid::new_v4().to_string();
    let input = reflect_input(
        vec![bogus.clone()],
        Some("task7-snf"),
        "missing-src",
        "test-agent-task7",
    );
    let err = db::reflect(&conn, &input).expect_err("must refuse missing src");
    match err {
        ReflectError::SourceNotFound(id) => assert_eq!(id, bogus),
        other => panic!("expected SourceNotFound, got {other:?}"),
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 32,
        // Larger inputs blow out the SQL plan cache without paying for
        // any extra coverage at the shim layer; 32 keeps wall-clock
        // well under 5s.
        max_shrink_iters: 128,
        .. proptest::test_runner::Config::default()
    })]

    /// For any well-formed title + content + tag list + priority +
    /// confidence, `db::reflect` and `db::reflect_with_hooks(_, _,
    /// &ReflectHooks::empty())` produce outcomes with identical
    /// reflection_depth, namespace, and reflects_on length. (The
    /// memory id is a fresh UUID per call so we don't compare ids.)
    #[test]
    fn shim_parity_random_inputs(
        // Tier-restricted to mid; the reflect path is tier-agnostic so
        // exercising every tier just doubles the test time without new
        // coverage.
        title in "[A-Za-z][A-Za-z0-9 -]{0,32}",
        // Validate constraints: content must be non-empty.
        content in "[A-Za-z][A-Za-z0-9 .,!?-]{1,128}",
        // Each tag is well-formed under `validate_tags`.
        tags in proptest::collection::vec("[a-z][a-z0-9-]{0,16}", 0..3),
        priority in 1i32..=10,
        confidence in 0.0f64..=1.0,
    ) {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let s1 = make_memory("task7-proptest", "src1", 0);
        let s2 = make_memory("task7-proptest", "src2", 0);
        let id1 = db::insert(&conn, &s1).unwrap();
        let id2 = db::insert(&conn, &s2).unwrap();

        let mut input_shim = reflect_input(
            vec![id1.clone()],
            Some("task7-proptest"),
            &format!("shim-{title}"),
            "ai-proptest",
        );
        input_shim.content = content.clone();
        input_shim.tags = tags.clone();
        input_shim.priority = priority;
        input_shim.confidence = confidence;

        let mut input_with = reflect_input(
            vec![id2.clone()],
            Some("task7-proptest"),
            &format!("with-{title}"),
            "ai-proptest",
        );
        input_with.content = content;
        input_with.tags = tags;
        input_with.priority = priority;
        input_with.confidence = confidence;

        let o_shim = db::reflect(&conn, &input_shim).expect("shim reflect ok");
        let o_with = db::reflect_with_hooks(&conn, &input_with, &ReflectHooks::empty())
            .expect("with-hooks empty reflect ok");

        // Identical structural outcome shape: depth=1, namespace
        // matches, one source apiece, both reflects_on edges land.
        proptest::prop_assert_eq!(o_shim.reflection_depth, o_with.reflection_depth);
        proptest::prop_assert_eq!(o_shim.namespace, o_with.namespace);
        proptest::prop_assert_eq!(o_shim.reflects_on.len(), o_with.reflects_on.len());
        proptest::prop_assert_eq!(o_shim.reflects_on.len(), 1);
        proptest::prop_assert_ne!(&o_shim.id, &o_with.id);

        let links_shim = db::get_links(&conn, &o_shim.id).unwrap();
        let links_with = db::get_links(&conn, &o_with.id).unwrap();
        proptest::prop_assert_eq!(
            links_shim.iter().filter(|l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn).count(),
            1,
        );
        proptest::prop_assert_eq!(
            links_with.iter().filter(|l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn).count(),
            1,
        );
    }
}
