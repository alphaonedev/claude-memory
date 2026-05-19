// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.6.4-005 — Static schema-size table.
//!
//! Computes the per-tool BPE token cost of every MCP tool registered by
//! `crate::mcp::tool_definitions`, using the `tiktoken-rs` `cl100k_base`
//! tokenizer (the same BPE Claude/GPT use for context-window accounting,
//! and the same one v0.6.3.1 P6/R1 already wires for `budget_tokens`).
//!
//! The table is computed lazily on first access and cached behind a
//! `OnceLock`. The cost of the first call is one full pass over every
//! tool schema (~7 ms on Apple M2) followed by cache hits forever after.
//!
//! ## Why lazy and not literally compile-time
//!
//! The "build-time" framing in the v0.6.4 issue spec referred to the
//! desire that operators be able to query the table without running
//! the full MCP `register_tools()` dance — the runtime cache satisfies
//! that constraint. A real build-time approach would need either a
//! proc-macro or a `build.rs` that re-parsed the JSON-emitting Rust
//! source, both of which trade simplicity for marginal warm-cache
//! performance that nobody is paying for here. The lazy approach also
//! keeps the BPE table out of `cargo bench` cold paths — every place
//! that *doesn't* run `doctor --tokens` pays exactly nothing.
//!
//! ## CI gate
//!
//! `tool_sizes_under_ci_gate()` returns the largest single tool cost.
//! The unit test `no_tool_exceeds_1500_tokens` enforces the v0.6.4-005
//! acceptance gate that no individual tool definition exceeds 1500
//! tokens. The number is high enough to permit growth on the more
//! schema-heavy KG/governance tools and low enough that doubling a
//! tool's schema by accident lands in CI red.

use std::sync::OnceLock;

use serde_json::Value;
use tiktoken_rs::CoreBPE;

/// Single-tool cost report. The `total` is what counts against the
/// per-request prefix; the `name_tokens` and `schema_tokens` split is
/// useful for the doctor's diagnostic output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSize {
    pub name: String,
    pub schema_tokens: usize,
    pub name_tokens: usize,
    pub total_tokens: usize,
}

/// Runtime-computed table of every tool's tokenized schema cost
/// **at the verbose ceiling** — every optional param, every default,
/// every per-property description. This is the upper bound a host
/// can ever pay (only reachable via
/// `memory_capabilities { verbose=true, family=…, include_schema=true }`
/// since v0.7 C4).
///
/// Returns a static slice on every call after the first invocation
/// (which performs the one-time BPE pass).
pub fn tool_sizes() -> &'static [ToolSize] {
    static TABLE: OnceLock<Vec<ToolSize>> = OnceLock::new();
    TABLE.get_or_init(|| compute_table(false)).as_slice()
}

/// v0.7 C4 + #859 — runtime-computed table of every tool's tokenized
/// schema cost **as actually shipped on `tools/list`**. Per-property
/// `description` prose is stripped, the top-level tool `description`
/// is compacted to the first sentence, the `docs` field is dropped,
/// but every property entry survives so MCP clients can discover the
/// call surface (per [`crate::mcp::trim_optional_params`] +
/// [`crate::mcp::strip_docs_from_tools`] + `wire_compact_descriptions`).
/// This is what an MCP host pays per request on the default code path.
///
/// **Wire-form invariant.** This table is computed by feeding the
/// output of [`crate::mcp::tool_definitions_for_profile`] (full
/// profile) into the cl100k_base tokenizer; the budget gate at
/// `tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_post_859_ceiling`
/// pins the sum at ≤ 5000 cl100k tokens (post-#859 floor; was 3500
/// pre-#859 when the trim hid optional property keys entirely).
pub fn trimmed_tool_sizes() -> &'static [ToolSize] {
    static TABLE: OnceLock<Vec<ToolSize>> = OnceLock::new();
    TABLE.get_or_init(|| compute_table(true)).as_slice()
}

/// Highest-cost tool in the verbose table. Used by the CI gate.
pub fn tool_sizes_under_ci_gate() -> usize {
    tool_sizes()
        .iter()
        .map(|t| t.total_tokens)
        .max()
        .unwrap_or(0)
}

/// Sum of every tool's `total_tokens` (verbose schema) — the
/// worst-case prefix cost on a `verbose=true` opt-in harness with
/// `--profile full`. The actually-paid cost on the default code path
/// is reported by [`trimmed_full_profile_total_tokens`].
pub fn full_profile_total_tokens() -> usize {
    tool_sizes().iter().map(|t| t.total_tokens).sum()
}

/// v0.7 C4 — sum of every tool's `total_tokens` after the C4 trim
/// (optionals hidden). This is the bare `tools/list` payload cost
/// under `--profile full`.
pub fn trimmed_full_profile_total_tokens() -> usize {
    trimmed_tool_sizes().iter().map(|t| t.total_tokens).sum()
}

/// Lookup a single tool by name in the verbose table. `O(n)` but
/// `n ≤ 57` (v0.7.0 L1-5 added 5 skill tools).
pub fn tool_size(name: &str) -> Option<&'static ToolSize> {
    tool_sizes().iter().find(|t| t.name == name)
}

fn compute_table(trimmed: bool) -> Vec<ToolSize> {
    let bpe = bpe();
    // #859 — to keep the budget model in lockstep with the actually-
    // shipped wire payload, delegate to `tool_definitions_for_profile`
    // for the trimmed case (which now performs the full wire shape:
    // properties preserved, per-property prose stripped, top-level
    // description compacted). For the verbose case we measure the raw
    // `tool_definitions()` table as it would appear on the
    // `memory_capabilities { verbose=true }` opt-in path.
    let defs = if trimmed {
        crate::mcp::tool_definitions_for_profile(&crate::profile::Profile::full())
    } else {
        crate::mcp::tool_definitions()
    };
    let tools = defs
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    tools
        .into_iter()
        .filter_map(|tool| size_one_tool(&bpe, &tool))
        .collect()
}

fn size_one_tool(bpe: &CoreBPE, tool: &Value) -> Option<ToolSize> {
    let name = tool.get("name").and_then(Value::as_str)?.to_string();
    // The cost the host pays is the serialized JSON of the entire tool
    // object — name + description + inputSchema. We use the canonical
    // serde_json serialization (no pretty-printing) because that is
    // what every MCP host transmits over stdio.
    let schema_json = serde_json::to_string(tool).ok()?;
    let schema_tokens = bpe.encode_with_special_tokens(&schema_json).len();
    let name_tokens = bpe.encode_with_special_tokens(&name).len();
    Some(ToolSize {
        name,
        schema_tokens,
        name_tokens,
        total_tokens: schema_tokens,
    })
}

fn bpe() -> CoreBPE {
    // We construct a fresh BPE on each compute_table call (only ever
    // called once) because `cl100k_base` returns an owned `CoreBPE`
    // and stashing it forever in a static would leak ~1.7 MB for a
    // table that only gets walked at startup. Cheap to throw away.
    tiktoken_rs::cl100k_base().expect("cl100k_base BPE table embedded in tiktoken-rs")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI gate per v0.6.4-005 acceptance criteria. If any tool's schema
    /// crosses 1500 tokens, the whole build fails. The number is roughly
    /// 2.5× today's largest tool (memory_store at ~620 tokens) so we have
    /// runway, but not so much runway that a 3× regression slips through.
    #[test]
    fn no_tool_exceeds_1500_tokens() {
        let max = tool_sizes_under_ci_gate();
        assert!(
            max <= 1500,
            "v0.6.4-005 CI gate: largest tool schema is {max} tokens (limit: 1500). \
             Inspect `cargo run -- doctor --tokens --raw-table` to find the offender."
        );
    }

    /// Sanity: the table must be populated. Catches accidental empty
    /// `tool_definitions()` regressions that would silently hide other
    /// failures.
    #[test]
    fn table_has_51_entries_matching_tool_definitions_count() {
        // v0.7.0 refactor PR-2 (#793) — tool-count SSOT. Anchor the
        // assertion on `Profile::full().expected_tool_count()` rather
        // than a hardcoded literal so adding a new MCP tool touches
        // ONE site (the per-Family `expected_tool_count` arm) instead
        // of N hardcoded assertions across the codebase.
        let n = tool_sizes().len();
        let expected = crate::profile::Profile::full().expected_tool_count();
        assert_eq!(
            n, expected,
            "expected exactly {expected} tools (v0.6.3.1 baseline 43 + v0.7.0 I4 \
             `memory_replay` + v0.7 H4 `memory_verify` + v0.7 B1 \
             `memory_load_family` + v0.7 B2 `memory_smart_load` + v0.7 K7 \
             `memory_subscription_replay` + `memory_subscription_dlq_list` \
             + v0.7 J7 `memory_find_paths` + v0.7 K8 `memory_quota_status` \
             + v0.7.0 Task 4/8 `memory_reflect` + v0.7.0 L2-2 \
             `memory_reflection_origin` + v0.7.0 L2-3 \
             `memory_dependents_of_invalidated` + v0.7.0 issue #691 \
             `memory_check_agent_action` + `memory_rule_list` + v0.7.0 L1-5 \
             `memory_skill_register` + `memory_skill_list` + \
             `memory_skill_get` + `memory_skill_resource` + \
             `memory_skill_export` + v0.7.0 L2-6 \
             `memory_skill_promote_from_reflection` + v0.7.0 L2-7 \
             `memory_skill_compositional_context` + v0.7.0 QW-1 \
             `memory_export_reflection` + v0.7.0 QW-3 follow-up \
             `memory_offload` + `memory_deref` + v0.7.0 WT-1-C \
             `memory_atomise` + v0.7.0 QW-2 `memory_persona` + \
             `memory_persona_generate` + v0.7.0 Form 3 \
             `memory_ingest_multistep` + v0.7.0 Form 5 (issue #758) \
             `memory_calibrate_confidence`, source-anchored at \
             src/mcp.rs::tool_definitions); got {n}. If the count changed, \
             update the family map and this assertion together."
        );
    }

    /// Every tool should have non-zero name + schema costs. Zero would
    /// mean either an empty schema or a tokenizer wiring break.
    #[test]
    fn every_tool_has_nonzero_cost() {
        for t in tool_sizes() {
            assert!(t.schema_tokens > 0, "tool {} schema_tokens = 0", t.name);
            assert!(t.name_tokens > 0, "tool {} name_tokens = 0", t.name);
        }
    }

    /// Full-profile total cost — measured against `cl100k_base` (the
    /// tokenizer Claude / GPT actually use for input accounting).
    ///
    /// **Truthfulness note (v0.6.4-005, 2026-05-04):** the v0.6.4 RFC
    /// claimed ~25,800 tokens for the full surface, derived from "~600
    /// tokens/tool × 43" measured against MiniLM. MiniLM is a sentence-
    /// embedding vocabulary (~30K tokens) that systematically over-counts
    /// JSON by ~4× vs. `cl100k_base` (100K-token chat-completion BPE).
    /// The actual measured cost in `cl100k_base` is ~6,000 tokens for
    /// the full surface — still material, still worth the v0.6.4 ship,
    /// but the public claims need a 4× downward correction (tracked in
    /// v0.6.4-014 + v0.6.4-015 docs work).
    ///
    /// **v0.7 C2 update (2026-05-06):** the canonical
    /// `tool_definitions()` now carries an additional per-tool `docs`
    /// field (long-form description + examples) that the bare
    /// `tools/list` payload strips before transmission. The numbers
    /// in this table reflect the **source of truth** (verbose +
    /// short), not the wire payload. The bare-wire C5 budget is
    /// pinned separately at ≤ 3500 tokens by
    /// `tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_post_859_ceiling`.
    /// The savings *percentage* from `core` is unchanged; the
    /// always-on payload is now ~85% smaller than the source.
    #[test]
    fn full_profile_total_in_honest_measured_range() {
        let total = full_profile_total_tokens();
        // **v0.7.0 #829 update.** Prior bound was 5K..=16K to soak the
        // multi-paragraph `docs` prose that every tool carried. After
        // the #829 trim every `docs` field is a single condensed
        // sentence with issue refs + tier annotations preserved, so
        // the verbose total settles at ~9500 tokens. The hard ceiling
        // is pinned at 10K by `tests/token_budget_guard.rs`; this
        // honest-range assertion tracks the same number, with a 5K
        // floor to catch a wiring break that drops the catalog
        // entirely.
        assert!(
            (5_000..=10_000).contains(&total),
            "full-profile total {total} tokens is outside the measured \
             cl100k_base range (5K-10K, post-#829 trim). If the schema \
             grew intentionally, update `tests/token_budget_guard.rs::\
             VERBOSE_FULL_PROFILE_CEILING_TOKENS` AND this bound together."
        );
    }

    /// Lookup by name should resolve a known tool.
    #[test]
    fn tool_size_resolves_memory_store() {
        let t = tool_size("memory_store").expect("memory_store should exist");
        assert!(t.total_tokens > 0);
        assert!(t.total_tokens < 1500);
    }

    /// Lookup of a nonexistent tool should return None, not panic.
    #[test]
    fn tool_size_returns_none_for_unknown() {
        assert!(tool_size("memory_does_not_exist_42").is_none());
    }

    /// v0.7 C4 + #859 acceptance gate: the trimmed `tools/list`
    /// payload (the shape an MCP host actually receives by default)
    /// must be materially smaller than the verbose ceiling AND must
    /// stay under the post-#859 5000-token wire-form budget.
    ///
    /// **History.** Pre-#859 baseline: trimmed ≈ 3456 tokens,
    /// verbose ≈ 7416 tokens (~53% saved). The trim dropped every
    /// optional property entry from the wire, hiding the call
    /// surface from MCP clients. #859 (v0.7.0 fix) restored every
    /// property entry on the wire (keeping per-property `description`
    /// prose stripped + the top-level tool description compacted to
    /// the first sentence) so NHI agents can discover what knobs
    /// exist. Post-#859: trimmed ≈ 4500-4700 tokens, verbose ≈ 9500.
    ///
    /// The savings now sit at ~50% (down from ~53%) because the
    /// property metadata that pre-fix lived only in the verbose
    /// catalog now also appears on the wire. The 5000 ceiling pins
    /// the post-#859 floor with ~300 tokens of headroom for future
    /// tool additions; the 25% lower bound on `saved_pct` keeps the
    /// trim itself honest (a regression that re-bloated the wire
    /// path with docs / per-property prose would still trip).
    #[test]
    fn trimmed_full_profile_total_under_post_859_ceiling() {
        let trimmed = trimmed_full_profile_total_tokens();
        let verbose = full_profile_total_tokens();
        assert!(
            trimmed < verbose,
            "trimmed total ({trimmed}) must be strictly smaller than verbose ({verbose})"
        );
        let saved_pct = (verbose - trimmed) as f64 / verbose as f64 * 100.0;
        assert!(
            saved_pct >= 25.0,
            "trim should save >=25% of full-profile tokens; got {saved_pct:.1}% \
             (verbose={verbose}, trimmed={trimmed}). Audit `strip_docs_from_tools` and \
             `wire_compact_descriptions` — if those broke the trim itself regressed."
        );
        assert!(
            trimmed <= 5_000,
            "post-#859 trimmed full-profile total {trimmed} > 5000-token ceiling. \
             The #859 fix preserves every property entry on the wire — if a tool's \
             schema grew, audit per-property `description` prose (must be stripped) \
             and consider routing the new tool to `family=power` instead of the \
             always-on core."
        );
    }

    /// Trim must shrink at least one optional from at least one tool;
    /// otherwise the wiring is broken (e.g. `trim_optional_params` got
    /// short-circuited or the keep-list went global).
    #[test]
    fn trimmed_table_strictly_smaller_per_tool_where_optionals_existed() {
        let verbose: std::collections::HashMap<&str, usize> = tool_sizes()
            .iter()
            .map(|t| (t.name.as_str(), t.total_tokens))
            .collect();
        let mut at_least_one_smaller = false;
        for trimmed_tool in trimmed_tool_sizes() {
            let v = verbose
                .get(trimmed_tool.name.as_str())
                .copied()
                .unwrap_or(0);
            assert!(
                trimmed_tool.total_tokens <= v,
                "{} grew under trim ({} > {})",
                trimmed_tool.name,
                trimmed_tool.total_tokens,
                v
            );
            if trimmed_tool.total_tokens < v {
                at_least_one_smaller = true;
            }
        }
        assert!(
            at_least_one_smaller,
            "trim should shrink at least one tool; none did — wiring is broken"
        );
    }
}
