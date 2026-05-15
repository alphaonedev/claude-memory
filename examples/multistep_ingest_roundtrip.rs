// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Cookbook harness for the v0.7.0 Form 3 multi-step ingest orchestrator.
//!
//! Drives the [`IngestExecutor`] directly (no MCP, no daemon, no
//! Ollama) so the cookbook recipe at
//! `cookbook/multistep-ingest/01-two-phase.sh` is reproducible in
//! seconds on any host. The production hot-path uses
//! [`ai_memory::multistep_ingest::executor::OllamaDispatch`] backed by
//! Gemma 4 over Ollama; here we inject a `MockLlmDispatch` with canned
//! responses so the recipe exercises the substrate semantics
//! (deterministic helpers, shared-prefix prompts, prompt-cache key
//! consistency, explicit-trust slots) without an LLM dependency.
//!
//! Audit-honesty note: the mock dispatch is plumbing-only. It pops
//! pre-baked JSON envelopes off a queue; the LLM's downstream
//! reasoning is NOT exercised. The substrate side is faithful: the
//! shared-prefix builder, the trust-slot rendering, and the
//! prompt-cache telemetry all run end-to-end against the real
//! production code paths.

use std::path::PathBuf;
use std::sync::Arc;

use ai_memory::multistep_ingest::{
    IngestExecutor, LlmDispatch, MockLlmDispatch, four_step_default, two_phase_default,
};
use anyhow::{Result, anyhow};

struct Args {
    variant: String,
    report: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut variant = "two_phase".to_string();
    let mut report = None;
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| anyhow!("flag {flag} needs a value"))?;
        match flag.as_str() {
            "--variant" => variant = value,
            "--report" => report = Some(PathBuf::from(value)),
            other => return Err(anyhow!("unknown flag {other}")),
        }
    }
    Ok(Args {
        variant,
        report: report.ok_or_else(|| anyhow!("--report required"))?,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;

    // Pick the variant.
    let (pipeline, responses): (_, Vec<Result<String, String>>) = match args.variant.as_str() {
        "two_phase" => (
            two_phase_default(),
            vec![Ok(
                r#"{"title":"Paris fact","summary":"Paris is the capital of France.","tags":["geography"],"atoms":["Paris is the capital of France."]}"#
                    .to_string(),
            )],
        ),
        "four_step" => (
            four_step_default(),
            vec![
                Ok(r#"{"fact_kind":"declarative","confidence":0.93}"#.to_string()),
                Ok(r#"{"entities":["Paris","France"],"claims":["Paris is the capital of France"],"relations":[]}"#.to_string()),
                Ok(r#"{"title":"Paris capital","summary":"Paris is the capital of France.","tags":["geography"],"proposed_links":[]}"#.to_string()),
            ],
        ),
        other => return Err(anyhow!("unknown --variant {other}; expected two_phase | four_step")),
    };

    let dispatch: Arc<dyn LlmDispatch> = Arc::new(MockLlmDispatch::new(responses));
    let exec = IngestExecutor::new(dispatch);

    let trace = exec
        .run(
            &pipeline,
            "Paris is the capital of France.",
            &[],
            None,
            Some("geography"),
        )
        .map_err(|e| anyhow!("pipeline execution failed: {e}"))?;

    // Acceptance gates. Exit code is non-zero on failure.
    if !trace.prompt_cache_consistent {
        return Err(anyhow!(
            "prompt-cache key drifted across stages within a run: {:?}",
            trace.distinct_cache_keys
        ));
    }
    if trace.stages.is_empty() {
        return Err(anyhow!("pipeline produced zero stages"));
    }

    // Persist the report.
    let report = serde_json::json!({
        "variant": trace.variant,
        "stages_run": trace.stages.len(),
        "distinct_cache_keys": trace.distinct_cache_keys,
        "prompt_cache_consistent": trace.prompt_cache_consistent,
        "final_output": trace.final_output,
    });
    let pretty = serde_json::to_string_pretty(&report)
        .map_err(|e| anyhow!("report serialisation failed: {e}"))?;
    if let Some(parent) = args.report.parent() {
        std::fs::create_dir_all(parent).map_err(|e| anyhow!("create report parent dir: {e}"))?;
    }
    std::fs::write(&args.report, pretty).map_err(|e| anyhow!("write report: {e}"))?;

    eprintln!(
        "multistep-ingest roundtrip OK: variant={} stages={} cache_consistent={} report={}",
        trace.variant,
        trace.stages.len(),
        trace.prompt_cache_consistent,
        args.report.display(),
    );
    Ok(())
}
