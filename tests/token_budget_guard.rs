// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #829 — token-budget regression guard.
//!
//! Pins both the verbose (full-profile / `memory_capabilities {
//! verbose=true }`) and the trimmed wire (`tools/list`) totals so a
//! future tool-definition edit can't silently re-bloat the catalog
//! past the operator-agreed budgets.
//!
//! ## Why two guards
//!
//! - **Verbose total** (`full_profile_total_tokens`) is what an
//!   operator pays on the verbose drill-down path. It must stay under
//!   **10000 cl100k tokens** so the worst-case verbose payload still
//!   fits comfortably inside any modern context window's
//!   tool-definitions prefix budget.
//! - **Trimmed total** (`trimmed_full_profile_total_tokens`) is what
//!   every MCP host pays per session on the default `tools/list`
//!   path. It must stay under the post-#859 **5000 cl100k token**
//!   ceiling. (The pre-#859 baseline of 3500 cl100k tokens was
//!   structurally lower because optional property entries were
//!   dropped from the wire — a behaviour that #859 reverted to
//!   restore NHI runtime discovery; see
//!   `tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_post_859_ceiling`
//!   for the full history.)
//!
//! Both guards trip on any regression so a casual schema edit can't
//! land without an explicit budget review. If a guard fires:
//!
//! 1. Run `ai-memory doctor --tokens --raw-table` to find the
//!    offender.
//! 2. If the growth is intentional (e.g. a new tool legitimately
//!    needs the prose), bump the bound in this file AND update the
//!    "Proposed fix" memory under issue #829 with the new floor.
//! 3. If the growth is accidental, trim the offending tool's `docs`
//!    field or per-property `description` text in
//!    `src/mcp/registry.rs`.

use ai_memory::sizes::{full_profile_total_tokens, trimmed_full_profile_total_tokens};

/// Hard ceiling for the verbose source-of-truth catalog.
///
/// Source of truth for the figure: the v0.7.0 #829 playbook (operator
/// target). The pre-#829 measured baseline was ~15570 cl100k tokens
/// (every tool carried multi-paragraph `docs` prose); after the #829
/// trim every `docs` string is a single condensed sentence, dropping
/// the verbose total to ~9500 with ~500 tokens of headroom under this
/// cap.
const VERBOSE_FULL_PROFILE_CEILING_TOKENS: usize = 10_000;

/// Hard ceiling for the trimmed wire (`tools/list`) catalog.
///
/// Pinned at the post-#859 ceiling — see the file-level docs above
/// for why this is 5000 (not the original playbook target of 3500).
/// The `c2_tools_list_token_budget_is_under_post_859_ceiling` test
/// already pins the same number for the wire-form payload; this
/// guard's job is to keep the runtime-computed model
/// (`trimmed_tool_sizes`) in lockstep.
const TRIMMED_FULL_PROFILE_CEILING_TOKENS: usize = 5_000;

#[test]
fn issue_829_verbose_full_profile_total_under_ceiling() {
    let total = full_profile_total_tokens();
    assert!(
        total <= VERBOSE_FULL_PROFILE_CEILING_TOKENS,
        "#829 regression: verbose full-profile total {total} cl100k tokens \
         exceeds the {VERBOSE_FULL_PROFILE_CEILING_TOKENS}-token ceiling. \
         Run `cargo run -- doctor --tokens --raw-table` to find the \
         offender and trim its `docs` field in `src/mcp/registry.rs`. \
         The pre-#829 baseline was ~15570 tokens; if the new growth is \
         intentional, bump the bound here AND document the new floor in \
         the #829 follow-up."
    );
}

#[test]
fn issue_829_trimmed_full_profile_total_under_ceiling() {
    let total = trimmed_full_profile_total_tokens();
    assert!(
        total <= TRIMMED_FULL_PROFILE_CEILING_TOKENS,
        "#829 regression: trimmed full-profile total {total} cl100k tokens \
         exceeds the {TRIMMED_FULL_PROFILE_CEILING_TOKENS}-token wire-form \
         ceiling. The wire payload now preserves every property entry (per \
         #859) for NHI discovery — to reclaim headroom, trim per-property \
         `description` prose, shorten the top-level tool `description` \
         (compacted to first sentence on the wire), or route a new tool \
         under `family=power` instead of the always-on core. Cross-check \
         with `tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_post_859_ceiling` \
         which pins the same ceiling against the wire-form serializer."
    );
}

/// Cross-check: trimmed must be strictly smaller than verbose. If
/// they're equal, the trim wiring is broken (e.g. `tool_definitions_for_profile`
/// short-circuited the `wire_compact_descriptions` /
/// `trim_optional_params` pipeline).
#[test]
fn issue_829_trimmed_strictly_smaller_than_verbose() {
    let trimmed = trimmed_full_profile_total_tokens();
    let verbose = full_profile_total_tokens();
    assert!(
        trimmed < verbose,
        "trim wiring broken: trimmed {trimmed} >= verbose {verbose}. \
         Audit `tool_definitions_for_profile` — the wire-compact + \
         optional-trim pipeline should always shrink the verbose total."
    );
}
