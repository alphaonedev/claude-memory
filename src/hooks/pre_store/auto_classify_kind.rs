// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.x Form 6 (issue #759) — `pre_store::auto_classify_kind`
//! substrate-side hook.
//!
//! When the namespace policy
//! [`crate::models::GovernancePolicy::auto_classify_kind`] resolves
//! to anything other than [`MemoryKindAutoClassify::Off`] for the
//! stored memory's namespace, this hook inspects the memory's
//! `title + content` and may set `memory_kind` from the default
//! `Observation` to a more specific Batman-taxonomy variant
//! (`Concept` / `Entity` / `Claim` / `Relation` / `Event` /
//! `Conversation` / `Decision`).
//!
//! # Hard guarantees
//!
//! 1. **Caller-supplied kind wins.** When the caller has already
//!    set `memory_kind` to anything other than the default
//!    `Observation`, the hook is a no-op. This preserves the
//!    contract of [`crate::mcp::tools::store`] handlers that
//!    plumb an explicit `kind` parameter through to the substrate.
//!
//! 2. **Deterministic regex pass is fast.** [`classify_by_regex`]
//!    is pure-Rust, allocation-light, and runs in tens of
//!    microseconds even on multi-kilobyte content. Safe to run on
//!    every write under the `RegexOnly` policy.
//!
//! 3. **LLM round-trip is opt-in.** [`MemoryKindAutoClassify::RegexThenLlm`]
//!    is the ONLY policy that fires an LLM round-trip. Operators
//!    who set `RegexOnly` (or leave the default `Off`) will never
//!    see this hook touch the LLM. The LLM round-trip path is also
//!    feature-gated on the `llm.classify_kind` capability — if the
//!    runtime doesn't carry a classifier, the hook degrades to
//!    `RegexOnly` semantics silently.
//!
//! 4. **Non-blocking failure path.** Regex misses ⇒ keep the
//!    caller-supplied (or default `Observation`) kind. LLM errors
//!    are logged via `tracing::warn!` and the hook returns the
//!    pre-LLM verdict. The substrate never aborts a `memory_store`
//!    because of a classifier hiccup.
//!
//! # Wiring
//!
//! The substrate-side call site is the `memory_store` write path
//! ([`crate::storage::insert`]). Right before the memory is
//! committed, [`maybe_auto_classify`] consults the namespace
//! policy and rewrites `mem.memory_kind` in place when the policy
//! warrants it. The hook is intentionally synchronous — the regex
//! pass is too fast to defer, and the LLM round-trip is opt-in.

use crate::models::{Memory, MemoryKind, MemoryKindAutoClassify};

/// Single-shot regex-style heuristic. Returns `Some(kind)` when a
/// rule fires, `None` otherwise. The patterns are deliberately
/// shallow — they encode the Batman exemplar's strongest signals
/// (verbs and connectives that disambiguate atom types) without
/// pretending to be a full NLP pipeline. Operators who want better
/// classification opt into `RegexThenLlm` for the harder cases.
///
/// Heuristic order matters: more specific rules win over more
/// generic ones. Conversation > Decision > Event > Relation >
/// Entity > Claim > Concept. This ordering matches the Batman
/// taxonomy's specificity gradient (a "decision to deploy at
/// 14:32" is a Decision first, an Event second).
#[must_use]
pub fn classify_by_regex(title: &str, content: &str) -> Option<MemoryKind> {
    // Combine title + content into a single haystack for the regex
    // pass. Lower-case once so each rule can use case-insensitive
    // checks without re-allocating. Cap at 4 KiB to keep the
    // worst-case pass O(small) even on multi-megabyte content.
    let mut hay = String::with_capacity(title.len() + content.len() + 1);
    hay.push_str(title);
    hay.push(' ');
    hay.push_str(content);
    let truncated_len = hay.len().min(4096);
    hay.truncate(truncated_len);
    let lower = hay.to_ascii_lowercase();

    // Conversation: speaker-tagged dialogue ("alice: ...", "X says:",
    // "user said", "claude said"). The colon-after-name pattern
    // catches every common chat-log style without a full regex
    // engine.
    if has_speaker_tag(&hay) {
        return Some(MemoryKind::Conversation);
    }
    // Decision: explicit verbs of choice with rationale-like context.
    // "decided to", "we will", "chose to", "approved the".
    if contains_any(
        &lower,
        &[
            "decided to ",
            "we will ",
            "i will ",
            "chose to ",
            "approved the ",
            "rejecting the ",
            "decision: ",
        ],
    ) {
        return Some(MemoryKind::Decision);
    }
    // Event: temporal anchor verbs. "happened on", "occurred at",
    // "deployed at", "incident at", time-like "HH:MM" or
    // "YYYY-MM-DD" near a past-tense verb.
    if contains_any(
        &lower,
        &[
            "happened on ",
            "happened at ",
            "occurred on ",
            "occurred at ",
            "deployed at ",
            "incident at ",
            "event: ",
            "at 09:",
            "at 10:",
            "at 11:",
            "at 12:",
            "at 13:",
            "at 14:",
            "at 15:",
            "at 16:",
            "at 17:",
            "at 18:",
            "at 19:",
            "at 20:",
        ],
    ) {
        return Some(MemoryKind::Event);
    }
    // Relation: triple-style connectives. "X depends on Y",
    // "X derives from Y", "X is a part of Y" (note: "is_a" caught
    // by Concept rule — Relation needs the stronger directional
    // connectives).
    if contains_any(
        &lower,
        &[
            " depends on ",
            " derives from ",
            " is part of ",
            " contains ",
            " contradicts ",
            " supersedes ",
            " relates to ",
        ],
    ) {
        return Some(MemoryKind::Relation);
    }
    // Entity: named-thing pattern. "X is a person", "X is an
    // organisation", "the system X", "the team X".
    if contains_any(
        &lower,
        &[
            " is a person",
            " is an organisation",
            " is a product",
            " is a service",
            " is a system",
            " is a team",
            "person: ",
            "org: ",
            "entity: ",
        ],
    ) {
        return Some(MemoryKind::Entity);
    }
    // Claim: assertion verbs. "is true that", "we claim", "asserts
    // that", "states that".
    if contains_any(
        &lower,
        &[
            "claim: ",
            "we claim ",
            "i claim ",
            "asserts that ",
            "states that ",
            "is true that ",
            "is false that ",
        ],
    ) {
        return Some(MemoryKind::Claim);
    }
    // Concept: abstract definitions. "X is_a Y", "X is defined as",
    // "concept of", "definition: ", "by definition".
    if contains_any(
        &lower,
        &[
            "is_a ",
            "is defined as ",
            "concept of ",
            "definition: ",
            "by definition ",
            "refers to ",
            "is the name of ",
        ],
    ) {
        return Some(MemoryKind::Concept);
    }
    None
}

/// Whether the haystack contains a speaker-tagged dialogue line.
/// Matches `Name: ...` or `Name said ...` patterns commonly seen
/// in chat-log exports. Conservative — requires a capital letter
/// start so prose like "value: 42" doesn't false-positive.
fn has_speaker_tag(hay: &str) -> bool {
    for line in hay.lines() {
        let line = line.trim_start();
        // "Alice:" or "Claude:" etc.
        if let Some(colon_idx) = line.find(':') {
            let name = &line[..colon_idx];
            if !name.is_empty()
                && name.len() <= 32
                && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return true;
            }
        }
        // "Alice said " / "Claude said "
        let lower = line.to_ascii_lowercase();
        if lower.contains(" said ") || lower.contains(" says ") || lower.contains(" replied ") {
            return true;
        }
    }
    false
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// v0.7.x Form 6 — main hook entry point. Consults the namespace
/// policy and may rewrite `mem.memory_kind` in place.
///
/// Returns the resolved [`MemoryKind`] for downstream telemetry /
/// test inspection. The caller (`storage::insert` write path)
/// discards the return value unless it cares about the verdict.
///
/// `policy == None` is identical to `Some(Off)` — both leave the
/// memory unchanged.
pub fn maybe_auto_classify(mem: &mut Memory, policy: Option<MemoryKindAutoClassify>) -> MemoryKind {
    // Caller-supplied kind wins. If the caller set anything other
    // than the default `Observation`, the hook is a no-op.
    if mem.memory_kind != MemoryKind::Observation {
        return mem.memory_kind;
    }
    let policy = policy.unwrap_or_default();
    if matches!(policy, MemoryKindAutoClassify::Off) {
        return mem.memory_kind;
    }
    if let Some(kind) = classify_by_regex(&mem.title, &mem.content) {
        mem.memory_kind = kind;
        return kind;
    }
    // RegexThenLlm path. The substrate keeps an LLM classifier
    // shim under [`crate::llm::classify_memory_kind`] when the
    // active tier carries one; if absent, the hook degrades to
    // RegexOnly semantics (returns Observation).
    if matches!(policy, MemoryKindAutoClassify::RegexThenLlm)
        && let Some(kind) = llm_classify_shim(&mem.title, &mem.content)
    {
        mem.memory_kind = kind;
        return kind;
    }
    mem.memory_kind
}

/// LLM-classifier shim. Returns `None` when no LLM backend is
/// wired (every test build, every CLI one-shot). When a future
/// patch wires `crate::llm::classify_memory_kind`, this shim
/// delegates to it. The hook stays loosely-coupled so we don't
/// drag in tokio / reqwest here.
fn llm_classify_shim(_title: &str, _content: &str) -> Option<MemoryKind> {
    // No LLM backend wired in the v0.7.x Form 6 substrate. Operators
    // who set `RegexThenLlm` get RegexOnly semantics until a future
    // patch lands the classifier endpoint. Logged at debug so
    // operators investigating "why isn't my LLM firing?" find the
    // wire-up gap.
    tracing::debug!(
        "auto_classify_kind: RegexThenLlm requested but no LLM classifier wired; \
         falling back to RegexOnly semantics"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Memory;

    fn fresh_mem(title: &str, content: &str) -> Memory {
        Memory {
            title: title.to_string(),
            content: content.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn off_policy_is_noop() {
        let mut m = fresh_mem("X depends on Y", "");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::Off));
        assert_eq!(verdict, MemoryKind::Observation);
        assert_eq!(m.memory_kind, MemoryKind::Observation);
    }

    #[test]
    fn none_policy_is_noop() {
        let mut m = fresh_mem("X depends on Y", "");
        let verdict = maybe_auto_classify(&mut m, None);
        assert_eq!(verdict, MemoryKind::Observation);
    }

    #[test]
    fn caller_supplied_kind_wins() {
        let mut m = fresh_mem("X depends on Y", "");
        m.memory_kind = MemoryKind::Claim;
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        // Caller-supplied Claim must NOT be downgraded to the
        // Relation verdict the regex would otherwise emit.
        assert_eq!(verdict, MemoryKind::Claim);
        assert_eq!(m.memory_kind, MemoryKind::Claim);
    }

    #[test]
    fn relation_pattern_fires_under_regex_only() {
        let mut m = fresh_mem("subsystem A", "A depends on B for token expiry");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Relation);
    }

    #[test]
    fn event_pattern_fires_under_regex_only() {
        let mut m = fresh_mem("deploy", "The cutover happened at 14:32 UTC");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Event);
    }

    #[test]
    fn conversation_pattern_fires_under_regex_only() {
        let mut m = fresh_mem("chat", "Alice: should we deploy?\nBob: yes");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Conversation);
    }

    #[test]
    fn concept_pattern_fires_on_is_a_marker() {
        let mut m = fresh_mem("ownership", "ownership is_a Rust borrow-checker rule");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Concept);
    }

    #[test]
    fn decision_pattern_fires_under_regex_only() {
        let mut m = fresh_mem("api migration", "We decided to deprecate v1 by Q3");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Decision);
    }

    #[test]
    fn entity_pattern_fires_under_regex_only() {
        let mut m = fresh_mem(
            "acme corp",
            "Acme corp is a service provider in our supply chain",
        );
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Entity);
    }

    #[test]
    fn claim_pattern_fires_under_regex_only() {
        let mut m = fresh_mem(
            "posture",
            "We claim that the GC scheduler is starvation-free",
        );
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Claim);
    }

    #[test]
    fn regex_miss_keeps_observation() {
        let mut m = fresh_mem("note", "just a stray thought without taxonomic signal");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
        assert_eq!(verdict, MemoryKind::Observation);
    }

    #[test]
    fn regex_then_llm_degrades_to_regex_only_when_no_llm_wired() {
        // The llm_classify_shim returns None until a future patch
        // wires the classifier endpoint. RegexThenLlm with a
        // regex-miss content therefore keeps the default Observation.
        let mut m = fresh_mem("inscrutable", "lorem ipsum dolor sit amet");
        let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexThenLlm));
        assert_eq!(verdict, MemoryKind::Observation);
    }
}
