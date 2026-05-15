// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows: test scaffolding does not need pedantic-clean.
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

//! v0.7.0 Form 3 (issue #756) — multi-step ingest orchestrator
//! acceptance suite.
//!
//! These five tests pin the Batman closeout criteria:
//!
//! 1. Helper-then-LLM stage runs and the LLM call receives helper
//!    output verbatim in its trust slot.
//! 2. The default two-phase pipeline produces a structured envelope.
//! 3. The default four-step pipeline produces a structured envelope.
//! 4. Prompt-cache key is consistent across stages within a run.
//! 5. The explicit-trust instruction appears in every LLM prompt
//!    (string assertion).
//!
//! Every test wires `MockLlmDispatch` so the suite never burns an LLM
//! round-trip; the helper output is deterministic by construction
//! (Jaccard / cosine / FTS classifier are pure functions of their
//! inputs).

use std::sync::Arc;

use ai_memory::multistep_ingest::{
    HelperKind, IngestExecutor, MemoryHandle, MockLlmDispatch, four_step_default, two_phase_default,
};

/// Trust phrase pinned at the substrate level. Lifted from
/// `src/multistep_ingest/cache.rs::EXPLICIT_TRUST_INSTRUCTION` so any
/// drift in the phrasing trips this fixture.
const EXPLICIT_TRUST_PHRASE: &str = "Do NOT re-run discovery. \
The following pre-computed helper output is authoritative; trust it.";

fn handle(id: &str, body: &str) -> MemoryHandle {
    MemoryHandle {
        id: id.to_string(),
        body: body.to_string(),
        embedding: None,
        namespace: None,
    }
}

// ---------------------------------------------------------------------------
// Test 1 — Helper-then-LLM stage chain. The first two stages of the
// default two-phase pipeline are helpers; the third is an LLM call. The
// helper outputs must appear verbatim in the LLM prompt's trust slot.
// ---------------------------------------------------------------------------
#[test]
fn helper_then_llm_runs_helper_output_into_trust_slot() {
    let mock = MockLlmDispatch::new(vec![Ok(
        r#"{"title":"T","summary":"S","tags":[],"atoms":[]}"#.to_string(),
    )]);
    let exec = IngestExecutor::new(Arc::new(mock));
    let pipeline = two_phase_default();
    let trace = exec
        .run(
            &pipeline,
            "the quick brown fox jumps over the lazy dog",
            &[
                handle("c1", "a quick brown fox runs"),
                handle("c2", "lazy dog naps under tree"),
            ],
            None,
            Some("global"),
        )
        .expect("two-phase pipeline runs");

    // Inspect the LLM stage's prompt (last stage).
    let llm_stage = trace
        .stages
        .last()
        .expect("pipeline must have at least one stage");
    let prompt = match llm_stage {
        ai_memory::multistep_ingest::executor::StageOutcome::LlmCall { prompt, .. } => {
            prompt.clone()
        }
        ai_memory::multistep_ingest::executor::StageOutcome::Helper { .. } => {
            panic!("last stage must be an LLM call")
        }
    };

    // The helper kind discriminator must appear in the trust slot.
    assert!(
        prompt.contains("jaccard_overlap"),
        "jaccard_overlap helper output must thread into the LLM prompt; got: {prompt}"
    );
    assert!(
        prompt.contains("fts_classifier"),
        "fts_classifier helper output must thread into the LLM prompt; got: {prompt}"
    );
    // Helper payload markers must appear (Jaccard's `top_candidates`
    // key and FTS classifier's `fact_kind` key).
    assert!(
        prompt.contains("top_candidates"),
        "Jaccard payload key must be rendered verbatim into the trust slot"
    );
    assert!(
        prompt.contains("fact_kind"),
        "FTS classifier payload key must be rendered verbatim into the trust slot"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — Two-phase pipeline produces structured output.
// ---------------------------------------------------------------------------
#[test]
fn two_phase_pipeline_produces_structured_output() {
    let mock = MockLlmDispatch::new(vec![Ok(
        r#"{"title":"Paris fact","summary":"Paris is the capital of France.","tags":["geography"],"atoms":["Paris is the capital of France."]}"#
            .to_string(),
    )]);
    let exec = IngestExecutor::new(Arc::new(mock));
    let pipeline = two_phase_default();
    let trace = exec
        .run(
            &pipeline,
            "Paris is the capital of France.",
            &[],
            None,
            None,
        )
        .expect("two-phase pipeline runs");

    assert_eq!(trace.variant, "two_phase");
    assert_eq!(trace.final_output["title"], "Paris fact");
    assert_eq!(trace.final_output["atoms"].as_array().unwrap().len(), 1);
    assert!(trace.prompt_cache_consistent);
}

// ---------------------------------------------------------------------------
// Test 3 — Four-step pipeline produces structured output.
// ---------------------------------------------------------------------------
#[test]
fn four_step_pipeline_produces_structured_output() {
    let mock = MockLlmDispatch::new(vec![
        Ok(r#"{"fact_kind":"declarative","confidence":0.93}"#.to_string()),
        Ok(r#"{"entities":["Paris","France"],"claims":["Paris is the capital"],"relations":[{"from":"Paris","to":"France","rel":"capital_of"}]}"#.to_string()),
        Ok(r#"{"title":"Paris capital","summary":"Paris is the capital of France.","tags":["geography"],"proposed_links":[]}"#.to_string()),
    ]);
    let exec = IngestExecutor::new(Arc::new(mock));
    let pipeline = four_step_default();
    let trace = exec
        .run(
            &pipeline,
            "Paris is the capital of France.",
            &[],
            None,
            Some("geography"),
        )
        .expect("four-step pipeline runs");

    assert_eq!(trace.variant, "four_step");
    // The final stage is the emit stage; its output drives final_output.
    assert_eq!(trace.final_output["title"], "Paris capital");
    // All three LLM stages must have run.
    let llm_count = trace
        .stages
        .iter()
        .filter(|s| {
            matches!(
                s,
                ai_memory::multistep_ingest::executor::StageOutcome::LlmCall { .. }
            )
        })
        .count();
    assert_eq!(llm_count, 3, "four-step pipeline has exactly 3 LLM stages");
}

// ---------------------------------------------------------------------------
// Test 4 — Prompt-cache key consistent across stages within a run.
// ---------------------------------------------------------------------------
#[test]
fn prompt_cache_key_consistent_across_stages_within_a_run() {
    let mock = MockLlmDispatch::new(vec![
        Ok(r#"{"fact_kind":"declarative","confidence":0.7}"#.to_string()),
        Ok(r#"{"entities":[],"claims":[],"relations":[]}"#.to_string()),
        Ok(r#"{"title":"T","summary":"S","tags":[],"proposed_links":[]}"#.to_string()),
    ]);
    let exec = IngestExecutor::new(Arc::new(mock));
    let telemetry = exec.telemetry();
    let pipeline = four_step_default();
    let trace = exec
        .run(&pipeline, "anything", &[], None, None)
        .expect("ok");
    assert!(
        trace.prompt_cache_consistent,
        "every LLM stage within the run must share the cache key"
    );
    assert_eq!(
        trace.distinct_cache_keys.len(),
        1,
        "single-variant run must produce exactly one distinct cache key; got {:?}",
        trace.distinct_cache_keys
    );
    // Telemetry must record one entry per LLM stage.
    assert_eq!(
        telemetry.len(),
        3,
        "telemetry should hold one record per LLM stage"
    );
    assert!(telemetry.all_keys_match());

    // Each per-stage cache_key in the trace must agree with the
    // distinct set.
    let canonical = &trace.distinct_cache_keys[0];
    for stage in &trace.stages {
        if let ai_memory::multistep_ingest::executor::StageOutcome::LlmCall { cache_key, .. } =
            stage
        {
            assert_eq!(
                cache_key, canonical,
                "stage cache_key must match the canonical cache key"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 5 — Explicit-trust instruction appears verbatim in every LLM
// prompt (string assertion). This is the audit-pinning string from the
// substrate's `EXPLICIT_TRUST_INSTRUCTION` constant.
// ---------------------------------------------------------------------------
#[test]
fn explicit_trust_instruction_appears_in_every_llm_prompt() {
    let mock = MockLlmDispatch::new(vec![
        Ok("{}".to_string()),
        Ok("{}".to_string()),
        Ok("{}".to_string()),
    ]);
    let exec = IngestExecutor::new(Arc::new(mock));
    let pipeline = four_step_default();
    let trace = exec.run(&pipeline, "content", &[], None, None).expect("ok");
    for stage in &trace.stages {
        if let ai_memory::multistep_ingest::executor::StageOutcome::LlmCall { prompt, .. } = stage {
            assert!(
                prompt.contains(EXPLICIT_TRUST_PHRASE),
                "every LLM prompt must carry the explicit-trust phrase verbatim; got: {prompt}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-cut: the MCP tool handler surface forwards the same invariants.
// Confirms the `memory_ingest_multistep` JSON envelope wraps a healthy
// pipeline trace.
// ---------------------------------------------------------------------------
#[test]
fn mcp_tool_handler_returns_consistent_cache_key_envelope() {
    use ai_memory::config::FeatureTier;
    use ai_memory::mcp::tools::{IngestMultistepHandler, handle_ingest_multistep};
    use ai_memory::multistep_ingest::LlmDispatch;
    use serde_json::json;

    let dispatch: Arc<dyn LlmDispatch> = Arc::new(MockLlmDispatch::new(vec![
        Ok(r#"{"fact_kind":"declarative","confidence":0.5}"#.to_string()),
        Ok(r#"{"entities":[],"claims":[],"relations":[]}"#.to_string()),
        Ok(r#"{"title":"T","summary":"S","tags":[],"proposed_links":[]}"#.to_string()),
    ]));
    let handler = IngestMultistepHandler::new(dispatch, FeatureTier::Smart);
    let resp = handle_ingest_multistep(
        &json!({"content": "Paris", "pipeline_variant": "four_step"}),
        Some(&handler),
        FeatureTier::Smart,
    )
    .expect("ok");
    assert_eq!(resp["variant"], "four_step");
    assert_eq!(resp["prompt_cache_consistent"], true);
    assert_eq!(resp["distinct_cache_keys"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Cross-cut: helpers handle empty candidate sets without panicking.
// Pinned because Form 3's executor degrades helper params from the
// pipeline descriptor when the caller doesn't supply them.
// ---------------------------------------------------------------------------
#[test]
fn helpers_degrade_cleanly_with_empty_candidates_and_namespace() {
    let mock = MockLlmDispatch::new(vec![Ok("{}".to_string())]);
    let exec = IngestExecutor::new(Arc::new(mock));
    let pipeline = two_phase_default();
    let trace = exec
        .run(&pipeline, "Step 1: do X. Then do Y.", &[], None, None)
        .expect("pipeline runs cleanly with no candidates");

    // Helper outputs landed in stages[0..2].
    let helper_stage_count = trace
        .stages
        .iter()
        .filter(|s| {
            matches!(
                s,
                ai_memory::multistep_ingest::executor::StageOutcome::Helper { .. }
            )
        })
        .count();
    assert_eq!(helper_stage_count, 2);
    // FTS classifier must have labelled it procedural.
    let fts = trace
        .stages
        .iter()
        .find_map(|s| match s {
            ai_memory::multistep_ingest::executor::StageOutcome::Helper {
                helper, payload, ..
            } if helper == "fts_classifier" => Some(payload),
            _ => None,
        })
        .expect("fts_classifier stage present");
    assert_eq!(fts["fact_kind"], "procedural");
}

// ---------------------------------------------------------------------------
// Cross-cut: helper kinds round-trip through the public surface.
// Sanity that `HelperKind::as_str()` is in sync with the trace's
// `helper` field.
// ---------------------------------------------------------------------------
#[test]
fn helper_kind_str_matches_trace_helper_field() {
    let mock = MockLlmDispatch::new(vec![Ok("{}".to_string())]);
    let exec = IngestExecutor::new(Arc::new(mock));
    let pipeline = two_phase_default();
    let trace = exec.run(&pipeline, "content", &[], None, None).expect("ok");
    let helper_names: Vec<String> = trace
        .stages
        .iter()
        .filter_map(|s| match s {
            ai_memory::multistep_ingest::executor::StageOutcome::Helper { helper, .. } => {
                Some(helper.clone())
            }
            ai_memory::multistep_ingest::executor::StageOutcome::LlmCall { .. } => None,
        })
        .collect();
    // The first two stages of the two-phase default are FTS + Jaccard.
    assert!(helper_names.contains(&HelperKind::FtsClassifier.as_str().to_string()));
    assert!(helper_names.contains(&HelperKind::JaccardOverlap.as_str().to_string()));
}
