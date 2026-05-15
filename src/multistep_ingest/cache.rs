// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Form 3 — prompt-cache key derivation + shared-prefix builder.
//!
//! The Batman exemplar's OpenKB four-step pipeline shares a SYSTEM
//! PROMPT prefix across every LLM stage so the prompt-cache hits. This
//! module owns the prefix builder and the deterministic key derivation
//! that lets telemetry assert "stages within a run share the cache key".
//!
//! # Cache key
//!
//! [`CacheKey`] is the SHA-256 hash of the SHARED PREFIX bytes. Two LLM
//! calls within the same pipeline run derive the SAME key because the
//! prefix is the same string. Two calls across different pipeline
//! variants derive DIFFERENT keys because the prefix carries the
//! variant tag. This is the substrate-side invariant the acceptance
//! tests pin.
//!
//! # Telemetry
//!
//! [`PromptCacheTelemetry`] is the small recorder the executor threads
//! through every LLM dispatch. The MCP tool surface and the test suite
//! both inspect it to verify cache reuse without having to drive a
//! real Ollama process.

use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// Deterministic prompt-cache key. Wraps a 64-character hex SHA-256
/// digest of the shared-prefix bytes that an LLM stage prepended to its
/// stage-specific prompt body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(pub String);

impl CacheKey {
    /// Derive a key from raw prefix bytes. The caller is responsible
    /// for assembling the prefix; this function just hashes it.
    #[must_use]
    pub fn from_prefix(prefix: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(prefix.as_bytes());
        let digest = hasher.finalize();
        Self(format!("{digest:x}"))
    }

    /// Hex string view (load-bearing for the telemetry JSON dump).
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

/// The Batman explicit-trust phrasing the audit pins. Threaded into
/// every LLM stage's prompt so test assertions on the prompt string
/// have a stable hook. The exact wording mirrors the
/// Understand-Anything six-of-nine exemplar (`Do NOT re-run discovery
/// commands or re-count lines, trust the script's results entirely`)
/// shortened for the prompt context.
pub const EXPLICIT_TRUST_INSTRUCTION: &str = "\
Do NOT re-run discovery. The following pre-computed helper output is \
authoritative; trust it.";

/// Build the shared prefix for an LLM stage. Every stage within a
/// pipeline run uses the SAME `pipeline_variant` + `system_prompt`
/// inputs, which is what keeps the cache key stable. Stage-specific
/// content goes into the body AFTER the prefix and does NOT affect
/// cache reuse.
///
/// Layout:
///
/// ```text
/// [SYSTEM] You are an ingest assistant for the v0.7.0 multi-step
/// ingest substrate (variant=<variant>). <system_prompt>
/// [TRUST INSTRUCTION] Do NOT re-run discovery. ...
/// ```
#[must_use]
pub fn build_shared_prefix(pipeline_variant: &str, system_prompt: &str) -> String {
    format!(
        "[SYSTEM] You are an ingest assistant for the v0.7.0 multi-step ingest \
         substrate (variant={pipeline_variant}). {system_prompt}\n\
         [TRUST INSTRUCTION] {EXPLICIT_TRUST_INSTRUCTION}\n"
    )
}

/// Recorder threaded through every LLM dispatch by the executor. Lets
/// the MCP tool surface and integration tests observe whether stages
/// within a run share the cache key.
#[derive(Debug, Default)]
pub struct PromptCacheTelemetry {
    keys: Mutex<Vec<CacheKey>>,
}

impl PromptCacheTelemetry {
    /// Construct an empty telemetry recorder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keys: Mutex::new(Vec::new()),
        }
    }

    /// Record a cache key (called by the executor before each LLM
    /// dispatch). A poisoned mutex is treated as "drop the record"
    /// rather than panic — telemetry should never wedge the dispatch.
    pub fn record(&self, key: CacheKey) {
        if let Ok(mut g) = self.keys.lock() {
            g.push(key);
        }
    }

    /// Snapshot the recorded keys in observation order. Used by tests
    /// + the MCP tool's response trace.
    #[must_use]
    pub fn snapshot(&self) -> Vec<CacheKey> {
        self.keys.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// `true` if every recorded key is identical. The Form 3
    /// acceptance criterion: stages within a run must share the cache
    /// key. With zero or one recordings the predicate trivially holds.
    #[must_use]
    pub fn all_keys_match(&self) -> bool {
        let snap = self.snapshot();
        match snap.split_first() {
            None => true,
            Some((first, rest)) => rest.iter().all(|k| k == first),
        }
    }

    /// Number of recordings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// `true` if no keys have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_deterministic_for_same_prefix() {
        let a = CacheKey::from_prefix("hello world");
        let b = CacheKey::from_prefix("hello world");
        assert_eq!(a, b);
        assert_eq!(a.as_hex().len(), 64);
    }

    #[test]
    fn cache_key_differs_for_different_prefixes() {
        let a = CacheKey::from_prefix("hello");
        let b = CacheKey::from_prefix("world");
        assert_ne!(a, b);
    }

    #[test]
    fn shared_prefix_includes_trust_instruction_verbatim() {
        let prefix = build_shared_prefix("two_phase", "Summarise.");
        assert!(
            prefix.contains(EXPLICIT_TRUST_INSTRUCTION),
            "prefix must carry the explicit-trust instruction"
        );
        assert!(prefix.contains("variant=two_phase"));
    }

    #[test]
    fn shared_prefix_differs_per_variant() {
        let a = build_shared_prefix("two_phase", "Same.");
        let b = build_shared_prefix("four_step", "Same.");
        assert_ne!(a, b);
        assert_ne!(CacheKey::from_prefix(&a), CacheKey::from_prefix(&b));
    }

    #[test]
    fn telemetry_all_keys_match_holds_for_empty_and_single() {
        let t = PromptCacheTelemetry::new();
        assert!(t.all_keys_match(), "empty telemetry trivially matches");
        t.record(CacheKey::from_prefix("a"));
        assert!(t.all_keys_match(), "single record trivially matches");
    }

    #[test]
    fn telemetry_detects_drift_across_records() {
        let t = PromptCacheTelemetry::new();
        t.record(CacheKey::from_prefix("a"));
        t.record(CacheKey::from_prefix("b"));
        assert!(!t.all_keys_match(), "differing keys should fail the check");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn telemetry_matches_when_every_record_is_identical() {
        let t = PromptCacheTelemetry::new();
        let key = CacheKey::from_prefix("shared");
        t.record(key.clone());
        t.record(key.clone());
        t.record(key);
        assert!(t.all_keys_match());
        assert_eq!(t.len(), 3);
    }
}
