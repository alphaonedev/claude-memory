// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.6.x legacy per-pair contradiction classifier + post-store
//! autonomy-hook metadata update path.
//!
//! #881 (PR-4 extraction): split out of the monolithic
//! `src/mcp/tools/store.rs` so the legacy autonomy-hook branch lives
//! in its own ~140-LOC module. Wire-compat preserved verbatim: every
//! response field, metadata key, and tracing label is byte-identical
//! to the pre-#881 inline code path.
//!
//! Two responsibilities:
//!
//! 1. [`maybe_run_autonomy_hooks`] — fires `auto_tag` synchronously
//!    after `db::insert` lands, then optionally fires the legacy
//!    per-pair contradiction classifier (gated by namespace policy
//!    `legacy_per_pair_classifier = true`). Persists the resulting
//!    `auto_tags` + `confirmed_contradictions` arrays into the
//!    memory's metadata via a follow-up `db::update`.
//!
//! 2. [`autonomy_skip_reason`] — surface the eligibility-gate
//!    short-circuit reason to the response envelope as
//!    `autonomy_hook_skipped: "<reason>"` so callers can distinguish
//!    silent fall-through (`disabled` / `no_llm` / `content_too_short`
//!    / `internal_namespace`) from real LLM failures (which log at
//!    WARN but do not appear in the response).
//!
//! Eligibility (mirrors the pre-#881 inline guard):
//!
//! * `autonomous_hooks` flag must be true
//! * an LLM client must be wired
//! * content must meet [`AUTONOMY_MIN_CONTENT_LEN`] (50 bytes)
//! * namespace must NOT start with `_` (internal/system namespaces
//!   would feed into self-reinforcing loops)

use serde_json::{Value, json};

use crate::llm::OllamaClient;
use crate::models::{GovernancePolicy, Memory};
use crate::{db, hnsw::VectorIndex};

use super::AUTONOMY_MIN_CONTENT_LEN;

/// Eligibility-gate short-circuit reason for the autonomy-hook pass.
/// Returns `None` when the hooks are eligible to run.
pub(super) fn autonomy_skip_reason(
    autonomous_hooks: bool,
    llm_present: bool,
    content_len: usize,
    namespace: &str,
) -> Option<&'static str> {
    if !autonomous_hooks {
        Some("disabled")
    } else if !llm_present {
        Some("no_llm")
    } else if content_len < AUTONOMY_MIN_CONTENT_LEN {
        Some("content_too_short")
    } else if namespace.starts_with('_') {
        Some("internal_namespace")
    } else {
        None
    }
}

/// Outcome of the post-store autonomy-hook pass that the store
/// handler threads back into the response envelope.
pub(super) struct AutonomyHookOutcome {
    pub auto_tags: Vec<String>,
    pub confirmed_contradictions: Vec<String>,
}

/// v0.6.0.0 post-store autonomy hooks. When enabled via
/// `AI_MEMORY_AUTONOMOUS_HOOKS=1` or `autonomous_hooks = true` in
/// config.toml AND an LLM is wired AND the content is long enough
/// to be meaningfully taggable, fires `auto_tag` synchronously and
/// optionally fires the legacy `detect_contradiction` per-pair loop.
/// Persists the results into the memory's metadata.
///
/// Best-effort: any LLM error is logged at WARN and does not fail
/// the store. Skipped silently for internal/system namespaces (the
/// store handler still surfaces the skip reason on the response
/// envelope when applicable).
///
/// The legacy per-pair classifier ONLY runs when the namespace
/// policy opts in via `legacy_per_pair_classifier = true`. Default
/// behaviour routes through the synthesis batch call (see
/// [`super::synthesis`]) and skips this loop entirely. Operators
/// who need the old metadata-only `confirmed_contradictions` field
/// set the policy flag to keep the previous semantics.
pub(super) fn maybe_run_autonomy_hooks(
    conn: &rusqlite::Connection,
    llm: &OllamaClient,
    mem: &Memory,
    actual_id: &str,
    existing: &[Memory],
    ns_policy: &GovernancePolicy,
) -> AutonomyHookOutcome {
    let mut auto_tags: Vec<String> = Vec::new();
    let mut confirmed_contradictions: Vec<String> = Vec::new();

    match llm.auto_tag(&mem.title, &mem.content, None) {
        Ok(tags) => {
            auto_tags = tags.into_iter().take(8).collect();
        }
        Err(e) => {
            tracing::warn!("auto_tag hook failed for {}: {}", actual_id, e);
        }
    }

    // v0.7.x Form 1 — the legacy per-pair binary contradiction
    // classifier ONLY runs when the namespace policy explicitly
    // opts in.
    if ns_policy.effective_legacy_per_pair_classifier() {
        for cand in existing {
            if cand.id == actual_id || cand.id == mem.id {
                continue;
            }
            match llm.detect_contradiction(&mem.content, &cand.content) {
                Ok(true) => confirmed_contradictions.push(cand.id.clone()),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        "detect_contradiction hook failed ({actual_id} vs {}): {e}",
                        cand.id
                    );
                }
            }
        }
    }

    // Persist hook results into metadata. Best-effort — a failed update
    // here does not fail the store (the memory is already committed).
    if !auto_tags.is_empty() || !confirmed_contradictions.is_empty() {
        let mut updated_metadata = mem.metadata.clone();
        if let Some(obj) = updated_metadata.as_object_mut() {
            if !auto_tags.is_empty() {
                obj.insert("auto_tags".to_string(), json!(auto_tags));
            }
            if !confirmed_contradictions.is_empty() {
                obj.insert(
                    "confirmed_contradictions".to_string(),
                    json!(confirmed_contradictions),
                );
            }
        }
        if let Err(e) = db::update(
            conn,
            actual_id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&updated_metadata),
        ) {
            tracing::warn!(
                "autonomy-hook metadata update failed for {}: {}",
                actual_id,
                e
            );
        }
    }

    AutonomyHookOutcome {
        auto_tags,
        confirmed_contradictions,
    }
}

/// Echo the `auto_tags` + `confirmed_contradictions` arrays on the
/// response envelope (when non-empty). Lifted from the inline
/// response-assembly block in `handle_store` so the call site stays
/// readable.
pub(super) fn merge_autonomy_outcome_into_response(
    response: &mut Value,
    outcome: &AutonomyHookOutcome,
) {
    if !outcome.auto_tags.is_empty() {
        response["auto_tags"] = json!(outcome.auto_tags);
    }
    if !outcome.confirmed_contradictions.is_empty() {
        response["confirmed_contradictions"] = json!(outcome.confirmed_contradictions);
    }
}

/// HNSW vector-index marker — kept as a module-local re-export so
/// callers in the store handler don't need a separate `use` line.
/// The compiler removes the alias under `--release`.
#[allow(dead_code)]
pub(super) type Idx = VectorIndex;
