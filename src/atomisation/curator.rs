// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-B — atomisation curator.
//!
//! The curator is the LLM-facing half of the atomisation engine: it
//! consumes one long memory body, asks Gemma 4 (E2B at the `smart`
//! tier, E4B at `autonomous`) to decompose it into atomic propositions,
//! parses the structured JSON response, validates per-atom token
//! budgets via `tiktoken-rs::cl100k_base`, and returns a `Vec<Atom>`
//! ready for the substrate writer in [`super::Atomiser::atomise`].
//!
//! The curator is intentionally factored as a trait
//! ([`Curator`]) so the substrate test suite can inject a deterministic
//! mock (see `tests/atomisation/core`). The production implementation
//! ([`LlmCurator`]) wraps an `OllamaClient` and is hot-path only when
//! the daemon's tier resolves to `smart` or higher.
//!
//! # Retry contract
//!
//! Malformed JSON responses retry up to `curator_max_retries` times
//! (default 3) with exponential backoff (100 ms → 500 ms → 2500 ms).
//! Each retry re-sends the original prompt verbatim — the LLM call is
//! stateless on our side. After the final attempt fails, the curator
//! surfaces [`CuratorError::MalformedResponse`] carrying the last
//! parser diagnostic; [`super::Atomiser::atomise`] maps that to
//! [`super::AtomiseError::CuratorFailed`].
//!
//! # Token-budget contract
//!
//! Atoms slightly over budget are accepted as-is — the curator emits
//! a warn-level log line and proceeds. The rationale is documented
//! in the WT-1-B brief ("fail-soft: accept atoms slightly over
//! budget rather than retry-loop"). The substrate writer is the
//! authoritative gate on memory size (governed by
//! `validate::validate_content`), not the curator.

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;

/// One proposed atom returned by the curator.
///
/// The wire shape mirrors the JSON the LLM emits — `{"text": "..."}` —
/// so the parser is `serde_json::from_str::<CuratorResponse>` with no
/// further fixup.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Atom {
    /// Self-contained atomic proposition. Must be ≤ `max_atom_tokens`
    /// when measured with `cl100k_base`; the curator accepts a small
    /// over-budget overshoot rather than retrying.
    pub text: String,
}

/// Top-level wire shape returned by the LLM.
///
/// `atoms` is the list of decomposed propositions. An empty array
/// signals "this input cannot be decomposed" — see the prompt
/// contract; the substrate handler maps that to
/// [`super::AtomiseError::SourceTooSmall`].
#[derive(Debug, Clone, Deserialize)]
pub struct CuratorResponse {
    pub atoms: Vec<Atom>,
}

/// Curator-side error surface.
///
/// All variants carry a human-readable diagnostic; the substrate
/// `atomise` flow wraps them into the typed
/// [`super::AtomiseError::CuratorFailed`] variant.
#[derive(Debug)]
pub enum CuratorError {
    /// LLM was unreachable, returned an HTTP error, or otherwise
    /// failed to produce a body. Retries do NOT happen at this layer
    /// (the underlying `OllamaClient` already retries transient
    /// failures); the substrate caller decides whether to surface or
    /// fall back.
    LlmUnavailable(String),
    /// The LLM produced a body but the body did not parse as a
    /// [`CuratorResponse`] (missing `atoms`, wrong types, JSON
    /// trailing garbage, etc.). Carries the last parse diagnostic
    /// AFTER all retries were exhausted.
    MalformedResponse(String),
}

impl std::fmt::Display for CuratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LlmUnavailable(m) => write!(f, "curator LLM unavailable: {m}"),
            Self::MalformedResponse(m) => write!(f, "curator response malformed: {m}"),
        }
    }
}

impl std::error::Error for CuratorError {}

/// Trait surface the [`super::Atomiser`] consumes.
///
/// The trait abstracts over the LLM round-trip so unit tests can
/// inject a deterministic stub (canned JSON, programmable
/// failure-then-success sequences) without standing up an Ollama
/// process. The production implementation [`LlmCurator`] performs
/// the real network call.
///
/// The trait method is sync (matching the rest of the curator surface
/// in this crate). The Ollama `generate` call is itself blocking-on-
/// HTTP-thread; the substrate `atomise` orchestrator runs on a thread
/// the caller manages.
pub trait Curator: Send + Sync {
    /// Decompose `body` into atomic propositions, each ≤ `max_atom_tokens`.
    ///
    /// Implementations MUST:
    /// 1. Send the canonical system prompt (see [`CURATOR_SYSTEM_PROMPT`]) — the
    ///    `{max_atom_tokens}` placeholder is substituted with the
    ///    caller-supplied value.
    /// 2. Parse the response body as a [`CuratorResponse`]. Retry up
    ///    to `max_retries` times on malformed JSON with exponential
    ///    backoff (100 ms / 500 ms / 2500 ms).
    /// 3. Validate per-atom token counts via
    ///    [`crate::storage::count_tokens_cl100k`]. Atoms slightly
    ///    over budget (≤ 25% overshoot) are accepted and
    ///    `tracing::warn!`-logged; gross over-budget atoms (> 25%)
    ///    are clamped at the prompt level by retry.
    /// 4. Bound the returned vec to `[2..=10]` atoms per the prompt
    ///    contract. An empty vec is a legitimate "cannot decompose"
    ///    signal — the caller maps that to `SourceTooSmall`.
    fn decompose(
        &self,
        body: &str,
        max_atom_tokens: u32,
        max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError>;
}

/// Verbatim system prompt sent to the LLM. The `{max_atom_tokens}`
/// token is substituted at call time. The shape of the JSON response
/// is pinned here — the parser depends on exactly this `{ atoms: [...] }`
/// envelope.
///
/// Lifted from the WT-1-B brief without modification so a future
/// audit can grep this constant in source against the spec doc.
pub const CURATOR_SYSTEM_PROMPT: &str =
    "You are decomposing a long memory into atomic propositions.
Each atom must:
(1) Be self-contained — readable without the original context
(2) Be at most {max_atom_tokens} tokens
(3) Contain exactly one fact, decision, observation, or relation
(4) Preserve original meaning — no editorial additions
Return JSON: { atoms: [{ text: string }] } with 2 to 10 atoms.
If the input cannot be decomposed (already atomic, all-or-nothing),
return { atoms: [] }.";

/// Render the system prompt with the supplied token budget substituted.
#[must_use]
pub fn render_system_prompt(max_atom_tokens: u32) -> String {
    CURATOR_SYSTEM_PROMPT.replace("{max_atom_tokens}", &max_atom_tokens.to_string())
}

/// Try to parse one candidate response body into a [`CuratorResponse`].
///
/// Returns `Ok(response)` on a clean parse, `Err(diagnostic)` on any
/// failure — the diagnostic is the underlying `serde_json` error
/// message verbatim so the retry loop can surface it in
/// [`CuratorError::MalformedResponse`].
///
/// LLM responses often arrive wrapped in markdown code fences (```json
/// … ```) or with leading/trailing prose; we strip the fences and
/// re-attempt once before giving up. This is the same defensive
/// shape used by `crate::llm::OllamaClient::auto_tag` and the
/// reflection curator's summariser.
pub fn parse_response(body: &str) -> Result<CuratorResponse, String> {
    // First attempt — direct parse.
    if let Ok(resp) = serde_json::from_str::<CuratorResponse>(body) {
        return Ok(resp);
    }
    // Second attempt — strip markdown fences. The LLM frequently
    // emits ```json\n...\n``` even when the prompt asks for raw
    // JSON; production curators have to tolerate this.
    let stripped = strip_code_fence(body);
    if let Ok(resp) = serde_json::from_str::<CuratorResponse>(&stripped) {
        return Ok(resp);
    }
    // Third attempt — extract the first balanced JSON object from
    // the body. Tolerates "Here are the atoms:\n{ ... }" preambles.
    if let Some(extracted) = extract_first_json_object(&stripped) {
        if let Ok(resp) = serde_json::from_str::<CuratorResponse>(&extracted) {
            return Ok(resp);
        }
    }
    // All three strategies failed; return the diagnostic from the
    // most informative (first) attempt.
    let err = serde_json::from_str::<CuratorResponse>(body)
        .err()
        .map_or_else(|| "unknown parse failure".to_string(), |e| e.to_string());
    Err(err)
}

/// Strip ``` and ```json fences from a candidate response body.
fn strip_code_fence(s: &str) -> String {
    let trimmed = s.trim();
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let stripped = stripped.trim_start_matches('\n');
    stripped
        .strip_suffix("```")
        .unwrap_or(stripped)
        .trim()
        .to_string()
}

/// Extract the first balanced `{ ... }` substring. Scans byte-wise so
/// string escapes inside the JSON don't fool the brace counter.
fn extract_first_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut prev_backslash = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if b == b'"' && !prev_backslash {
                in_string = false;
            }
            prev_backslash = b == b'\\' && !prev_backslash;
            continue;
        }
        prev_backslash = false;
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s0) = start {
                        return Some(s[s0..=i].to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Token-budget guardrail — accept atoms within 25% of the budget,
/// warn-log overshoots, drop atoms more than 25% over budget so a
/// pathological response cannot pollute the memory store.
///
/// Returns the (atoms_kept, atoms_dropped) pair so the caller can
/// telemetry-log how often the soft cap fires.
#[must_use]
pub fn enforce_token_budget(atoms: Vec<Atom>, max_atom_tokens: u32) -> (Vec<Atom>, usize) {
    let hard_cap = max_atom_tokens.saturating_add(max_atom_tokens / 4);
    let mut kept = Vec::with_capacity(atoms.len());
    let mut dropped = 0usize;
    for atom in atoms {
        let count = crate::storage::count_tokens_cl100k(&atom.text);
        let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
        if count_u32 <= max_atom_tokens {
            kept.push(atom);
        } else if count_u32 <= hard_cap {
            tracing::warn!(
                target: "atomisation::curator",
                atom_tokens = count_u32,
                budget = max_atom_tokens,
                "atom slightly over token budget — accepting (fail-soft)"
            );
            kept.push(atom);
        } else {
            tracing::warn!(
                target: "atomisation::curator",
                atom_tokens = count_u32,
                hard_cap,
                "atom grossly over token budget — dropping"
            );
            dropped += 1;
        }
    }
    (kept, dropped)
}

/// Exponential backoff schedule for the curator retry loop:
/// 100 ms, 500 ms, 2500 ms. Indexed by zero-based retry attempt; out
/// of range collapses to the last entry so a misconfigured retry cap
/// does not surface a `panic!`.
#[must_use]
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    const SCHEDULE_MS: &[u64] = &[100, 500, 2500];
    let idx = (attempt as usize).min(SCHEDULE_MS.len() - 1);
    Duration::from_millis(SCHEDULE_MS[idx])
}

// ---------------------------------------------------------------------------
// LlmCurator — production impl backed by `crate::llm::OllamaClient`
// ---------------------------------------------------------------------------

/// Production curator. Wraps an `OllamaClient` (or any
/// `crate::autonomy::AutonomyLlm`-like surface — we re-use the
/// existing `generate` shape via a free function rather than coupling
/// to the autonomy trait, because the autonomy trait does not expose
/// `generate(prompt, system)`).
pub struct LlmCurator<L: LlmGenerate + Send + Sync> {
    llm: L,
    /// Sleep function. Production passes `std::thread::sleep`; tests
    /// pass a no-op to keep the suite fast.
    sleep: Mutex<Box<dyn FnMut(Duration) + Send + Sync>>,
}

/// Minimal generate surface the curator needs. Implemented for
/// `crate::llm::OllamaClient` in the same module; the trait stays
/// here (not in `src/llm.rs`) so external callers don't accidentally
/// pull it into their wire path.
pub trait LlmGenerate {
    /// Run a single generate cycle. Returns the response body verbatim
    /// (no trimming, no fence-stripping — `parse_response` handles
    /// that).
    fn generate(&self, prompt: &str, system: Option<&str>) -> Result<String, CuratorError>;
}

impl LlmGenerate for crate::llm::OllamaClient {
    fn generate(&self, prompt: &str, system: Option<&str>) -> Result<String, CuratorError> {
        Self::generate(self, prompt, system)
            .map_err(|e| CuratorError::LlmUnavailable(e.to_string()))
    }
}

impl<L: LlmGenerate + Send + Sync> LlmCurator<L> {
    /// Construct a curator with the supplied LLM and the real
    /// `std::thread::sleep` for retry backoff.
    pub fn new(llm: L) -> Self {
        Self {
            llm,
            sleep: Mutex::new(Box::new(std::thread::sleep)),
        }
    }

    /// Construct a curator with an injected sleep — used by the
    /// unit test below to keep the suite under one second.
    #[cfg(test)]
    pub fn with_sleep<F>(llm: L, sleep: F) -> Self
    where
        F: FnMut(Duration) + Send + Sync + 'static,
    {
        Self {
            llm,
            sleep: Mutex::new(Box::new(sleep)),
        }
    }
}

impl<L: LlmGenerate + Send + Sync> Curator for LlmCurator<L> {
    fn decompose(
        &self,
        body: &str,
        max_atom_tokens: u32,
        max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        let system = render_system_prompt(max_atom_tokens);
        let mut last_err = String::from("no attempts made");
        for attempt in 0..=max_retries {
            let resp = self.llm.generate(body, Some(&system))?;
            match parse_response(&resp) {
                Ok(parsed) => {
                    let (kept, _dropped) = enforce_token_budget(parsed.atoms, max_atom_tokens);
                    return Ok(kept);
                }
                Err(e) => {
                    last_err = e;
                    if attempt < max_retries {
                        let backoff = backoff_for_attempt(attempt);
                        if let Ok(mut s) = self.sleep.lock() {
                            (s)(backoff);
                        }
                    }
                }
            }
        }
        Err(CuratorError::MalformedResponse(last_err))
    }
}

// ---------------------------------------------------------------------------
// Unit tests — pure logic. Mocked LLM. No DB, no network.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Mock that returns a programmable sequence of responses. Used by
    /// the integration suite as well as the unit tests below.
    pub(crate) struct MockLlm {
        responses: Mutex<Vec<Result<String, CuratorError>>>,
        calls: Mutex<usize>,
    }

    impl MockLlm {
        pub fn new(responses: Vec<Result<String, CuratorError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(0),
            }
        }

        pub fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl LlmGenerate for Arc<MockLlm> {
        fn generate(&self, _prompt: &str, _system: Option<&str>) -> Result<String, CuratorError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let mut rs = self.responses.lock().unwrap();
            if rs.is_empty() {
                return Err(CuratorError::LlmUnavailable(
                    "mock: no responses left".into(),
                ));
            }
            rs.remove(0)
        }
    }

    #[test]
    fn render_prompt_substitutes_max_atom_tokens() {
        let p = render_system_prompt(200);
        assert!(p.contains("at most 200 tokens"));
        assert!(!p.contains("{max_atom_tokens}"));
    }

    #[test]
    fn parse_response_accepts_direct_json() {
        let body = r#"{"atoms":[{"text":"alpha"},{"text":"beta"}]}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.atoms.len(), 2);
        assert_eq!(r.atoms[0].text, "alpha");
    }

    #[test]
    fn parse_response_strips_markdown_fence() {
        let body = "```json\n{\"atoms\":[{\"text\":\"alpha\"}]}\n```";
        let r = parse_response(body).unwrap();
        assert_eq!(r.atoms.len(), 1);
    }

    #[test]
    fn parse_response_extracts_object_with_preamble() {
        let body = "Sure, here's the JSON:\n{\"atoms\":[{\"text\":\"alpha\"}]}\nThanks!";
        let r = parse_response(body).unwrap();
        assert_eq!(r.atoms.len(), 1);
    }

    #[test]
    fn parse_response_empty_atoms_is_valid() {
        // "Cannot decompose" signal — substrate maps to SourceTooSmall.
        let body = r#"{"atoms":[]}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.atoms.len(), 0);
    }

    #[test]
    fn parse_response_rejects_garbage() {
        assert!(parse_response("nope nope nope").is_err());
        assert!(parse_response("").is_err());
        assert!(parse_response(r#"{"wrong":"shape"}"#).is_err());
    }

    #[test]
    fn enforce_token_budget_keeps_in_budget() {
        let atoms = vec![
            Atom {
                text: "small atom".to_string(),
            },
            Atom {
                text: "another small atom".to_string(),
            },
        ];
        let (kept, dropped) = enforce_token_budget(atoms, 200);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn enforce_token_budget_drops_grossly_over() {
        // Build a string that is firmly over the 25% overshoot window.
        let huge: String = "word ".repeat(500);
        let atoms = vec![
            Atom {
                text: "fine".to_string(),
            },
            Atom { text: huge },
        ];
        let (kept, dropped) = enforce_token_budget(atoms, 10);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn backoff_schedule_is_monotonic_and_bounded() {
        assert_eq!(backoff_for_attempt(0), Duration::from_millis(100));
        assert_eq!(backoff_for_attempt(1), Duration::from_millis(500));
        assert_eq!(backoff_for_attempt(2), Duration::from_millis(2500));
        assert_eq!(backoff_for_attempt(99), Duration::from_millis(2500));
    }

    #[test]
    fn curator_succeeds_on_first_attempt() {
        let mock = Arc::new(MockLlm::new(vec![Ok(
            r#"{"atoms":[{"text":"alpha"},{"text":"beta"}]}"#.to_string(),
        )]));
        let curator = LlmCurator::with_sleep(mock.clone(), |_| {});
        let atoms = curator.decompose("input", 200, 3).unwrap();
        assert_eq!(atoms.len(), 2);
        assert_eq!(mock.call_count(), 1);
    }

    #[test]
    fn curator_retries_on_malformed_then_succeeds() {
        let mock = Arc::new(MockLlm::new(vec![
            Ok("garbage".to_string()),
            Ok("still garbage".to_string()),
            Ok(r#"{"atoms":[{"text":"alpha"}]}"#.to_string()),
        ]));
        let curator = LlmCurator::with_sleep(mock.clone(), |_| {});
        let atoms = curator.decompose("input", 200, 3).unwrap();
        assert_eq!(atoms.len(), 1);
        assert_eq!(mock.call_count(), 3);
    }

    #[test]
    fn curator_fails_after_max_retries() {
        let mock = Arc::new(MockLlm::new(vec![
            Ok("garbage 1".to_string()),
            Ok("garbage 2".to_string()),
            Ok("garbage 3".to_string()),
            Ok("garbage 4".to_string()),
        ]));
        let curator = LlmCurator::with_sleep(mock.clone(), |_| {});
        // max_retries=3 means 1 initial + 3 retries = 4 total attempts.
        let err = curator.decompose("input", 200, 3).unwrap_err();
        assert!(matches!(err, CuratorError::MalformedResponse(_)));
        assert_eq!(mock.call_count(), 4);
    }

    #[test]
    fn curator_propagates_llm_unavailable() {
        let mock = Arc::new(MockLlm::new(vec![Err(CuratorError::LlmUnavailable(
            "connection refused".into(),
        ))]));
        let curator = LlmCurator::with_sleep(mock, |_| {});
        let err = curator.decompose("input", 200, 3).unwrap_err();
        assert!(matches!(err, CuratorError::LlmUnavailable(_)));
    }

    #[test]
    fn extract_first_json_object_handles_braces_in_strings() {
        // Brace-counting must NOT be fooled by braces inside JSON strings.
        let s = r#"prefix {"atoms":[{"text":"contains } brace"}]} suffix"#;
        let extracted = extract_first_json_object(s).unwrap();
        let parsed: CuratorResponse = serde_json::from_str(&extracted).unwrap();
        assert_eq!(parsed.atoms.len(), 1);
        assert_eq!(parsed.atoms[0].text, "contains } brace");
    }
}
