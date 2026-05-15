// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 3 (issue #756) — `memory_ingest_multistep` MCP tool.
//!
//! Surfaces the multi-step ingest orchestrator
//! ([`crate::multistep_ingest`]) at the Family::Power tier. Tier-gated
//! to smart+ — the keyword and semantic tiers short-circuit with a
//! tier-locked advisory envelope, matching the convention from
//! `memory_atomise` / `memory_consolidate`.
//!
//! # Tool contract
//!
//! Input arguments:
//!
//! - `content` (string, required) — the content to ingest.
//! - `namespace` (string, optional) — routing hint for the FTS
//!   classifier helper. Default `"global"`.
//! - `pipeline_variant` (string, optional, default `"two_phase"`) —
//!   one of `"two_phase"` | `"four_step"`. Picks the default
//!   pipeline.
//! - `pipeline_override` (object, optional) — full
//!   [`Pipeline`](crate::multistep_ingest::Pipeline) JSON. Overrides
//!   `pipeline_variant` when both are present.
//!
//! Output JSON envelope:
//!
//! ```json
//! {
//!   "variant": "two_phase",
//!   "stages": [ ... per-stage trace ... ],
//!   "distinct_cache_keys": ["<hex>"],
//!   "prompt_cache_consistent": true,
//!   "final_output": { ... },
//!   "ingested_memory_ids": []
//! }
//! ```
//!
//! `ingested_memory_ids` is reserved for the follow-up wave that wires
//! the substrate `memory_store` writer behind a Form 3 emit-stage
//! dispatcher. For the initial Form 3 closeout, the tool returns the
//! structured pipeline trace + final output so operators and
//! downstream automation can route the synthesis result themselves.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::FeatureTier;
#[cfg(test)]
use crate::multistep_ingest::MockLlmDispatch;
use crate::multistep_ingest::{
    IngestExecutor, LlmDispatch, Pipeline, PipelineVariant, four_step_default, two_phase_default,
};

/// Handler bundle. Keeps the dispatch implementation behind an `Arc<dyn
/// LlmDispatch>` so the daemon-runtime side can construct it once at
/// MCP boot and re-use across calls. The dispatch is `None` until the
/// daemon wires an LLM client (semantic-tier and below); the tier gate
/// in [`handle_ingest_multistep`] short-circuits before consulting the
/// dispatch in that case.
pub struct IngestMultistepHandler {
    /// LLM dispatch — production binding via
    /// [`crate::multistep_ingest::executor::OllamaDispatch`]; a mock
    /// queue under tests.
    pub dispatch: Arc<dyn LlmDispatch>,
    /// Daemon's resolved feature tier. Retained as defense-in-depth so
    /// callers outside the MCP path still have it available.
    #[allow(dead_code)]
    pub tier: FeatureTier,
}

impl IngestMultistepHandler {
    /// Construct a handler with the supplied dispatch + tier.
    #[must_use]
    pub fn new(dispatch: Arc<dyn LlmDispatch>, tier: FeatureTier) -> Self {
        Self { dispatch, tier }
    }
}

/// Required-tier label for the tier-locked advisory envelope.
const REQUIRED_TIER: &str = "smart";

/// Handle a `memory_ingest_multistep` MCP tool call.
///
/// # Arguments
///
/// - `params` — JSON-RPC `arguments` object.
/// - `handler` — pre-built handler bundle, or `None` when the daemon
///   has no LLM wired (collapses to the tier-locked advisory).
/// - `tier` — fallback tier for the advisory envelope when `handler`
///   is `None`.
///
/// # Errors
///
/// Returns `Err(String)` on input validation failure or pipeline
/// execution failure; the dispatcher wraps the string into the MCP
/// `isError: true` envelope.
pub fn handle_ingest_multistep(
    params: &Value,
    handler: Option<&IngestMultistepHandler>,
    tier: FeatureTier,
) -> Result<Value, String> {
    // ── Argument validation ─────────────────────────────────────────
    let content = params
        .get("content")
        .ok_or("content is required")?
        .as_str()
        .ok_or("content must be a string")?;
    if content.is_empty() {
        return Err("content must not be empty".to_string());
    }

    let namespace = params
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("global");

    // ── Tier gate ───────────────────────────────────────────────────
    if tier == FeatureTier::Keyword || handler.is_none() {
        return Ok(json!({
            "tier-locked": "memory_ingest_multistep requires smart tier or higher",
            "current_tier": tier.as_str(),
            "required_tier": REQUIRED_TIER,
        }));
    }
    let handler = handler.expect("checked above");

    // ── Pipeline resolution ─────────────────────────────────────────
    let pipeline = if let Some(override_value) = params.get("pipeline_override") {
        if !override_value.is_null() {
            serde_json::from_value::<Pipeline>(override_value.clone())
                .map_err(|e| format!("pipeline_override is malformed: {e}"))?
        } else {
            resolve_variant(params)?
        }
    } else {
        resolve_variant(params)?
    };

    // ── Execute ────────────────────────────────────────────────────
    let executor: IngestExecutor<dyn LlmDispatch> =
        IngestExecutor::new(Arc::clone(&handler.dispatch));
    let trace = executor
        .run(&pipeline, content, &[], None, Some(namespace))
        .map_err(|e| format!("INGEST_MULTISTEP_FAILED: {e}"))?;

    Ok(json!({
        "variant": trace.variant,
        "stages": trace.stages,
        "distinct_cache_keys": trace.distinct_cache_keys,
        "prompt_cache_consistent": trace.prompt_cache_consistent,
        "final_output": trace.final_output,
        "ingested_memory_ids": Vec::<String>::new(),
    }))
}

fn resolve_variant(params: &Value) -> Result<Pipeline, String> {
    let variant_tag = params
        .get("pipeline_variant")
        .and_then(Value::as_str)
        .unwrap_or("two_phase");
    let variant = PipelineVariant::from_str(variant_tag).ok_or_else(|| {
        format!(
            "pipeline_variant must be one of \"two_phase\" | \"four_step\"; got {variant_tag:?}"
        )
    })?;
    Ok(match variant {
        PipelineVariant::TwoPhase => two_phase_default(),
        PipelineVariant::FourStep => four_step_default(),
    })
}

/// Test-only helper: build a handler bundle with a `MockLlmDispatch`
/// pre-loaded with the supplied canned responses. Exposed under
/// `cfg(test)` so the integration suite at
/// `tests/form_3_multistep_ingest.rs` can drive the handler without
/// spinning up a real `OllamaClient`.
#[cfg(test)]
pub(crate) fn handler_with_mock_responses(
    responses: Vec<Result<String, String>>,
    tier: FeatureTier,
) -> IngestMultistepHandler {
    let dispatch: Arc<dyn LlmDispatch> = Arc::new(MockLlmDispatch::new(responses));
    IngestMultistepHandler::new(dispatch, tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_content_errors() {
        let err = handle_ingest_multistep(&json!({}), None, FeatureTier::Smart).unwrap_err();
        assert!(err.contains("content is required"), "got: {err}");
    }

    #[test]
    fn non_string_content_errors() {
        let err =
            handle_ingest_multistep(&json!({"content": 42}), None, FeatureTier::Smart).unwrap_err();
        assert!(err.contains("must be a string"), "got: {err}");
    }

    #[test]
    fn empty_content_errors() {
        let err =
            handle_ingest_multistep(&json!({"content": ""}), None, FeatureTier::Smart).unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn keyword_tier_returns_tier_locked_advisory() {
        let h = handler_with_mock_responses(vec![Ok("{}".to_string())], FeatureTier::Smart);
        let resp = handle_ingest_multistep(
            &json!({"content": "hello world"}),
            Some(&h),
            FeatureTier::Keyword,
        )
        .expect("tier-locked is informational");
        assert_eq!(
            resp["tier-locked"].as_str(),
            Some("memory_ingest_multistep requires smart tier or higher")
        );
        assert_eq!(resp["current_tier"].as_str(), Some("keyword"));
    }

    #[test]
    fn handler_none_returns_tier_locked_at_higher_tier() {
        let resp = handle_ingest_multistep(
            &json!({"content": "hello world"}),
            None,
            FeatureTier::Semantic,
        )
        .expect("none-handler degrades to advisory");
        assert!(resp["tier-locked"].is_string());
    }

    #[test]
    fn unknown_variant_errors_with_explicit_options() {
        let h = handler_with_mock_responses(vec![Ok("{}".to_string())], FeatureTier::Smart);
        let err = handle_ingest_multistep(
            &json!({"content": "hi", "pipeline_variant": "magic"}),
            Some(&h),
            FeatureTier::Smart,
        )
        .unwrap_err();
        assert!(err.contains("two_phase"), "got: {err}");
        assert!(err.contains("four_step"), "got: {err}");
    }

    #[test]
    fn two_phase_run_returns_structured_envelope() {
        let h = handler_with_mock_responses(
            vec![Ok(
                r#"{"title":"T","summary":"S","tags":[],"atoms":[]}"#.to_string()
            )],
            FeatureTier::Smart,
        );
        let resp = handle_ingest_multistep(
            &json!({"content": "Paris is the capital of France."}),
            Some(&h),
            FeatureTier::Smart,
        )
        .expect("ok");
        assert_eq!(resp["variant"], "two_phase");
        assert_eq!(resp["prompt_cache_consistent"], true);
        assert!(resp["stages"].as_array().unwrap().len() >= 3);
    }

    #[test]
    fn four_step_run_returns_structured_envelope() {
        let h = handler_with_mock_responses(
            vec![
                Ok(r#"{"fact_kind":"declarative","confidence":0.9}"#.to_string()),
                Ok(r#"{"entities":[],"claims":[],"relations":[]}"#.to_string()),
                Ok(r#"{"title":"X","summary":"Y","tags":[],"proposed_links":[]}"#.to_string()),
            ],
            FeatureTier::Smart,
        );
        let resp = handle_ingest_multistep(
            &json!({"content": "Paris", "pipeline_variant": "four_step"}),
            Some(&h),
            FeatureTier::Smart,
        )
        .expect("ok");
        assert_eq!(resp["variant"], "four_step");
        // All LLM stages within the run must share the cache key.
        assert_eq!(resp["distinct_cache_keys"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn pipeline_override_drives_custom_pipeline() {
        use crate::multistep_ingest::HelperKind;
        use crate::multistep_ingest::pipeline::{Pipeline, PipelineVariant, Stage};
        let pipeline = Pipeline {
            variant: PipelineVariant::TwoPhase,
            system_prompt: "Custom system prompt".to_string(),
            stages: vec![Stage::Helper {
                kind: HelperKind::FtsClassifier,
                params: Default::default(),
            }],
        };
        let h = handler_with_mock_responses(vec![], FeatureTier::Smart);
        let resp = handle_ingest_multistep(
            &json!({
                "content": "First, do step one. Then do step two.",
                "pipeline_override": pipeline,
            }),
            Some(&h),
            FeatureTier::Smart,
        )
        .expect("ok");
        // Helper-only pipeline → final output is the helper payload.
        assert_eq!(resp["final_output"]["fact_kind"], "procedural");
    }
}
