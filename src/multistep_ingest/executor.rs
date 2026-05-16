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
use super::helpers::{HelperContext, HelperOutput, MemoryHandle, run_helper_with};
use super::pipeline::{HelperOutputRef, Pipeline, Stage};

/// Default cap on the number of characters of `content` inlined into a
/// single Form 3 multistep-ingest LLM stage (issue #782 PERF-11).
/// Mirrors the synthesis-prompt cap from Cluster B (PERF-7); operators
/// override per-namespace via
/// [`crate::models::GovernancePolicy::multistep_max_content_chars`].
pub const DEFAULT_MULTISTEP_MAX_CONTENT_CHARS: usize = 1500;

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
        /// v0.7.0 polish (issue #782 PERF-11) — number of `content`
        /// bytes the executor surfaced to this helper stage. Helper
        /// stages receive content by **borrow**, so this number is the
        /// size of the same backing string across every helper in the
        /// run — operators inspecting the trace can prove the
        /// content-clone-per-stage regression has not regressed.
        content_bytes: usize,
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
        /// v0.7.0 polish (issue #782 PERF-11) — number of `content`
        /// bytes actually inlined into the LLM prompt **after** the
        /// `multistep_max_content_chars` cap was applied. Lets
        /// operators observe truncation events without diffing the
        /// raw prompt strings.
        content_bytes: usize,
        /// v0.7.0 polish (issue #782 PERF-11) — `true` when the
        /// content was truncated to fit the cap.
        content_truncated: bool,
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
    /// v0.7.0 polish (issue #782 PERF-11) — per-stage content-bytes
    /// histogram. Indexed by stage execution order (matches
    /// `stages[i]`). Helpers report the borrowed-slice length; LLM
    /// stages report the post-truncation length. Operators threading
    /// the trace into Prometheus/Statsd can publish this as a
    /// histogram with one bucket per stage label.
    pub bytes_per_stage: Vec<usize>,
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
    /// v0.7.0 polish (issue #782 PERF-11) — per-LLM-stage content cap.
    /// `None` defers to [`DEFAULT_MULTISTEP_MAX_CONTENT_CHARS`].
    /// Operators set this via [`Self::with_max_content_chars`] after
    /// resolving the per-namespace
    /// [`crate::models::GovernancePolicy::multistep_max_content_chars`].
    max_content_chars: Option<usize>,
    /// v0.7.0 polish (issue #782 PERF-11) — debug-build test seam
    /// recording `content.as_ptr() as usize` for every helper
    /// invocation. Used by the borrow-not-clone acceptance test
    /// (`tests/form_3_multistep_ingest.rs::
    /// multistep_phase_1_helpers_receive_content_borrow_not_clone`)
    /// to prove that the content string is threaded by reference,
    /// not duplicated per helper. Release builds elide the
    /// recording entirely so production paths see zero overhead.
    helper_content_ptrs: Arc<std::sync::Mutex<Vec<usize>>>,
}

impl<D: LlmDispatch + ?Sized> IngestExecutor<D> {
    /// Construct an executor around a dispatch implementation.
    #[must_use]
    pub fn new(dispatch: Arc<D>) -> Self {
        Self {
            dispatch,
            telemetry: Arc::new(PromptCacheTelemetry::new()),
            max_content_chars: None,
            helper_content_ptrs: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Builder-style setter for the per-LLM-stage content cap (issue
    /// #782 PERF-11). Callers resolve the namespace policy via
    /// [`crate::models::GovernancePolicy::effective_multistep_max_content_chars`]
    /// and thread the value here before calling [`Self::run`].
    #[must_use]
    pub fn with_max_content_chars(mut self, cap: usize) -> Self {
        self.max_content_chars = Some(cap);
        self
    }

    /// Telemetry handle. Used by the MCP tool surface to surface the
    /// per-run cache-key trace.
    #[must_use]
    pub fn telemetry(&self) -> Arc<PromptCacheTelemetry> {
        Arc::clone(&self.telemetry)
    }

    /// v0.7.0 polish (issue #782 PERF-11) — debug-build test seam
    /// returning the helper-content pointer recordings from the
    /// most-recent `run()`. The integration test
    /// `multistep_phase_1_helpers_receive_content_borrow_not_clone`
    /// pins the borrow invariant by asserting every entry is the
    /// same pointer.
    ///
    /// Hidden from rustdoc because it is a test seam, not a
    /// production API. The recorder is only populated under
    /// `debug_assertions` (debug builds); release builds return an
    /// empty vec so the call has zero observable overhead in
    /// production.
    #[doc(hidden)]
    #[must_use]
    pub fn helper_content_ptrs(&self) -> Vec<usize> {
        self.helper_content_ptrs
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
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
        let mut bytes_per_stage: Vec<usize> = Vec::with_capacity(pipeline.stages.len());

        // v0.7.0 polish (issue #782 PERF-11): build the borrowed helper
        // context ONCE per run. Every helper stage in Phase 1 receives
        // the SAME `&str` slice — the executor never clones `content`
        // into a per-stage `HelperParams::content`. The
        // `multistep_phase_1_helpers_receive_content_borrow_not_clone`
        // integration test pins this invariant by asserting the
        // pointer recorded for each helper is identical.
        let helper_ctx = HelperContext::new(content, candidates, content_embedding, namespace);
        // v0.7.0 polish (issue #782 PERF-11): record the caller's
        // pointer once so the borrow-not-clone invariant can be
        // observed by the integration test. Debug builds only —
        // release builds skip the recording entirely.
        #[cfg(debug_assertions)]
        let content_ptr_for_test = content.as_ptr() as usize;

        // Phase 1: run every helper stage in declaration order. Helpers
        // are pure functions of their `HelperParams`, so a future
        // optimisation could parallelise them via rayon; for now the
        // serial walk keeps the trace deterministic for tests.
        for (idx, stage) in pipeline.stages.iter().enumerate() {
            if let Stage::Helper { kind, params } = stage {
                #[cfg(debug_assertions)]
                {
                    // Record the pointer of the EFFECTIVE content slice
                    // for the borrow-not-clone acceptance test. The
                    // ctx's `effective_content` returns either the
                    // descriptor override (rare) or the same borrowed
                    // slice across every stage.
                    let effective_ptr = helper_ctx.effective_content(params).as_ptr() as usize;
                    if let Ok(mut g) = self.helper_content_ptrs.lock() {
                        g.push(effective_ptr);
                    }
                    // Pin against accidental drift: if no descriptor
                    // override is present, the pointer MUST equal the
                    // caller's `content.as_ptr()`.
                    if params.content.is_empty() {
                        debug_assert_eq!(effective_ptr, content_ptr_for_test);
                    }
                }
                let out = run_helper_with(*kind, params, &helper_ctx);
                // Helpers see the borrowed slice — the byte count is
                // the size of the SAME backing string across every
                // stage in the run.
                bytes_per_stage.push(content.len());
                stage_outcomes.push(StageOutcome::Helper {
                    index: idx,
                    helper: out.kind.as_str().to_string(),
                    summary: out.summary.clone(),
                    payload: out.payload.clone(),
                    content_bytes: content.len(),
                });
                helper_outputs[idx] = Some(out);
            }
        }

        // Phase 2: walk the LLM stages, assembling the shared-prefix
        // prompt and resolving trust slots against helper_outputs.
        let prefix = build_shared_prefix(pipeline.variant_tag(), &pipeline.system_prompt);
        let cache_key = CacheKey::from_prefix(&prefix);
        let llm_cap = self
            .max_content_chars
            .unwrap_or(DEFAULT_MULTISTEP_MAX_CONTENT_CHARS);

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

            // v0.7.0 polish (issue #782 PERF-11): cap the content
            // inlined into the LLM prompt to `llm_cap` characters
            // (default 1500, mirroring Cluster B's PERF-7 synthesis
            // cap). Truncation only affects the LLM prompt — the
            // helper payloads and the caller-visible final output are
            // untouched. The truncation marker keeps the LLM informed
            // that it's seeing a clipped view so it doesn't
            // hallucinate "completeness" claims.
            let (content_view, truncated) = truncate_content_for_llm(content, llm_cap);

            // Compose the full prompt: shared prefix + stage tail.
            let stage_tail = format!(
                "\n[STAGE label={label} index={idx}]\n\
                 [INCOMING CONTENT]\n{content_view}\n\
                 [TRUST INPUTS]\n{trust_block}\n\
                 [TASK]\n{prompt_template}\n\
                 [OUTPUT SCHEMA]\n{schema}\n",
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

            let content_bytes = content_view.len();
            bytes_per_stage.push(content_bytes);
            stage_outcomes.push(StageOutcome::LlmCall {
                index: idx,
                label: label.clone(),
                prompt,
                cache_key: cache_key.as_hex().to_string(),
                response: response_value.clone(),
                content_bytes,
                content_truncated: truncated,
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
            bytes_per_stage,
        })
    }
}

/// v0.7.0 polish (issue #782 PERF-11) — truncate `content` to at most
/// `cap` characters (codepoint-safe), appending a `[...truncated N
/// chars]` marker when truncation occurred. Returns the rendered view
/// + a flag so the caller can record the truncation event in the
/// trace.
///
/// A `cap` of `0` is treated as "do not truncate"; callers who want to
/// disable the LLM content slot entirely should compose a different
/// prompt template instead.
fn truncate_content_for_llm(content: &str, cap: usize) -> (std::borrow::Cow<'_, str>, bool) {
    use std::fmt::Write as _;
    if cap == 0 {
        return (std::borrow::Cow::Borrowed(content), false);
    }
    let total_chars = content.chars().count();
    if total_chars <= cap {
        return (std::borrow::Cow::Borrowed(content), false);
    }
    let mut truncated: String = content.chars().take(cap).collect();
    // `write!` into a `String` is infallible — discard the error to
    // satisfy clippy::format_push_string.
    let _ = write!(
        truncated,
        " [...truncated {} chars]",
        total_chars.saturating_sub(cap)
    );
    (std::borrow::Cow::Owned(truncated), true)
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
