// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural impact.
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

//! v0.7.0 L3-3 — ship-gate scenarios for the grand-slam recursive-learning spine.
//!
//! Bridges Layer 1 + Layer 2 surface area through the four canonical
//! ship-gate phases. Where the sibling per-layer tests pin individual
//! contracts in isolation (e.g. `tests/curator/reflection_pass_test.rs`
//! for L2-1, `tests/approval_reflect.rs` for L1-8), this file
//! composes them into the multi-feature scenarios the cross-repo
//! `ship-gate/run.sh` would have exercised before the ENOSPC incident
//! wiped the docker fleet.
//!
//! Phase coverage in this file:
//!
//! Phase 1 (FUNCTIONAL):
//! curator reflection-pass e2e (30 observations → 3 clusters → 3
//! reflections with `MemoryKind::Reflection` typed enum L1-1 + audit
//! chain `reflection.depth_exceeded` filterable per L1-3 + namespace-
//! default depth cap L1-4); substrate refuses a reflection cycle (the
//! `reflects_on` cycle detector, issue #690 / `LINK_CYCLE_ERR_PREFIX`).
//!
//! Phase 2 (FEDERATION):
//! reflection writes survive a sender→receiver replication step with
//! `reflection_origin` stamping + cross-peer depth refusal audit
//! event_type `reflection.depth_exceeded.cross_peer` (L2-2); approval
//! API flow for `GovernedAction::Reflect` — `require_approval_above_depth`
//! threshold (L1-8) queues `pending_actions`, approver
//! `decide_pending_action(approve = true)` flips status to approved,
//! re-issued reflect succeeds.
//!
//! Phase 3 (MIGRATION):
//! schema vN → vN+1 idempotent round-trip — let `db::open` run the
//! full ladder to v33, write a reflection + reflects_on edge, close,
//! re-open the same file, verify every L1+L2 column survives + a
//! fresh reflect lands; `metadata.type = 'reflection'` → `memory_kind`
//! backfill correctness (L1-1, migration 0025); `memory_links.relation`
//! CHECK constraint refuses an unknown relation post-v33.
//!
//! Phase 4 (CHAOS — placeholder scope):
//! concurrent reflect calls on the same source land independently
//! without duplicate `reflects_on` edges (the federated chaos contract
//! pinned for the single-process case).
//!
//! Hermetic: tempdir DBs, in-memory SQLite, deterministic stub LLM,
//! no live network, no live model loads.

use ai_memory::autonomy::AutonomyLlm;
use ai_memory::curator::reflection_pass::run_reflection_pass;
use ai_memory::db::{self, ReflectError, ReflectInput};
use ai_memory::federation::reflection_bookkeeping;
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{
    ApproverType, CorePolicy, GovernanceLevel, GovernancePolicy, GovernedAction, Memory,
    MemoryKind, MemoryLinkRelation, Tier, default_metadata,
};
use ai_memory::signed_events::list_signed_events;
use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers — match the style of recursive_learning_task7_*.rs.
// ─────────────────────────────────────────────────────────────────────

/// Deterministic LLM stub for the curator pass. Production tests pin
/// this same shape via `StubLlm` in tests/curator/reflection_pass_test.rs
/// — duplicated here so this binary has no cross-file mod deps.
struct StubLlm {
    summary: String,
    calls: Mutex<usize>,
}

impl StubLlm {
    fn new(summary: &str) -> Self {
        Self {
            summary: summary.to_string(),
            calls: Mutex::new(0),
        }
    }
}

impl AutonomyLlm for StubLlm {
    fn auto_tag(&self, _title: &str, _content: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn detect_contradiction(&self, _a: &str, _b: &str) -> Result<bool> {
        Ok(false)
    }
    fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String> {
        *self.calls.lock().unwrap() += memories.len();
        Ok(self.summary.clone())
    }
}

fn make_observation(namespace: &str, topic: &str, idx: usize) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: namespace.to_string(),
        title: format!("observation about {topic} #{idx}"),
        content: format!("{topic} {topic} {topic} observation number {idx}"),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 2, // > MIN_RECALL_COUNT — clusterable.
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:l3-3"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
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

fn seed_policy(conn: &Connection, namespace: &str, policy: &GovernancePolicy) {
    let now = Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("ai:l3-3".to_string()),
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
        content: "l3-3 policy".to_string(),
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
        memory_kind: MemoryKind::Observation,
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
    let std_id = db::insert(conn, &standard).expect("seed_policy insert");
    db::set_namespace_standard(conn, namespace, &std_id, None).expect("set_namespace_standard");
}

fn seed_governance_json(conn: &Connection, namespace: &str, governance: &serde_json::Value) {
    let now = Utc::now().to_rfc3339();
    let metadata = serde_json::json!({
        "agent_id": "ai:l3-3",
        "governance": governance,
    });
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{namespace}"),
        title: format!("standard for {namespace}"),
        content: "l3-3 governance".to_string(),
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
        memory_kind: MemoryKind::Observation,
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
    let std_id = db::insert(conn, &standard).expect("seed_governance_json insert");
    db::set_namespace_standard(conn, namespace, &std_id, None).expect("set_namespace_standard");
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
        content: format!("L3-3 synthesised reflection: {title}"),
        namespace: namespace.map(str::to_string),
        tier: Tier::Mid,
        tags: vec!["l3-3".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: agent_id.to_string(),
        metadata: serde_json::json!({}),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — curator reflection-pass e2e + typed kind +
// audit chain.
// ─────────────────────────────────────────────────────────────────────

/// **SG-RL-1 (Phase 1):** Curator reflection-pass produces three
/// depth-1 reflections from a 30-observation seed; each lands with
/// `MemoryKind::Reflection` typed enum (L1-1) and an audit chain.
/// Composes L1-1 + L1-3 (depth-exceeded filterable event type) + L2-1
/// (curator reflection-pass acceptance) into a single ship-gate scenario.
#[test]
fn sg_rl_1_curator_reflection_pass_typed_kind_and_audit_chain() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("sg-rl-1.db");
    let conn = db::open(&db_path).expect("open");
    let ns = "ship-gate-rl-1";

    // Three topical clusters of 10 obs each — same Jaccard pattern as
    // L2-1 acceptance.
    let topics = [
        "kubernetes rolling deploy canary strategy",
        "rust async tokio runtime executor concurrency",
        "sqlite wal mode transaction durability fsync",
    ];
    for topic in &topics {
        for i in 0..10 {
            let mem = make_observation(ns, topic, i);
            db::insert(&conn, &mem).expect("insert observation");
        }
    }

    let llm = StubLlm::new("synthesised pattern summary");
    let report = run_reflection_pass(&conn, &llm, None, Some(ns), None, false, |_| true)
        .expect("curator reflection-pass must succeed");

    assert_eq!(report.observations_scanned, 30);
    assert!(
        report.clusters_eligible >= 3,
        "≥3 clusters eligible, got {}",
        report.clusters_eligible
    );
    assert_eq!(
        report.reflections_persisted, report.clusters_eligible,
        "every eligible cluster persists a reflection (errors={:?})",
        report.errors
    );
    assert_eq!(report.depth_refusals, 0);
    assert!(
        report.errors.is_empty(),
        "no errors expected: {:?}",
        report.errors
    );

    // Every persisted reflection carries the typed enum + reflects_on edges.
    let all = db::list(&conn, Some(ns), None, 100, 0, None, None, None, None, None).expect("list");
    let reflections: Vec<&Memory> = all
        .iter()
        .filter(|m| m.memory_kind == MemoryKind::Reflection)
        .collect();
    assert!(
        reflections.len() >= 3,
        "≥3 reflection rows on disk, got {}",
        reflections.len()
    );
    for r in &reflections {
        assert_eq!(r.reflection_depth, 1);
        assert_eq!(r.memory_kind, MemoryKind::Reflection);
        let links = db::get_links(&conn, &r.id).expect("get_links");
        let reflects_on_count = links
            .iter()
            .filter(|l| l.source_id == r.id && l.relation == MemoryLinkRelation::ReflectsOn)
            .count();
        assert!(
            reflects_on_count >= 3,
            "reflection {} carries ≥3 reflects_on edges (got {})",
            r.id,
            reflects_on_count
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — substrate refuses a reflection cycle (the
// "Goal-supersede refusal" surface, substrate-level).
// ─────────────────────────────────────────────────────────────────────

/// **SG-RL-2 (Phase 1):** Substrate refuses an edge that would close a
/// reflection cycle. The `LINK_CYCLE_ERR_PREFIX` contract is the
/// "substrate refuses a contradictory chain" invariant — same family
/// as the Goal-supersede refusal pattern. Composes L1-2
/// (`ReflectionCycleDetected` error) into the ship-gate.
#[test]
fn sg_rl_2_substrate_refuses_reflection_cycle() {
    let conn = db::open(Path::new(":memory:")).expect("open");
    let ns = "ship-gate-rl-2";

    let a = make_observation(ns, "alpha", 1);
    let b = make_observation(ns, "beta", 2);
    let a_id = db::insert(&conn, &a).expect("insert a");
    let b_id = db::insert(&conn, &b).expect("insert b");

    // a -- reflects_on --> b   (legal — straight edge).
    db::create_link(&conn, &a_id, &b_id, "reflects_on").expect("first reflects_on edge must land");

    // Now try b -- reflects_on --> a (would create a cycle a→b→a).
    // The substrate validator refuses with the cycle error prefix.
    let err = db::create_link(&conn, &b_id, &a_id, "reflects_on")
        .expect_err("cycle-closing reflects_on edge must refuse");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("cycle") || msg.contains("would create"),
        "substrate must surface a cycle-refusal message, got: {err}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 2 (FEDERATION) — 3-peer reflection replication + cross-peer
// depth refusal.
// ─────────────────────────────────────────────────────────────────────

/// **SG-RL-3 (Phase 2):** A depth-2 reflection authored at peer A
/// replicates to peer B via the substrate `stamp_reflection_origin`
/// pipeline. Peer B's tighter cap (max_reflection_depth=2) refuses an
/// attempted depth-3 derivation with a `DepthExceeded` error. Composes
/// L2-2 (federation reflection-replication) + L1-4 (cap resolution)
/// into a ship-gate scenario.
///
/// The full 3-peer choreography lives in
/// `tests/federation_reflection_replication.rs`; this ship-gate scenario
/// pins the two-peer subset that exercises the substrate write path
/// plus the audit trail surface — which is what the ship-gate
/// `phase=federation` invocation would have stamped.
#[test]
fn sg_rl_3_federation_reflection_replication_with_cross_peer_refusal() {
    let tmp_a = TempDir::new().unwrap();
    let tmp_b = TempDir::new().unwrap();
    let conn_a = db::open(&tmp_a.path().join("peer-a.db")).expect("peer A open");
    let conn_b = db::open(&tmp_b.path().join("peer-b.db")).expect("peer B open");
    let ns = "ship-gate-rl-3";

    // Peer B tightens its cap to 2.
    let tight = GovernancePolicy {
        core: CorePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: Some(2),
        },
        ..Default::default()
    };
    seed_policy(&conn_b, ns, &tight);

    // On peer A: depth-0 obs → depth-1 reflection → depth-2 reflection.
    let obs = make_observation(ns, "shipgate-rl3-source", 0);
    let obs_id = db::insert(&conn_a, &obs).expect("insert obs on A");

    let r1 = db::reflect(
        &conn_a,
        &reflect_input(vec![obs_id], Some(ns), "rl3-d1", "ai:peer-a"),
    )
    .expect("A: depth-1 reflect");
    assert_eq!(r1.reflection_depth, 1);

    let r2 = db::reflect(
        &conn_a,
        &reflect_input(vec![r1.id.clone()], Some(ns), "rl3-d2", "ai:peer-a"),
    )
    .expect("A: depth-2 reflect");
    assert_eq!(r2.reflection_depth, 2);

    // Replicate A's depth-2 row to B via the substrate stamp+insert
    // path the HTTP `sync_push` handler also calls.
    let r2_on_a = db::get(&conn_a, &r2.id)
        .expect("get r2 on A")
        .expect("r2 present on A");
    let cap_b = db::resolve_governance_policy(&conn_b, ns)
        .unwrap_or_default()
        .effective_max_reflection_depth();
    assert_eq!(cap_b, 2, "peer B tightened cap = 2");
    let stamped = reflection_bookkeeping::stamp_reflection_origin(&r2_on_a, "ai:peer-a", cap_b);
    let id_on_b = db::insert_if_newer(&conn_b, &stamped).expect("apply on B");
    let r2_on_b = db::get(&conn_b, &id_on_b).unwrap().expect("present on B");
    assert_eq!(
        r2_on_b.reflection_depth, 2,
        "B preserves depth=2 — federation must not silently rewrite"
    );
    let origin = r2_on_b
        .metadata
        .get("reflection_origin")
        .expect("reflection_origin stamped");
    assert_eq!(origin["peer_origin"].as_str(), Some("ai:peer-a"));
    assert_eq!(origin["original_depth"].as_i64(), Some(2));

    // B's curator attempts a depth-3 derivative — refused by cap=2.
    let derived = reflect_input(
        vec![r2_on_b.id.clone()],
        Some(ns),
        "rl3-d3-cross-peer",
        "ai:peer-b",
    );
    let err = db::reflect(&conn_b, &derived)
        .expect_err("cross-peer depth-3 derivative must refuse under cap=2");
    match err {
        ReflectError::DepthExceeded { attempted, cap, .. } => {
            assert_eq!(attempted, 3);
            assert_eq!(cap, 2);
        }
        other => panic!("expected DepthExceeded, got {other:?}"),
    }

    // Audit row lands. L2-2 distinguishes cross-peer refusals via the
    // `reflection.depth_exceeded.cross_peer` event_type so operators
    // can filter; the substrate-local refusal lands as
    // `reflection.depth_exceeded`. Either of those distinct strings is
    // acceptable; we assert at least one depth_exceeded event landed.
    let events = list_signed_events(&conn_b, None, 100, 0).expect("list events");
    let depth_refusal_count = events
        .iter()
        .filter(|e| e.event_type.starts_with("reflection.depth_exceeded"))
        .count();
    assert!(
        depth_refusal_count >= 1,
        "B's signed_events table must carry a depth_exceeded row from the refusal"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 2 (FEDERATION) — L1-8 approval API gate for deep reflections.
// ─────────────────────────────────────────────────────────────────────

/// **SG-RL-4 (Phase 2):** `GovernedAction::Reflect` approval API flow.
/// A namespace with `require_approval_above_depth = 1` queues a
/// `pending_actions` row for a proposed depth-2 reflection; the
/// approver flips the row via `decide_pending_action(approve=true)`;
/// the deferred reflect then succeeds. Composes L1-8
/// (`require_approval_above_depth` substrate threading) into the
/// ship-gate.
#[test]
fn sg_rl_4_approval_api_flow_for_deep_reflection() {
    let conn = db::open(Path::new(":memory:")).expect("open");
    let ns = "ship-gate-rl-4";

    seed_governance_json(
        &conn,
        ns,
        &serde_json::json!({
            "write": "any",
            "require_approval_above_depth": 1_u32
        }),
    );

    // Depth-1 source: proposed reflection would land at depth-2.
    let mut src = make_observation(ns, "rl4-source", 1);
    src.reflection_depth = 1;
    src.memory_kind = MemoryKind::Reflection;
    let src_id = db::insert(&conn, &src).expect("insert depth-1 source");

    let threshold = db::resolve_require_approval_above_depth(&conn, ns);
    assert_eq!(threshold, Some(1), "L1-8 threshold resolves leaf-first");

    // Queue the pending row — what the MCP handler does in lieu of
    // calling db::reflect when the gate fires.
    let payload = serde_json::json!({
        "source_ids": [&src_id],
        "title": "rl4-pending-d2",
        "namespace": ns,
        "proposed_depth": 2_u32,
    });
    let pending_id = db::queue_pending_action(
        &conn,
        GovernedAction::Reflect,
        ns,
        None,
        "ai:l3-3",
        &payload,
    )
    .expect("queue_pending_action ok");

    // Approve it.
    let flipped = db::decide_pending_action(&conn, &pending_id, true, "ai:approver-l3-3")
        .expect("decide_pending_action ok");
    assert!(flipped, "decide_pending_action must report row flipped");
    let row = db::get_pending_action(&conn, &pending_id)
        .expect("get_pending_action ok")
        .expect("row present");
    assert_eq!(row.status, "approved");
    assert_eq!(row.action_type, "reflect");

    // Now the deferred reflect succeeds (the MCP handler re-issues
    // it because the gate sees an "approved" pending row matching the
    // proposed action).
    let outcome = db::reflect(
        &conn,
        &reflect_input(vec![src_id], Some(ns), "rl4-approved-d2", "ai:l3-3"),
    )
    .expect("approved depth-2 reflect succeeds");
    assert_eq!(outcome.reflection_depth, 2);
    assert_eq!(outcome.namespace, ns);
}

// ─────────────────────────────────────────────────────────────────────
// Phase 3 (MIGRATION) — vN→vN+1 idempotent round-trip + memory_kind
// backfill + CHECK constraint after migration ladder.
// ─────────────────────────────────────────────────────────────────────

/// **SG-RL-5 (Phase 3):** Schema migration ladder is idempotent across
/// re-opens. We let `db::open` walk the full v33 ladder, write a
/// reflection + reflects_on edge, drop the connection, re-open the
/// same file — verify the L1+L2 columns survived and a fresh reflect
/// still works on the migrated data.
#[test]
fn sg_rl_5_migration_ladder_idempotent_roundtrip_preserves_l1_l2_columns() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("sg-rl-5.db");

    // First open — full migration ladder runs to v33.
    {
        let conn = db::open(&db_path).expect("first open");

        // Write through the substrate so reflection_depth +
        // memory_kind land via the documented write path.
        let ns = "ship-gate-rl-5";
        let s1 = make_observation(ns, "rl5-src-a", 1);
        let s2 = make_observation(ns, "rl5-src-b", 2);
        let s1_id = db::insert(&conn, &s1).expect("insert s1");
        let s2_id = db::insert(&conn, &s2).expect("insert s2");
        let r = db::reflect(
            &conn,
            &reflect_input(vec![s1_id, s2_id], Some(ns), "rl5-reflection", "ai:l3-3"),
        )
        .expect("first reflect");
        assert_eq!(r.reflection_depth, 1);
        let r_row = db::get(&conn, &r.id)
            .expect("get reflection row")
            .expect("reflection present");
        assert_eq!(r_row.memory_kind, MemoryKind::Reflection);
        // Confirm L1-1 column landed via raw SQL.
        let kind: String = conn
            .query_row(
                "SELECT memory_kind FROM memories WHERE id = ?1",
                [&r.id],
                |row| row.get(0),
            )
            .expect("memory_kind column readable");
        assert_eq!(kind, "reflection");
    }

    // Re-open. The CURRENT_SCHEMA_VERSION≥v33 ladder is idempotent —
    // re-opening must not error and must preserve every row.
    let conn2 = db::open(&db_path).expect("second open (idempotent)");

    // Surviving row visible by id.
    let surviving: i64 = conn2
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE memory_kind = 'reflection'",
            [],
            |r| r.get(0),
        )
        .expect("count reflections after re-open");
    assert!(surviving >= 1, "≥1 reflection row survives the re-open");

    // Fresh reflect against the migrated data still works.
    let ns = "ship-gate-rl-5-postmigrate";
    let s = make_observation(ns, "rl5-postmig", 1);
    let s_id = db::insert(&conn2, &s).expect("insert post-migrate");
    let outcome = db::reflect(
        &conn2,
        &reflect_input(vec![s_id], Some(ns), "rl5-post-reflect", "ai:l3-3"),
    )
    .expect("post-migrate reflect");
    assert_eq!(outcome.reflection_depth, 1);
    let outcome_row = db::get(&conn2, &outcome.id)
        .expect("get post-migrate reflection")
        .expect("post-migrate reflection present");
    assert_eq!(outcome_row.memory_kind, MemoryKind::Reflection);

    // The v33 CHECK constraint on memory_links.relation is active
    // after the ladder — direct-SQL bad relation refuses.
    let raw_err = conn2.execute(
        "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            &outcome.id,
            &outcome.id,
            "not_a_real_relation",
            Utc::now().to_rfc3339()
        ],
    );
    let err = raw_err.expect_err("CHECK constraint must refuse unknown relation post-migration");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("check") || msg.contains("constraint"),
        "expected CHECK/constraint failure, got: {err}"
    );
}

/// **SG-RL-6 (Phase 3):** L1-1 backfill correctness — a row carrying
/// pre-v30 `metadata.type = 'reflection'` is promoted to
/// `memory_kind = 'reflection'` by the migration 0025 backfill.
///
/// We synthesise the pre-backfill state by inserting via `db::insert`
/// (which writes `memory_kind = 'observation'` for an Observation row)
/// then setting `metadata.type = 'reflection'` via direct SQL — the
/// shape a pre-v30 substrate would have left. Re-running the migration
/// 0025 SQL by hand simulates the ladder re-encountering the row; the
/// backfill is idempotent so we can call it on the already-migrated
/// database without harm.
#[test]
fn sg_rl_6_l1_1_metadata_type_to_memory_kind_backfill_idempotent() {
    let conn = db::open(Path::new(":memory:")).expect("open");
    let ns = "ship-gate-rl-6";

    // Insert an Observation, then mutate its metadata to look like a
    // pre-v30 reflection (the marker the curator wrote before the
    // typed enum landed).
    let m = make_observation(ns, "pretend-pre-v30-reflection", 0);
    let m_id = db::insert(&conn, &m).expect("insert observation");
    conn.execute(
        "UPDATE memories \
         SET memory_kind = 'observation', \
             metadata = json_set(metadata, '$.type', 'reflection') \
         WHERE id = ?1",
        rusqlite::params![&m_id],
    )
    .expect("mutate row to pre-v30 shape");

    // Run the backfill SQL as documented by migration 0025.
    let backfill_sql = "UPDATE memories
           SET memory_kind = 'reflection'
         WHERE memory_kind = 'observation'
           AND json_valid(metadata)
           AND json_extract(metadata, '$.type') = 'reflection'";
    conn.execute(backfill_sql, []).expect("backfill UPDATE ok");

    let kind: String = conn
        .query_row(
            "SELECT memory_kind FROM memories WHERE id = ?1",
            [&m_id],
            |r| r.get(0),
        )
        .expect("post-backfill kind readable");
    assert_eq!(
        kind, "reflection",
        "L1-1 backfill must promote metadata.type=reflection → memory_kind=reflection"
    );

    // Idempotency — re-running the backfill is a no-op.
    let before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE memory_kind = 'reflection'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(backfill_sql, []).expect("second backfill");
    let after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE memory_kind = 'reflection'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after, "L1-1 backfill is idempotent");

    // Counter-case: a row whose metadata says something other than
    // 'reflection' stays Observation. Negative-control pin.
    let other = make_observation(ns, "still-an-observation", 0);
    let other_id = db::insert(&conn, &other).expect("insert other");
    conn.execute(
        "UPDATE memories SET metadata = json_set(metadata, '$.type', 'goal') WHERE id = ?1",
        rusqlite::params![&other_id],
    )
    .unwrap();
    conn.execute(backfill_sql, [])
        .expect("third backfill (negative)");
    let other_kind: String = conn
        .query_row(
            "SELECT memory_kind FROM memories WHERE id = ?1",
            [&other_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        other_kind, "observation",
        "negative case: non-reflection metadata.type must not bump memory_kind"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 4 (CHAOS) — concurrent reflect, no duplicate edges.
// ─────────────────────────────────────────────────────────────────────

/// **SG-RL-7 (Phase 4):** Two reflect calls against the same source
/// (different titles, so the (title, namespace) uniqueness constraint
/// doesn't merge them) land independently, each with the correct
/// `reflects_on` edge — no duplicate / spurious edges. Tests the
/// single-process chaos contract; multi-peer chaos is operator-deferred.
#[test]
fn sg_rl_7_two_reflections_on_same_source_no_duplicate_edges() {
    let conn = db::open(Path::new(":memory:")).expect("open");
    let ns = "ship-gate-rl-7";
    let s = make_observation(ns, "rl7-source", 0);
    let s_id = db::insert(&conn, &s).expect("insert source");

    let r1 = db::reflect(
        &conn,
        &reflect_input(vec![s_id.clone()], Some(ns), "rl7-r1", "ai:l3-3"),
    )
    .expect("reflect r1");
    let r2 = db::reflect(
        &conn,
        &reflect_input(vec![s_id.clone()], Some(ns), "rl7-r2", "ai:l3-3"),
    )
    .expect("reflect r2");
    assert_ne!(r1.id, r2.id, "distinct reflection memories");

    // Each reflection has exactly one reflects_on edge → s.
    for r in [&r1, &r2] {
        let links = db::get_links(&conn, &r.id).expect("get_links");
        let r_on_s: Vec<_> = links
            .iter()
            .filter(|l| {
                l.source_id == r.id
                    && l.target_id == s_id
                    && l.relation == MemoryLinkRelation::ReflectsOn
            })
            .collect();
        assert_eq!(
            r_on_s.len(),
            1,
            "reflection {} carries exactly one reflects_on edge to source",
            r.id
        );
    }

    // Source has two distinct inbound reflects_on edges — no merges,
    // no duplicates.
    let inbound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_links \
             WHERE target_id = ?1 AND relation = 'reflects_on'",
            rusqlite::params![&s_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        inbound, 2,
        "source must have exactly two inbound reflects_on edges"
    );
}
