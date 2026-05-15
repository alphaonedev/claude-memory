// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Form 3 — pipeline executor.
//!
//! Threads the deterministic helpers through their stages first
//! (parallel-where-independent), then dispatches the LLM stages with
//! the shared-prefix prompt assembled in [`super::cache`]. Trust slots
//! are resolved against the in-flight stage outputs and rendered into
//! the LLM prompt verbatim, so the explicit-trust contract holds end-
//! to-end.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::cache::{
    CacheKey, EXPLICIT_TRUST_INSTRUCTION, PromptCacheTelemetry, build_shared_prefix,
};
#[cfg(test)]
use super::helpers::HelperParams;
use super::helpers::{HelperOutput, MemoryHandle, run_helper};
use super::pipeline::{HelperOutputRef, Pipeline, Stage};

/// Per-stage trace entry produced by the executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage_type", rename_all = "snake_case")]
pub enum StageOutcome {
    /// A deterministic helper stage.
    Helper {
        /// Stage index in the pipeline.
        index: usize,
        /// Snake-case helper discriminator (matches
        /// `HelperKind::as_str`).
        helper: String,
        /// Helper's one-line summary (operator-facing).
        summary: String,
        /// Structured payload threaded into downstream LLM stages.
        payload: Value,
    },
    /// An LLM call stage.
    LlmCall {
        /// Stage index in the pipeline.
        index: usize,
        /// Stage label from the descriptor.
        label: String,
        /// Prompt string sent to the LLM (shared prefix + trust slots
        /// + per-stage body). Included verbatim so test assertions can
        /// check for the explicit-trust phrase.
        prompt: String,
        /// Prompt-cache key derived from the shared prefix.
        cache_key: String,
        /// LLM response — parsed as JSON when the response was JSON,
        /// or wrapped in `{"raw": "..."}` when the LLM returned text.
        response: Value,
    },
}

/// Full execution trace returned by [`IngestExecutor::run`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTrace {
    /// Pipeline variant tag (`"two_phase"` / `"four_step"`).
    pub variant: String,
    /// Stage-by-stage outcomes in execution order.
    pub stages: Vec<StageOutcome>,
    /// Distinct cache keys observed across LLM stages. Form 3's
    /// acceptance criterion is that this set has length 1 (or 0 when
    /// the pipeline has no LLM stages).
    pub distinct_cache_keys: Vec<String>,
    /// `true` when every LLM stage shared the same cache key.
    pub prompt_cache_consistent: bool,
    /// Final structured output emitted by the last LLM stage, OR the
    /// last helper stage if the pipeline had no LLM stages.
    pub final_output: Value,
}

/// Structured error surface for the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorError {
    /// A trust slot pointed at a stage index that hasn't run yet, or
    /// at a non-helper stage.
    InvalidTrustSlot {
        /// Stage index the trust slot pointed at.
        stage_index: usize,
        /// Label of the trust slot that failed to resolve.
        label: String,
    },
    /// The LLM dispatch returned an error.
    LlmDispatch(String),
    /// Pipeline descriptor had no stages — nothing to execute.
    EmptyPipeline,
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTrustSlot { stage_index, label } => write!(
                f,
                "invalid trust slot: stage_index={stage_index} (label={label})"
            ),
            Self::LlmDispatch(msg) => write!(f, "llm dispatch failed: {msg}"),
            Self::EmptyPipeline => write!(f, "pipeline has no stages"),
        }
    }
}

impl std::error::Error for ExecutorError {}

/// LLM dispatch trait — abstracted so tests can wire a deterministic
/// mock while production binds to `OllamaClient::generate` via
/// [`OllamaDispatch`].
pub trait LlmDispatch: Send + Sync {
    /// Dispatch a single LLM call. The prompt carries the full
    /// shared-prefix + trust slots + stage body; the executor has
    /// already recorded the cache key.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` when the underlying LLM call fails — the
    /// executor maps that into [`ExecutorError::LlmDispatch`].
    fn dispatch(&self, prompt: &str) -> Result<String, String>;
}

/// Production binding to the project's `OllamaClient::generate`. Wraps
/// the existing breaker / timeout discipline.
pub struct OllamaDispatch {
    client: Arc<crate::llm::OllamaClient>,
}

impl OllamaDispatch {
    /// Construct a production dispatch around an existing `OllamaClient`.
    #[must_use]
    pub fn new(client: Arc<crate::llm::OllamaClient>) -> Self {
        Self { client }
    }
}

impl LlmDispatch for OllamaDispatch {
    fn dispatch(&self, prompt: &str) -> Result<String, String> {
        self.client
            .generate(prompt, None)
            .map_err(|e| e.to_string())
    }
}

/// Deterministic mock dispatch used by the test suite and the
/// cookbook demo. Pops canned responses off a queue; returns
/// `Err("mock: queue exhausted")` once empty so tests catch over-call
/// bugs.
pub struct MockLlmDispatch {
    responses: std::sync::Mutex<Vec<Result<String, String>>>,
}

impl MockLlmDispatch {
    /// Construct a mock dispatch with a canned response queue.
    #[must_use]
    pub fn new(responses: Vec<Result<String, String>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

impl LlmDispatch for MockLlmDispatch {
    fn dispatch(&self, _prompt: &str) -> Result<String, String> {
        let mut q = self.responses.lock().expect("mutex not poisoned in tests");
        if q.is_empty() {
            return Err("mock: queue exhausted".to_string());
        }
        q.remove(0)
    }
}

/// The orchestrator. Walks a [`Pipeline`] start-to-finish, runs
/// helpers up front (parallel-where-independent), threads outputs into
/// LLM stages with explicit-trust slots, and returns an
/// [`ExecutionTrace`].
pub struct IngestExecutor<D: LlmDispatch + ?Sized> {
    dispatch: Arc<D>,
    telemetry: Arc<PromptCacheTelemetry>,
}

impl<D: LlmDispatch + ?Sized> IngestExecutor<D> {
    /// Construct an executor around a dispatch implementation.
    #[must_use]
    pub fn new(dispatch: Arc<D>) -> Self {
        Self {
            dispatch,
            telemetry: Arc::new(PromptCacheTelemetry::new()),
        }
    }

    /// Telemetry handle. Used by the MCP tool surface to surface the
    /// per-run cache-key trace.
    #[must_use]
    pub fn telemetry(&self) -> Arc<PromptCacheTelemetry> {
        Arc::clone(&self.telemetry)
    }

    /// Run a pipeline against an incoming content blob + candidate
    /// memory set.
    ///
    /// # Errors
    ///
    /// - [`ExecutorError::EmptyPipeline`] if the descriptor has no
    ///   stages.
    /// - [`ExecutorError::InvalidTrustSlot`] if an LLM stage references
    ///   a stage index that hasn't run yet or doesn't refer to a
    ///   helper.
    /// - [`ExecutorError::LlmDispatch`] if the underlying LLM call
    ///   fails.
    pub fn run(
        &self,
        pipeline: &Pipeline,
        content: &str,
        candidates: &[MemoryHandle],
        content_embedding: Option<&[f32]>,
        namespace: Option<&str>,
    ) -> Result<ExecutionTrace, ExecutorError> {
        if pipeline.stages.is_empty() {
            return Err(ExecutorError::EmptyPipeline);
        }

        let mut helper_outputs: Vec<Option<HelperOutput>> = vec![None; pipeline.stages.len()];
        let mut stage_outcomes: Vec<StageOutcome> = Vec::with_capacity(pipeline.stages.len());

        // Phase 1: run every helper stage in declaration order. Helpers
        // are pure functions of their `HelperParams`, so a future
        // optimisation could parallelise them via rayon; for now the
        // serial walk keeps the trace deterministic for tests.
        for (idx, stage) in pipeline.stages.iter().enumerate() {
            if let Stage::Helper { kind, params } = stage {
                let mut effective = params.clone();
                if effective.content.is_empty() {
                    effective.content = content.to_string();
                }
                if effective.candidates.is_empty() {
                    effective.candidates = candidates.to_vec();
                }
                if effective.content_embedding.is_none() {
                    effective.content_embedding = content_embedding.map(<[f32]>::to_vec);
                }
                if effective.namespace.is_none() {
                    effective.namespace = namespace.map(str::to_string);
                }
                let out = run_helper(*kind, &effective);
                stage_outcomes.push(StageOutcome::Helper {
                    index: idx,
                    helper: out.kind.as_str().to_string(),
                    summary: out.summary.clone(),
                    payload: out.payload.clone(),
                });
                helper_outputs[idx] = Some(out);
            }
        }

        // Phase 2: walk the LLM stages, assembling the shared-prefix
        // prompt and resolving trust slots against helper_outputs.
        let prefix = build_shared_prefix(pipeline.variant_tag(), &pipeline.system_prompt);
        let cache_key = CacheKey::from_prefix(&prefix);

        let mut last_llm_response: Option<Value> = None;

        for (idx, stage) in pipeline.stages.iter().enumerate() {
            let Stage::LlmCall {
                prompt_template,
                trust_inputs,
                output_schema,
                label,
            } = stage
            else {
                continue;
            };

            // Build the trust-slot block.
            let trust_block = render_trust_inputs(trust_inputs, &helper_outputs)?;

            // Compose the full prompt: shared prefix + stage tail.
            let stage_tail = format!(
                "\n[STAGE label={label} index={idx}]\n\
                 [INCOMING CONTENT]\n{content}\n\
                 [TRUST INPUTS]\n{trust_block}\n\
                 [TASK]\n{prompt_template}\n\
                 [OUTPUT SCHEMA]\n{schema}\n",
                content = content,
                schema = serde_json::to_string(output_schema).unwrap_or_else(|_| "{}".to_string()),
            );
            let prompt = format!("{prefix}{stage_tail}");

            // Telemetry: cache key is the same for every LLM stage in
            // the run because the prefix is identical.
            self.telemetry.record(cache_key.clone());

            let response_text = self
                .dispatch
                .dispatch(&prompt)
                .map_err(ExecutorError::LlmDispatch)?;

            let response_value = match serde_json::from_str::<Value>(&response_text) {
                Ok(v) => v,
                Err(_) => json!({ "raw": response_text }),
            };

            stage_outcomes.push(StageOutcome::LlmCall {
                index: idx,
                label: label.clone(),
                prompt,
                cache_key: cache_key.as_hex().to_string(),
                response: response_value.clone(),
            });
            last_llm_response = Some(response_value);
        }

        let distinct_cache_keys: Vec<String> = {
            let mut seen: Vec<String> =
                self.telemetry.snapshot().into_iter().map(|k| k.0).collect();
            seen.sort();
            seen.dedup();
            seen
        };
        let prompt_cache_consistent = self.telemetry.all_keys_match();

        // Choose the final output: last LLM response if any, else the
        // last helper payload.
        let final_output = last_llm_response.unwrap_or_else(|| {
            helper_outputs
                .iter()
                .rev()
                .find_map(|o| o.as_ref().map(|h| h.payload.clone()))
                .unwrap_or_else(|| json!({}))
        });

        Ok(ExecutionTrace {
            variant: pipeline.variant_tag().to_string(),
            stages: stage_outcomes,
            distinct_cache_keys,
            prompt_cache_consistent,
            final_output,
        })
    }
}

/// Render the trust-slot block for an LLM stage's prompt. Each slot's
/// label and payload appears under the explicit-trust banner so the
/// LLM sees the same instruction every stage.
fn render_trust_inputs(
    inputs: &[HelperOutputRef],
    helper_outputs: &[Option<HelperOutput>],
) -> Result<String, ExecutorError> {
    if inputs.is_empty() {
        return Ok(format!("(none — but: {EXPLICIT_TRUST_INSTRUCTION})"));
    }
    let mut out = String::new();
    out.push_str(EXPLICIT_TRUST_INSTRUCTION);
    out.push_str("\n\n");
    for input in inputs {
        let payload = helper_outputs
            .get(input.stage_index)
            .and_then(|o| o.as_ref())
            .ok_or_else(|| ExecutorError::InvalidTrustSlot {
                stage_index: input.stage_index,
                label: input.label.clone(),
            })?;
        out.push_str(&format!(
            "<<TRUST label={} helper={}>>\n{}\n<<END TRUST>>\n\n",
            input.label,
            payload.kind.as_str(),
            serde_json::to_string_pretty(&payload.payload).unwrap_or_default()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multistep_ingest::pipeline::{four_step_default, two_phase_default};

    fn mh(id: &str, body: &str) -> MemoryHandle {
        MemoryHandle {
            id: id.to_string(),
            body: body.to_string(),
            embedding: None,
            namespace: None,
        }
    }

    #[test]
    fn helper_then_llm_runs_in_order_and_renders_trust_slot() {
        let mock = MockLlmDispatch::new(vec![Ok(
            r#"{"title":"T","summary":"S","tags":[],"atoms":[]}"#.to_string(),
        )]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let pipeline = two_phase_default();
        let trace = exec
            .run(
                &pipeline,
                "the quick brown fox",
                &[mh("c1", "a quick fox")],
                None,
                Some("global"),
            )
            .expect("pipeline runs");

        // First two stages are helpers; third is the LLM call.
        assert!(matches!(trace.stages[0], StageOutcome::Helper { .. }));
        assert!(matches!(trace.stages[1], StageOutcome::Helper { .. }));
        assert!(matches!(trace.stages[2], StageOutcome::LlmCall { .. }));

        // The LLM prompt must carry the explicit-trust instruction.
        if let StageOutcome::LlmCall { prompt, .. } = &trace.stages[2] {
            assert!(
                prompt.contains(EXPLICIT_TRUST_INSTRUCTION),
                "LLM prompt must carry the explicit-trust instruction verbatim"
            );
            assert!(
                prompt.contains("jaccard_overlap") || prompt.contains("fts_classifier"),
                "LLM prompt must cite a helper kind from the trust slots"
            );
        } else {
            panic!("stage 2 must be an LLM call");
        }
    }

    #[test]
    fn two_phase_pipeline_produces_structured_output() {
        let mock = MockLlmDispatch::new(vec![Ok(
            r#"{"title":"T","summary":"S","tags":["a"],"atoms":["one","two"]}"#.to_string(),
        )]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let pipeline = two_phase_default();
        let trace = exec
            .run(&pipeline, "anything", &[], None, None)
            .expect("ok");
        assert_eq!(trace.variant, "two_phase");
        assert_eq!(trace.final_output["title"], "T");
        assert_eq!(trace.final_output["atoms"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn four_step_pipeline_produces_structured_output() {
        let mock = MockLlmDispatch::new(vec![
            Ok(r#"{"fact_kind":"declarative","confidence":0.9}"#.to_string()),
            Ok(r#"{"entities":["a"],"claims":["c"],"relations":[]}"#.to_string()),
            Ok(r#"{"title":"X","summary":"Y","tags":[],"proposed_links":[]}"#.to_string()),
        ]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let pipeline = four_step_default();
        let trace = exec
            .run(
                &pipeline,
                "Paris is the capital of France.",
                &[],
                None,
                None,
            )
            .expect("ok");
        assert_eq!(trace.variant, "four_step");
        // Three LLM stages → three entries in the cache-key trace.
        let llm_count = trace
            .stages
            .iter()
            .filter(|s| matches!(s, StageOutcome::LlmCall { .. }))
            .count();
        assert_eq!(llm_count, 3);
        // Final output is the emit stage's response.
        assert_eq!(trace.final_output["title"], "X");
    }

    #[test]
    fn prompt_cache_key_is_consistent_across_stages_within_a_run() {
        let mock = MockLlmDispatch::new(vec![
            Ok(r#"{"fact_kind":"declarative","confidence":0.5}"#.to_string()),
            Ok(r#"{"entities":[],"claims":[],"relations":[]}"#.to_string()),
            Ok(r#"{"title":"T","summary":"S","tags":[],"proposed_links":[]}"#.to_string()),
        ]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let pipeline = four_step_default();
        let trace = exec.run(&pipeline, "content", &[], None, None).expect("ok");
        assert!(
            trace.prompt_cache_consistent,
            "every LLM stage within a run must share the cache key"
        );
        assert_eq!(
            trace.distinct_cache_keys.len(),
            1,
            "exactly one distinct cache key for a single-variant run"
        );
    }

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
            if let StageOutcome::LlmCall { prompt, .. } = stage {
                assert!(
                    prompt.contains(EXPLICIT_TRUST_INSTRUCTION),
                    "every LLM prompt must carry the explicit-trust phrase"
                );
            }
        }
    }

    #[test]
    fn empty_pipeline_returns_structured_error() {
        let mock = MockLlmDispatch::new(vec![]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let pipeline = Pipeline {
            variant: super::super::pipeline::PipelineVariant::TwoPhase,
            stages: vec![],
            system_prompt: String::new(),
        };
        let err = exec
            .run(&pipeline, "x", &[], None, None)
            .expect_err("empty pipeline should error");
        assert!(matches!(err, ExecutorError::EmptyPipeline));
    }

    #[test]
    fn helper_only_pipeline_uses_last_helper_payload_as_final_output() {
        let mock = MockLlmDispatch::new(vec![]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let pipeline = Pipeline {
            variant: super::super::pipeline::PipelineVariant::TwoPhase,
            stages: vec![Stage::Helper {
                kind: super::super::helpers::HelperKind::FtsClassifier,
                params: HelperParams::default(),
            }],
            system_prompt: String::new(),
        };
        let trace = exec
            .run(&pipeline, "first, do X. then do Y.", &[], None, None)
            .expect("ok");
        assert_eq!(trace.final_output["helper"], "fts_classifier");
        assert_eq!(trace.final_output["fact_kind"], "procedural");
    }

    #[test]
    fn invalid_trust_slot_index_returns_structured_error() {
        let mock = MockLlmDispatch::new(vec![Ok("{}".to_string())]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let pipeline = Pipeline {
            variant: super::super::pipeline::PipelineVariant::TwoPhase,
            stages: vec![Stage::LlmCall {
                prompt_template: "anything".to_string(),
                trust_inputs: vec![HelperOutputRef {
                    stage_index: 99,
                    label: "missing".to_string(),
                }],
                output_schema: json!({}),
                label: "broken".to_string(),
            }],
            system_prompt: "x".to_string(),
        };
        let err = exec
            .run(&pipeline, "y", &[], None, None)
            .expect_err("invalid trust slot must error");
        assert!(matches!(err, ExecutorError::InvalidTrustSlot { .. }));
    }

    #[test]
    fn telemetry_records_one_key_per_llm_stage() {
        let mock = MockLlmDispatch::new(vec![
            Ok("{}".to_string()),
            Ok("{}".to_string()),
            Ok("{}".to_string()),
        ]);
        let exec = IngestExecutor::new(Arc::new(mock));
        let telemetry = exec.telemetry();
        let pipeline = four_step_default();
        exec.run(&pipeline, "content", &[], None, None).unwrap();
        assert_eq!(telemetry.len(), 3, "four-step has 3 LLM stages");
        assert!(telemetry.all_keys_match());
    }
}
