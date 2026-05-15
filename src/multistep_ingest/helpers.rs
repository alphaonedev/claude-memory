// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Form 3 — deterministic helpers. Cheap, fast, JSON-output.
//!
//! The Batman exemplar is "phase one is a deterministic helper script;
//! phase two reads the JSON output and is told `Do NOT re-run discovery
//! commands or re-count lines, trust the script's results entirely`."
//! This module's three helpers (Jaccard overlap, cosine pre-filter, FTS
//! classifier) are the deterministic substrate Form 3's LLM stages
//! lean on.
//!
//! Every helper returns a [`HelperOutput`] carrying:
//!
//! 1. A `serde_json::Value` payload that goes verbatim into the LLM
//!    prompt's trust slot.
//! 2. A short `summary` line for the operator trace.
//! 3. A `kind` discriminator so the executor can label the slot.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// In-memory envelope a caller passes into the orchestrator. This is the
/// substrate-agnostic shape: an id, the body text, and (optionally) a
/// pre-computed embedding for cosine pre-filtering. The orchestrator
/// does NOT touch the storage layer directly — callers (the MCP
/// handler, CLI, integration tests) materialise the candidate set
/// before calling [`crate::multistep_ingest::executor::IngestExecutor::run`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryHandle {
    /// Stable identifier (UUID-shaped in production; arbitrary string
    /// in tests).
    pub id: String,
    /// Body text used for FTS / Jaccard overlap.
    pub body: String,
    /// Optional dense embedding for cosine pre-filter. `None` skips
    /// cosine and falls through to the keyword-only path.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    /// Optional namespace tag used by the FTS classifier as a coarse
    /// routing hint.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Which deterministic helper a pipeline stage runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperKind {
    /// Jaccard token overlap between the incoming content and a set of
    /// candidate memories. Cheap, no embedding required.
    JaccardOverlap,
    /// Cosine similarity pre-filter — drops candidates below a
    /// threshold so the LLM stage gets a tighter set.
    CosinePreFilter,
    /// FTS-style classifier — returns a coarse fact-kind tag
    /// (`procedural` / `declarative` / `episodic`) derived from
    /// substring + namespace heuristics.
    FtsClassifier,
}

impl HelperKind {
    /// Snake-case discriminator used in the JSON trace + cache key
    /// derivation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JaccardOverlap => "jaccard_overlap",
            Self::CosinePreFilter => "cosine_pre_filter",
            Self::FtsClassifier => "fts_classifier",
        }
    }
}

/// Parameters passed to a helper invocation. Each helper inspects only
/// the fields it cares about; unused fields are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HelperParams {
    /// Incoming content the orchestrator is ingesting.
    pub content: String,
    /// Candidate memories to score against. Empty for helpers that
    /// don't need a candidate set (e.g., FTS classifier on standalone
    /// content).
    #[serde(default)]
    pub candidates: Vec<MemoryHandle>,
    /// Cosine threshold (only consulted by `CosinePreFilter`). Default
    /// `0.20` matches the substrate's recall semantic threshold.
    #[serde(default)]
    pub cosine_threshold: Option<f32>,
    /// Caller-supplied embedding for the incoming content (cosine
    /// pre-filter input). `None` if the caller hasn't embedded the
    /// content yet — the helper degrades to a no-op in that case.
    #[serde(default)]
    pub content_embedding: Option<Vec<f32>>,
    /// Namespace hint forwarded to the FTS classifier.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Output of a single helper invocation. Carries the JSON payload that
/// LLM stages render into trust slots verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperOutput {
    /// Which helper produced this output.
    pub kind: HelperKind,
    /// Free-form one-line summary for the operator trace (e.g.,
    /// `"jaccard: 3/10 candidates over 0.40 overlap"`).
    pub summary: String,
    /// Structured JSON payload threaded into the LLM stage's trust
    /// slot. Helper-specific shape; downstream stages MUST treat it as
    /// authoritative per the explicit-trust contract.
    pub payload: Value,
}

/// Jaccard token overlap between the incoming content and each
/// candidate body. Returns the top-N candidates sorted by overlap.
///
/// The overlap metric is `|A ∩ B| / |A ∪ B|` on whitespace-split
/// lowercase tokens. Two empty bodies score `0.0` (no overlap) rather
/// than `1.0` (degenerate sets) to avoid surfacing zero-length matches
/// to the LLM.
#[must_use]
pub fn jaccard_overlap(params: &HelperParams) -> HelperOutput {
    let content_tokens = tokenise(&params.content);
    let mut scored: Vec<(String, f32, String)> = params
        .candidates
        .iter()
        .map(|c| {
            let candidate_tokens = tokenise(&c.body);
            let overlap = jaccard(&content_tokens, &candidate_tokens);
            (c.id.clone(), overlap, c.body.clone())
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(10);

    let over_threshold: Vec<&(String, f32, String)> = scored
        .iter()
        .filter(|(_, score, _)| *score >= 0.40)
        .collect();

    let summary = format!(
        "jaccard: {}/{} candidates over 0.40 overlap",
        over_threshold.len(),
        params.candidates.len()
    );

    let payload = json!({
        "helper": "jaccard_overlap",
        "candidates_scored": params.candidates.len(),
        "top_candidates": scored
            .iter()
            .map(|(id, score, body)| json!({
                "id": id,
                "overlap": score,
                "preview": preview(body, 120),
            }))
            .collect::<Vec<_>>(),
    });

    HelperOutput {
        kind: HelperKind::JaccardOverlap,
        summary,
        payload,
    }
}

/// Cosine similarity pre-filter over candidates with embeddings.
/// Returns the candidate set above `cosine_threshold` (default `0.20`).
/// Candidates without embeddings are passed through with `score = null`
/// so the LLM still sees them but they don't contribute to ranking.
#[must_use]
pub fn cosine_pre_filter(params: &HelperParams) -> HelperOutput {
    let threshold = params.cosine_threshold.unwrap_or(0.20);
    let content_emb = params.content_embedding.as_deref();

    let scored: Vec<Value> = params
        .candidates
        .iter()
        .map(|c| {
            let score = match (content_emb, c.embedding.as_deref()) {
                (Some(a), Some(b)) => Some(cosine(a, b)),
                _ => None,
            };
            json!({
                "id": c.id,
                "score": score,
                "above_threshold": score.is_some_and(|s| s >= threshold),
                "preview": preview(&c.body, 120),
            })
        })
        .collect();

    let kept = scored
        .iter()
        .filter(|v| v["above_threshold"].as_bool().unwrap_or(false))
        .count();
    let total = scored.len();

    let summary = format!("cosine: {kept}/{total} candidates over {threshold:.2} threshold");

    let payload = json!({
        "helper": "cosine_pre_filter",
        "threshold": threshold,
        "candidates_scored": total,
        "candidates_kept": kept,
        "candidates": scored,
    });

    HelperOutput {
        kind: HelperKind::CosinePreFilter,
        summary,
        payload,
    }
}

/// FTS-style classifier — labels the incoming content as one of
/// `procedural` / `declarative` / `episodic` using substring + tag
/// heuristics. Deterministic; no LLM involved.
///
/// The classification is intentionally coarse — it exists to give the
/// LLM stage a starting hint so it doesn't burn tokens re-deriving the
/// kind from scratch. Per Batman's contract, the LLM is told "trust
/// this label" rather than asked to re-classify.
#[must_use]
pub fn fts_classifier(params: &HelperParams) -> HelperOutput {
    let body = params.content.to_lowercase();
    let kind = if body.contains("step ") || body.contains("first, ") || body.contains("then ") {
        "procedural"
    } else if body.contains("yesterday")
        || body.contains("today")
        || body.contains("happened")
        || body.contains("event")
    {
        "episodic"
    } else {
        "declarative"
    };

    let summary = format!(
        "fts_classifier: kind={kind} (namespace={})",
        params.namespace.as_deref().unwrap_or("global")
    );

    let payload = json!({
        "helper": "fts_classifier",
        "fact_kind": kind,
        "namespace": params.namespace.clone().unwrap_or_else(|| "global".to_string()),
        "tokens": tokenise(&params.content).len(),
    });

    HelperOutput {
        kind: HelperKind::FtsClassifier,
        summary,
        payload,
    }
}

/// Dispatch a helper by kind. Used by the executor when walking a
/// pipeline's stage list.
#[must_use]
pub fn run_helper(kind: HelperKind, params: &HelperParams) -> HelperOutput {
    match kind {
        HelperKind::JaccardOverlap => jaccard_overlap(params),
        HelperKind::CosinePreFilter => cosine_pre_filter(params),
        HelperKind::FtsClassifier => fts_classifier(params),
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn tokenise(body: &str) -> HashSet<String> {
    body.split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersect: usize = a.intersection(b).count();
    let union: usize = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        intersect as f32 / union as f32
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn preview(body: &str, max: usize) -> String {
    if body.chars().count() <= max {
        body.to_string()
    } else {
        let truncated: String = body.chars().take(max).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mh(id: &str, body: &str) -> MemoryHandle {
        MemoryHandle {
            id: id.to_string(),
            body: body.to_string(),
            embedding: None,
            namespace: None,
        }
    }

    fn mh_emb(id: &str, body: &str, embedding: Vec<f32>) -> MemoryHandle {
        MemoryHandle {
            id: id.to_string(),
            body: body.to_string(),
            embedding: Some(embedding),
            namespace: None,
        }
    }

    #[test]
    fn jaccard_overlap_returns_non_empty_for_overlapping_text() {
        let params = HelperParams {
            content: "the quick brown fox jumps over the lazy dog".to_string(),
            candidates: vec![
                mh("a", "a quick brown dog"),
                mh("b", "completely unrelated content here"),
            ],
            ..Default::default()
        };
        let out = jaccard_overlap(&params);
        assert_eq!(out.kind, HelperKind::JaccardOverlap);
        let top = out.payload["top_candidates"].as_array().unwrap();
        assert_eq!(top.len(), 2);
        // The 'a' candidate must rank higher than 'b'.
        assert_eq!(top[0]["id"].as_str(), Some("a"));
        let top_score = top[0]["overlap"].as_f64().unwrap();
        let bot_score = top[1]["overlap"].as_f64().unwrap();
        assert!(top_score > bot_score);
    }

    #[test]
    fn jaccard_overlap_handles_empty_candidates_cleanly() {
        let params = HelperParams {
            content: "hello world".to_string(),
            candidates: vec![],
            ..Default::default()
        };
        let out = jaccard_overlap(&params);
        assert_eq!(out.payload["candidates_scored"], 0);
        assert_eq!(out.payload["top_candidates"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn cosine_pre_filter_drops_below_threshold() {
        let params = HelperParams {
            content: "x".to_string(),
            candidates: vec![
                mh_emb("near", "near body", vec![1.0, 0.0, 0.0]),
                mh_emb("far", "far body", vec![0.0, 1.0, 0.0]),
            ],
            content_embedding: Some(vec![1.0, 0.05, 0.0]),
            cosine_threshold: Some(0.50),
            ..Default::default()
        };
        let out = cosine_pre_filter(&params);
        let kept = out.payload["candidates_kept"].as_u64().unwrap();
        assert_eq!(kept, 1, "only the 'near' candidate should pass");
    }

    #[test]
    fn cosine_pre_filter_no_embedding_degrades_to_null_scores() {
        let params = HelperParams {
            content: "x".to_string(),
            candidates: vec![mh("a", "a")],
            content_embedding: None,
            ..Default::default()
        };
        let out = cosine_pre_filter(&params);
        let candidates = out.payload["candidates"].as_array().unwrap();
        assert!(candidates[0]["score"].is_null());
        assert_eq!(candidates[0]["above_threshold"], false);
    }

    #[test]
    fn fts_classifier_labels_procedural_text() {
        let params = HelperParams {
            content: "Step 1: open the door. Then walk through.".to_string(),
            ..Default::default()
        };
        let out = fts_classifier(&params);
        assert_eq!(out.payload["fact_kind"], "procedural");
    }

    #[test]
    fn fts_classifier_labels_episodic_text() {
        let params = HelperParams {
            content: "Yesterday I went to the store.".to_string(),
            ..Default::default()
        };
        let out = fts_classifier(&params);
        assert_eq!(out.payload["fact_kind"], "episodic");
    }

    #[test]
    fn fts_classifier_default_is_declarative() {
        let params = HelperParams {
            content: "The capital of France is Paris.".to_string(),
            ..Default::default()
        };
        let out = fts_classifier(&params);
        assert_eq!(out.payload["fact_kind"], "declarative");
    }

    #[test]
    fn run_helper_dispatches_correctly() {
        let params = HelperParams {
            content: "anything".to_string(),
            ..Default::default()
        };
        let out = run_helper(HelperKind::FtsClassifier, &params);
        assert_eq!(out.kind, HelperKind::FtsClassifier);
    }

    #[test]
    fn helper_kind_serialisation_is_snake_case() {
        assert_eq!(HelperKind::JaccardOverlap.as_str(), "jaccard_overlap");
        assert_eq!(HelperKind::CosinePreFilter.as_str(), "cosine_pre_filter");
        assert_eq!(HelperKind::FtsClassifier.as_str(), "fts_classifier");
    }
}
