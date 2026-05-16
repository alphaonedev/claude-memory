// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Form 3 — pipeline descriptor + the two default Batman exemplars.
//!
//! A [`Pipeline`] is an ordered list of [`Stage`]s. Helpers go first
//! (deterministic, parallel-where-independent); LLM stages follow with
//! explicit trust slots pointing back at the helper outputs.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::helpers::{HelperKind, HelperParams};

/// Named pipeline variant exposed at the MCP tool surface. Operators
/// pick a variant via `pipeline_variant: "two_phase" | "four_step"` and
/// can override the descriptor entirely via `pipeline_override` (a
/// JSON-encoded [`Pipeline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineVariant {
    /// Understand-Anything two-phase exemplar.
    TwoPhase,
    /// OpenKB four-step exemplar.
    FourStep,
}

impl PipelineVariant {
    /// Snake-case discriminator used in the shared-prefix builder and
    /// the JSON trace.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TwoPhase => "two_phase",
            Self::FourStep => "four_step",
        }
    }

    /// Parse a variant tag (snake_case). Returns `None` for unknown
    /// inputs so the caller can surface a structured validation error.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "two_phase" => Some(Self::TwoPhase),
            "four_step" => Some(Self::FourStep),
            _ => None,
        }
    }
}

/// Reference to a prior helper output, surfaced to an LLM stage via its
/// explicit-trust slot. `stage_index` is the zero-based position of the
/// helper stage that produced the output; the executor resolves these
/// against its in-flight context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperOutputRef {
    /// Zero-based index of the producing helper stage.
    pub stage_index: usize,
    /// Label for the slot — appears in the LLM prompt so the model can
    /// distinguish multiple trust slots (`"overlap"`, `"classification"`,
    /// etc.).
    pub label: String,
}

/// A pipeline stage. Helpers run first; LLM stages follow with trust
/// slots resolved against the helper outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Stage {
    /// Deterministic helper stage. Runs synchronously, produces a JSON
    /// payload, no LLM involvement.
    Helper {
        /// Which deterministic helper to run.
        kind: HelperKind,
        /// Helper parameters. The executor merges in the run-time
        /// `content` / `candidates` if the descriptor omitted them.
        #[serde(default)]
        params: HelperParams,
    },
    /// LLM call stage. The prompt template is appended to the SHARED
    /// PREFIX from [`super::cache::build_shared_prefix`]; trust slots
    /// are rendered verbatim into the prompt.
    LlmCall {
        /// Free-form prompt body (the stage-specific tail of the
        /// shared-prefix sandwich).
        prompt_template: String,
        /// Trust slots — references to prior helper outputs that get
        /// rendered into the prompt under the explicit-trust banner.
        #[serde(default)]
        trust_inputs: Vec<HelperOutputRef>,
        /// Output schema hint forwarded to the LLM and echoed in the
        /// trace so callers can route the parsed JSON.
        #[serde(default)]
        output_schema: Value,
        /// Stage label — surfaces in the trace and the LLM prompt.
        #[serde(default)]
        label: String,
    },
}

/// A pipeline descriptor. Each stage runs in declaration order; the
/// executor enforces "helpers before LLM stages" so the trust slots
/// are always resolvable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    /// Variant tag (drives the shared-prefix builder).
    pub variant: PipelineVariant,
    /// Stages in execution order.
    pub stages: Vec<Stage>,
    /// System prompt shared across every LLM stage in this pipeline.
    /// Goes into the prompt-cache-friendly shared prefix; do NOT put
    /// per-stage variation here.
    #[serde(default)]
    pub system_prompt: String,
}

impl Pipeline {
    /// Variant-tag accessor used by the executor when assembling the
    /// shared prefix.
    #[must_use]
    pub fn variant_tag(&self) -> &'static str {
        self.variant.as_str()
    }
}

/// Understand-Anything-style two-phase pipeline.
///
/// Phase 1 (Helper): FTS overlap + Jaccard pre-filter against existing
/// memories. Both helpers run in the same stage chain; the executor
/// parallelises them because they have no inter-dependency.
///
/// Phase 2 (LLM): synthesise summary + tags + atoms with explicit trust
/// citing the helper output.
#[must_use]
pub fn two_phase_default() -> Pipeline {
    Pipeline {
        variant: PipelineVariant::TwoPhase,
        system_prompt: "Synthesise the incoming content into a structured \
                        memory envelope with title, summary, tags, and atoms. \
                        Cite the helper output verbatim when it carries \
                        candidate overlaps or classifications."
            .to_string(),
        stages: vec![
            Stage::Helper {
                kind: HelperKind::FtsClassifier,
                params: HelperParams::default(),
            },
            Stage::Helper {
                kind: HelperKind::JaccardOverlap,
                params: HelperParams::default(),
            },
            Stage::LlmCall {
                label: "synthesise".to_string(),
                prompt_template: "Produce a JSON object {title, summary, \
                                  tags[], atoms[]} where each atom is a \
                                  standalone fact distilled from the content. \
                                  The trust slots below carry the \
                                  pre-computed classifier label and the top \
                                  candidate overlaps."
                    .to_string(),
                trust_inputs: vec![
                    HelperOutputRef {
                        stage_index: 0,
                        label: "classification".to_string(),
                    },
                    HelperOutputRef {
                        stage_index: 1,
                        label: "overlap".to_string(),
                    },
                ],
                output_schema: json!({
                    "type": "object",
                    "required": ["title", "summary", "tags", "atoms"],
                    "properties": {
                        "title": {"type": "string"},
                        "summary": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "atoms": {"type": "array", "items": {"type": "string"}}
                    }
                }),
            },
        ],
    }
}

/// OpenKB-style four-step pipeline.
///
/// Stage 1 (Helper): `load_context` — assemble candidate set via FTS
/// classifier + Jaccard overlap. (Two helpers under one logical "load"
/// step.)
///
/// Stage 2 (LLM): `classify` — what kind of fact is this. Trust slot
/// carries the FTS classifier label.
///
/// Stage 3 (LLM): `enrich` — extract entities, claims, relations.
/// Trust slot carries the overlap output so the LLM doesn't re-rank.
///
/// Stage 4 (LLM): `emit` — final structured memory output.
///
/// All LLM stages share the SAME system prompt prefix so the
/// prompt-cache key stays stable across stages within a run.
#[must_use]
pub fn four_step_default() -> Pipeline {
    Pipeline {
        variant: PipelineVariant::FourStep,
        system_prompt: "Run the OpenKB four-step ingest pipeline. Each \
                        stage produces a JSON object that feeds the next \
                        stage. Trust the helper output verbatim — do not \
                        re-derive classifications or overlap scores."
            .to_string(),
        stages: vec![
            Stage::Helper {
                kind: HelperKind::FtsClassifier,
                params: HelperParams::default(),
            },
            Stage::Helper {
                kind: HelperKind::JaccardOverlap,
                params: HelperParams::default(),
            },
            Stage::LlmCall {
                label: "classify".to_string(),
                prompt_template: "Classify this content. Return JSON \
                                  {fact_kind, confidence}."
                    .to_string(),
                trust_inputs: vec![HelperOutputRef {
                    stage_index: 0,
                    label: "fts_classifier".to_string(),
                }],
                output_schema: json!({
                    "type": "object",
                    "required": ["fact_kind", "confidence"],
                    "properties": {
                        "fact_kind": {
                            "type": "string",
                            "enum": ["procedural", "declarative", "episodic"]
                        },
                        "confidence": {
                            "type": "number",
                            "minimum": 0.0,
                            "maximum": 1.0
                        }
                    }
                }),
            },
            Stage::LlmCall {
                label: "enrich".to_string(),
                prompt_template: "Extract entities, claims, and relations \
                                  from the content. Return JSON {entities[], \
                                  claims[], relations[]}."
                    .to_string(),
                trust_inputs: vec![HelperOutputRef {
                    stage_index: 1,
                    label: "overlap".to_string(),
                }],
                output_schema: json!({
                    "type": "object",
                    "required": ["entities", "claims", "relations"],
                    "properties": {
                        "entities": {"type": "array", "items": {"type": "string"}},
                        "claims": {"type": "array", "items": {"type": "string"}},
                        "relations": {"type": "array", "items": {"type": "object"}}
                    }
                }),
            },
            Stage::LlmCall {
                label: "emit".to_string(),
                prompt_template: "Emit the final memory envelope. Return \
                                  JSON {title, summary, tags[], \
                                  proposed_links[]}."
                    .to_string(),
                trust_inputs: vec![
                    HelperOutputRef {
                        stage_index: 0,
                        label: "fts_classifier".to_string(),
                    },
                    HelperOutputRef {
                        stage_index: 1,
                        label: "overlap".to_string(),
                    },
                ],
                output_schema: json!({
                    "type": "object",
                    "required": ["title", "summary", "tags", "proposed_links"],
                    "properties": {
                        "title": {"type": "string"},
                        "summary": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "proposed_links": {"type": "array", "items": {"type": "object"}}
                    }
                }),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_variant_round_trip_via_str() {
        assert_eq!(
            PipelineVariant::from_str("two_phase"),
            Some(PipelineVariant::TwoPhase)
        );
        assert_eq!(
            PipelineVariant::from_str("four_step"),
            Some(PipelineVariant::FourStep)
        );
        assert_eq!(PipelineVariant::from_str("nonsense"), None);
    }

    #[test]
    fn two_phase_default_has_two_phases() {
        let p = two_phase_default();
        assert_eq!(p.variant, PipelineVariant::TwoPhase);
        let helpers = p
            .stages
            .iter()
            .filter(|s| matches!(s, Stage::Helper { .. }))
            .count();
        let llms = p
            .stages
            .iter()
            .filter(|s| matches!(s, Stage::LlmCall { .. }))
            .count();
        // Two helpers (FTS + Jaccard) feed a single LLM synthesise stage.
        assert_eq!(helpers, 2);
        assert_eq!(llms, 1);
    }

    #[test]
    fn four_step_default_has_four_logical_stages() {
        let p = four_step_default();
        assert_eq!(p.variant, PipelineVariant::FourStep);
        let llms = p
            .stages
            .iter()
            .filter(|s| matches!(s, Stage::LlmCall { .. }))
            .count();
        // Stage 1 (Helper) decomposes into 2 helpers; stages 2/3/4 are
        // three LLM calls.
        assert_eq!(llms, 3);
    }

    #[test]
    fn two_phase_llm_stage_references_both_helpers() {
        let p = two_phase_default();
        let Stage::LlmCall { trust_inputs, .. } = p.stages.last().unwrap() else {
            panic!("last stage should be LLM call");
        };
        assert_eq!(trust_inputs.len(), 2);
        assert_eq!(trust_inputs[0].stage_index, 0);
        assert_eq!(trust_inputs[1].stage_index, 1);
    }

    #[test]
    fn four_step_llm_stages_each_have_trust_inputs() {
        let p = four_step_default();
        for stage in &p.stages {
            if let Stage::LlmCall { trust_inputs, .. } = stage {
                assert!(
                    !trust_inputs.is_empty(),
                    "every LLM stage must have at least one trust input"
                );
            }
        }
    }

    #[test]
    fn pipeline_descriptor_round_trips_through_serde() {
        let p = four_step_default();
        let s = serde_json::to_string(&p).expect("serialises");
        let back: Pipeline = serde_json::from_str(&s).expect("deserialises");
        assert_eq!(back.variant, p.variant);
        assert_eq!(back.stages.len(), p.stages.len());
    }

    #[test]
    fn variant_tag_matches_as_str() {
        assert_eq!(two_phase_default().variant_tag(), "two_phase");
        assert_eq!(four_step_default().variant_tag(), "four_step");
    }
}
