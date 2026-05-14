// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (integration test): pedantic lints with no behavioural impact.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

//! v0.7.0 Layer 3 Task L3-1 — LongMemEval-Reflection bench unit tests.
//!
//! Pinning tests for the bench dataset generator + runner. Run under
//! the standard `cargo test` gate so the smoke-run + determinism gates
//! are exercised on every PR alongside the rest of the test suite.
//!
//! These tests do NOT run inside `cargo bench --bench
//! longmemeval_reflection` because that bench uses `harness = false`
//! (so there's no test runner inside the bench binary). Co-locating
//! them here ensures the unit-test gate covers them.

use ai_memory::models::MemoryKind;

#[path = "../benchmarks/longmemeval_reflection/dataset.rs"]
mod dataset;

#[path = "../benchmarks/longmemeval_reflection/runner.rs"]
mod runner;

use dataset::{
    OBSERVATIONS_PER_SCENARIO, SCENARIO_COUNT, generate_scenarios, load_jsonl, serialise_jsonl,
};
use runner::{DeterministicJudge, DeterministicLlmStub, LlmJudge, run};

/// Determinism gate: regenerating the dataset with the fixed seed
/// MUST yield the same scenarios in the same order. A regression here
/// means the snapshot is stale and the audit will diverge.
#[test]
fn dataset_is_deterministic() {
    let a = generate_scenarios();
    let b = generate_scenarios();
    assert_eq!(a.len(), SCENARIO_COUNT);
    assert_eq!(b.len(), SCENARIO_COUNT);
    for (sa, sb) in a.iter().zip(b.iter()) {
        assert_eq!(sa.id, sb.id);
        assert_eq!(sa.topic, sb.topic);
        assert_eq!(sa.ground_truth_depth_1, sb.ground_truth_depth_1);
        assert_eq!(sa.ground_truth_depth_2, sb.ground_truth_depth_2);
        assert_eq!(sa.siblings, sb.siblings);
        assert_eq!(sa.observations.len(), OBSERVATIONS_PER_SCENARIO);
        for (oa, ob) in sa.observations.iter().zip(sb.observations.iter()) {
            assert_eq!(oa.id, ob.id);
            assert_eq!(oa.title, ob.title);
            assert_eq!(oa.content, ob.content);
            assert_eq!(oa.created_at, ob.created_at);
            assert_eq!(oa.memory_kind, MemoryKind::Observation);
        }
    }
}

/// Every depth-1 ground truth must mention the scenario topic — the
/// CI judge depends on this for its token-Jaccard signal.
#[test]
fn depth_1_ground_truth_mentions_topic() {
    for s in generate_scenarios() {
        assert!(
            s.ground_truth_depth_1.contains(&s.topic),
            "scenario {} depth-1 ground truth missing topic '{}'",
            s.id,
            s.topic
        );
    }
}

/// JSONL round-trip preserves scenario shape.
#[test]
fn jsonl_roundtrip() {
    let scenarios = generate_scenarios();
    let jsonl = serialise_jsonl(&scenarios);
    let parsed = load_jsonl(&jsonl).expect("parse");
    assert_eq!(parsed.len(), scenarios.len());
    assert_eq!(parsed[0].id, scenarios[0].id);
    assert_eq!(parsed[0].topic, scenarios[0].topic);
    assert_eq!(
        parsed[SCENARIO_COUNT - 1].id,
        scenarios[SCENARIO_COUNT - 1].id
    );
}

/// The deterministic judge gives a perfect score when the candidate
/// is the ground truth verbatim, and zero when there's no token
/// overlap.
#[test]
fn judge_endpoints() {
    let j = DeterministicJudge::default();
    let (m, s) = j.score("same", "same");
    assert!(m);
    assert!((s - 1.0).abs() < f64::EPSILON);
    let (m, s) = j.score("alpha", "omega");
    assert!(!m);
    assert!(s.abs() < f64::EPSILON);
}

/// End-to-end: deterministic stub + judge → every spec gate passes on
/// a six-scenario smoke run. This is the CI signal for L3-1.
#[test]
fn smoke_run_meets_targets() {
    let scenarios = generate_scenarios();
    let llm = DeterministicLlmStub::from_scenarios(&scenarios);
    let judge = DeterministicJudge::default();
    let report = run(&scenarios, &llm, &judge, true).expect("smoke run");
    match report.check_targets() {
        Ok(()) => {}
        Err(fails) => panic!("smoke run failed gates: {fails:?}\nreport={report:#?}"),
    }
}

/// Snapshot equivalence: the committed `data/scenarios.jsonl` must
/// match the in-memory generator byte-for-byte. A divergence means
/// either the seed changed without regen, or the dataset code drifted
/// without re-snapshotting — both are audit failures.
#[test]
fn snapshot_matches_generator() {
    let snapshot_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benchmarks")
        .join("longmemeval_reflection")
        .join("data")
        .join("scenarios.jsonl");
    let on_disk = std::fs::read_to_string(&snapshot_path).unwrap_or_else(|e| {
        panic!(
            "snapshot not found at {} — regenerate with `cargo bench --bench longmemeval_reflection -- --regenerate` ({})",
            snapshot_path.display(),
            e
        )
    });
    let in_memory = serialise_jsonl(&generate_scenarios());
    assert_eq!(
        on_disk, in_memory,
        "scenarios.jsonl drift — regenerate via `cargo bench --bench longmemeval_reflection -- --regenerate`"
    );
}
