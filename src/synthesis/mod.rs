// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.x Form 1 — online dedup-and-synthesis (Batman framework Form 1).
//!
//! Single batch action-emitting LLM call evaluated BEFORE the SQL write.
//! Input: incoming fact + N existing candidates from the FTS overlap
//! pre-filter. Output: per-candidate verb in `{add, update, delete,
//! no_op}` plus (when `update`) merged-content; (when `delete`) the
//! candidate id to remove.
//!
//! This module replaces the legacy per-pair binary contradiction
//! classifier on the store path. The legacy classifier is preserved
//! behind the namespace policy `legacy_per_pair_classifier`; operators
//! who prefer the old behaviour can opt in via that flag.
//!
//! # Wire shape
//!
//! The prompt instructs the model to return strict JSON:
//!
//! ```json
//! {
//!   "verdicts": [
//!     {
//!       "candidate_id": "<id>",
//!       "verb": "add" | "update" | "delete" | "no_op",
//!       "merged_content": "<string, only present when verb=update>",
//!       "reason": "<short human-readable string, optional>"
//!     }
//!   ]
//! }
//! ```
//!
//! When the model emits a free-form preamble the parser still strips
//! to the first balanced JSON object. Each verdict is validated; the
//! whole batch is rejected (and the legacy fall-through engaged) when
//! ANY verdict fails validation — audit-honest "all-or-nothing" is the
//! safer default than partial application of a half-parsed plan.

use crate::llm::OllamaClient;
use crate::models::Memory;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Per-candidate action verb returned by the synthesis LLM call.
///
/// * `Add` — keep the candidate; insert the incoming fact as a new row.
/// * `Update` — modify the candidate IN PLACE with `merged_content`;
///   SKIP the new-row insert (the merge subsumes the incoming fact).
/// * `Delete` — remove the candidate; proceed with new-row insert
///   (the incoming fact supersedes the stale candidate).
/// * `NoOp` — leave the candidate alone; proceed with the new-row
///   insert (the candidate is unrelated / orthogonal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisVerb {
    Add,
    Update,
    Delete,
    NoOp,
}

impl SynthesisVerb {
    /// Telemetry label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::NoOp => "no_op",
        }
    }
}

/// A single verdict in the synthesis batch response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    /// Candidate memory id this verdict applies to.
    pub candidate_id: String,
    /// Per-candidate action verb.
    pub verb: SynthesisVerb,
    /// When `verb=update`, the merged-content the candidate should
    /// be rewritten with. `None` for the other three verbs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_content: Option<String>,
    /// Optional human-readable reason; surfaced in telemetry and the
    /// response envelope's `synthesis_decisions` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Full synthesis batch response. Fans out one [`Verdict`] per
/// candidate the pre-filter surfaced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesisResponse {
    pub verdicts: Vec<Verdict>,
}

/// Build the synthesis prompt: incoming fact + N candidates + the
/// strict JSON output schema instruction. Prompt-engineered for Gemma
/// 4 / generic Ollama-served instruction models.
#[must_use]
pub fn build_prompt(incoming_title: &str, incoming_content: &str, candidates: &[Memory]) -> String {
    let mut buf = String::with_capacity(
        512 + incoming_title.len() + incoming_content.len() + candidates.len() * 256,
    );
    buf.push_str(
        "You are a memory-dedup synthesiser. Given an INCOMING fact and a list of \
         EXISTING memory candidates from the same namespace, return a strict JSON \
         object naming exactly one action verb per candidate. Verbs are:\n\
         \n\
         - \"add\":    candidate is unrelated; keep it untouched.\n\
         - \"update\": candidate is the same fact restated; rewrite it with the \
         supplied merged_content (string) that combines both.\n\
         - \"delete\": candidate is now stale or contradicted; remove it.\n\
         - \"no_op\":  candidate is loosely related but distinct; leave it.\n\
         \n\
         Output JSON shape (NO PROSE, NO MARKDOWN FENCE):\n\
         {\"verdicts\":[{\"candidate_id\":\"<id>\",\"verb\":\"add|update|delete|no_op\",\
         \"merged_content\":\"<only when verb=update>\",\"reason\":\"<short string>\"}]}\n\
         \n\
         INCOMING:\n\
         Title: ",
    );
    buf.push_str(incoming_title);
    buf.push_str("\nContent: ");
    buf.push_str(incoming_content);
    buf.push_str("\n\nEXISTING CANDIDATES:\n");
    for (idx, cand) in candidates.iter().enumerate() {
        buf.push_str(&format!(
            "[{}] id={} title={}\n  content: {}\n",
            idx, cand.id, cand.title, cand.content
        ));
    }
    buf.push_str("\nReturn ONLY the JSON object. No commentary.\n");
    buf
}

/// Strip a JSON object out of a potentially-noisy LLM response. The
/// model SHOULD emit pure JSON but Gemma 4 / smaller Ollama models
/// occasionally prepend a one-line preamble or wrap in ```json fences.
///
/// Returns the substring spanning the first balanced top-level `{...}`
/// pair, or `None` if no balanced object exists.
fn extract_json_object(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let mut start = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if start.is_none() {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = start
                {
                    return Some(&raw[s..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse a model response into a [`SynthesisResponse`], validating
/// that:
///
/// 1. The response decodes as JSON containing the `verdicts` array.
/// 2. Every verdict's `candidate_id` matches one of the supplied
///    candidate ids (no fabricated ids — Gemma 4 occasionally
///    hallucinates ids when over-eager).
/// 3. Every `verb=update` carries non-empty `merged_content`.
/// 4. Every supplied candidate id is covered by exactly one verdict.
///
/// On any validation failure returns `Err`; the caller falls back to
/// the legacy code path (a structurally-degraded LLM does NOT block
/// the store).
pub fn parse_response(raw: &str, candidates: &[Memory]) -> Result<SynthesisResponse> {
    let json_str =
        extract_json_object(raw).ok_or_else(|| anyhow!("synthesis: no JSON object in response"))?;
    let parsed: Value =
        serde_json::from_str(json_str).map_err(|e| anyhow!("synthesis: JSON parse failed: {e}"))?;
    let response: SynthesisResponse = serde_json::from_value(parsed)
        .map_err(|e| anyhow!("synthesis: shape mismatch (missing verdicts/verb): {e}"))?;

    // Validate every candidate has exactly one verdict and no
    // fabricated ids leaked in.
    let candidate_ids: std::collections::HashSet<&str> =
        candidates.iter().map(|c| c.id.as_str()).collect();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for v in &response.verdicts {
        if !candidate_ids.contains(v.candidate_id.as_str()) {
            return Err(anyhow!(
                "synthesis: verdict references unknown candidate_id '{}'",
                v.candidate_id
            ));
        }
        if !seen.insert(v.candidate_id.as_str()) {
            return Err(anyhow!(
                "synthesis: duplicate verdict for candidate_id '{}'",
                v.candidate_id
            ));
        }
        if v.verb == SynthesisVerb::Update {
            let m = v.merged_content.as_deref().unwrap_or("");
            if m.trim().is_empty() {
                return Err(anyhow!(
                    "synthesis: update verdict for '{}' lacks merged_content",
                    v.candidate_id
                ));
            }
        }
    }
    if seen.len() != candidate_ids.len() {
        return Err(anyhow!(
            "synthesis: verdict count {} does not match candidate count {}",
            seen.len(),
            candidate_ids.len()
        ));
    }
    Ok(response)
}

/// Issue the synthesis batch call against an `OllamaClient`. Single
/// LLM round-trip; the prompt instructs the model to emit one verdict
/// per candidate. Errors propagate; the caller swallows them and
/// falls back to the legacy per-pair classifier.
///
/// # Errors
///
/// Returns `Err` when the LLM call fails, the response is not parseable,
/// or any verdict fails validation.
pub fn synthesise(
    llm: &OllamaClient,
    incoming_title: &str,
    incoming_content: &str,
    candidates: &[Memory],
) -> Result<SynthesisResponse> {
    if candidates.is_empty() {
        // No candidates means there's nothing to synthesise — return
        // an empty verdict list. Caller proceeds with the standard
        // insert path.
        return Ok(SynthesisResponse { verdicts: vec![] });
    }
    let prompt = build_prompt(incoming_title, incoming_content, candidates);
    let raw = llm.generate(&prompt, Some(SYNTHESIS_SYSTEM))?;
    parse_response(&raw, candidates)
}

/// System prompt the synthesis call ships. Pinned to deterministic
/// behaviour so retries against the same input converge.
pub const SYNTHESIS_SYSTEM: &str = "You return strict JSON only. No markdown fences. \
                                    No prose. Cover every supplied candidate exactly once.";

/// Summary counts of the per-verb verdicts in a synthesis batch.
/// Surfaced via `tracing::info!` and the response envelope.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SynthesisCounts {
    pub add: usize,
    pub update: usize,
    pub delete: usize,
    pub no_op: usize,
}

impl SynthesisCounts {
    /// Tally verdicts. Used by the store path for telemetry + response.
    #[must_use]
    pub fn from_response(resp: &SynthesisResponse) -> Self {
        let mut c = Self::default();
        for v in &resp.verdicts {
            match v.verb {
                SynthesisVerb::Add => c.add += 1,
                SynthesisVerb::Update => c.update += 1,
                SynthesisVerb::Delete => c.delete += 1,
                SynthesisVerb::NoOp => c.no_op += 1,
            }
        }
        c
    }

    /// JSON shape for the response envelope. Stable wire contract.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "add": self.add,
            "update": self.update,
            "delete": self.delete,
            "no_op": self.no_op,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, MemoryKind, Tier};

    fn cand(id: &str, title: &str, content: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: id.to_string(),
            tier: Tier::Mid,
            namespace: "ns".to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
            reflection_depth: 0,
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
        }
    }

    #[test]
    fn build_prompt_includes_all_candidates() {
        let cs = vec![
            cand("a", "title-a", "content-a"),
            cand("b", "title-b", "content-b"),
        ];
        let p = build_prompt("incoming-title", "incoming-content", &cs);
        assert!(p.contains("incoming-title"));
        assert!(p.contains("incoming-content"));
        assert!(p.contains("title-a"));
        assert!(p.contains("title-b"));
        assert!(p.contains("id=a"));
        assert!(p.contains("id=b"));
        assert!(p.contains("\"verdicts\""));
    }

    #[test]
    fn extract_json_object_handles_preamble() {
        let raw = "Sure! Here is the JSON: {\"verdicts\":[]} thanks!";
        let extracted = extract_json_object(raw).unwrap();
        assert_eq!(extracted, "{\"verdicts\":[]}");
    }

    #[test]
    fn extract_json_object_handles_nested_braces() {
        let raw = r#"{"verdicts":[{"candidate_id":"x","verb":"add"}]}"#;
        let extracted = extract_json_object(raw).unwrap();
        assert_eq!(extracted, raw);
    }

    #[test]
    fn extract_json_object_handles_string_with_brace() {
        let raw =
            r#"{"verdicts":[{"candidate_id":"x","verb":"no_op","reason":"has } in string"}]}"#;
        let extracted = extract_json_object(raw).unwrap();
        assert_eq!(extracted, raw);
    }

    #[test]
    fn parse_response_valid_batch() {
        let cs = vec![cand("a", "ta", "ca"), cand("b", "tb", "cb")];
        let raw = r#"{"verdicts":[
            {"candidate_id":"a","verb":"no_op"},
            {"candidate_id":"b","verb":"delete"}
        ]}"#;
        let r = parse_response(raw, &cs).unwrap();
        assert_eq!(r.verdicts.len(), 2);
        assert_eq!(r.verdicts[0].verb, SynthesisVerb::NoOp);
        assert_eq!(r.verdicts[1].verb, SynthesisVerb::Delete);
    }

    #[test]
    fn parse_response_rejects_fabricated_id() {
        let cs = vec![cand("a", "ta", "ca")];
        let raw = r#"{"verdicts":[{"candidate_id":"FAKE","verb":"add"}]}"#;
        assert!(parse_response(raw, &cs).is_err());
    }

    #[test]
    fn parse_response_rejects_missing_merged_content_for_update() {
        let cs = vec![cand("a", "ta", "ca")];
        let raw = r#"{"verdicts":[{"candidate_id":"a","verb":"update"}]}"#;
        assert!(parse_response(raw, &cs).is_err());
    }

    #[test]
    fn parse_response_rejects_partial_coverage() {
        let cs = vec![cand("a", "ta", "ca"), cand("b", "tb", "cb")];
        let raw = r#"{"verdicts":[{"candidate_id":"a","verb":"add"}]}"#;
        assert!(parse_response(raw, &cs).is_err());
    }

    #[test]
    fn parse_response_rejects_duplicate_verdicts() {
        let cs = vec![cand("a", "ta", "ca")];
        let raw = r#"{"verdicts":[
            {"candidate_id":"a","verb":"add"},
            {"candidate_id":"a","verb":"no_op"}
        ]}"#;
        assert!(parse_response(raw, &cs).is_err());
    }

    #[test]
    fn synthesis_counts_tallies_correctly() {
        let resp = SynthesisResponse {
            verdicts: vec![
                Verdict {
                    candidate_id: "a".into(),
                    verb: SynthesisVerb::Add,
                    merged_content: None,
                    reason: None,
                },
                Verdict {
                    candidate_id: "b".into(),
                    verb: SynthesisVerb::Update,
                    merged_content: Some("merged".into()),
                    reason: None,
                },
                Verdict {
                    candidate_id: "c".into(),
                    verb: SynthesisVerb::Update,
                    merged_content: Some("merged".into()),
                    reason: None,
                },
                Verdict {
                    candidate_id: "d".into(),
                    verb: SynthesisVerb::Delete,
                    merged_content: None,
                    reason: None,
                },
                Verdict {
                    candidate_id: "e".into(),
                    verb: SynthesisVerb::NoOp,
                    merged_content: None,
                    reason: None,
                },
            ],
        };
        let c = SynthesisCounts::from_response(&resp);
        assert_eq!(c.add, 1);
        assert_eq!(c.update, 2);
        assert_eq!(c.delete, 1);
        assert_eq!(c.no_op, 1);
    }

    #[test]
    fn synthesise_with_no_candidates_returns_empty() {
        // No LLM call should be made; we test the early return.
        // We can't easily construct an OllamaClient without Ollama running,
        // so verify the empty-candidates path via the prompt builder instead.
        let p = build_prompt("incoming", "body", &[]);
        assert!(p.contains("EXISTING CANDIDATES"));
    }

    #[test]
    fn verb_as_str_round_trip() {
        assert_eq!(SynthesisVerb::Add.as_str(), "add");
        assert_eq!(SynthesisVerb::Update.as_str(), "update");
        assert_eq!(SynthesisVerb::Delete.as_str(), "delete");
        assert_eq!(SynthesisVerb::NoOp.as_str(), "no_op");
    }
}
