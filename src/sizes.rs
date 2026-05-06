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

/// v0.7 C4 — runtime-computed table of every tool's tokenized schema
/// cost **as actually shipped on `tools/list`**. Optional params are
/// stripped (per [`crate::mcp::trim_optional_params`]); only required
/// params + the C4 keep-list (`namespace`, `format`) remain. This is
/// what an MCP host pays per request on the default code path.
///
/// v0.7 C2 rebase note (2026-05-06): the wire payload also strips the
/// per-tool `docs` field (long-form prose) and per-property
/// `description` strings via [`crate::mcp::strip_docs_from_tools`],
/// so this table mirrors that double-strip. Keeping the model in
/// lockstep with `tool_definitions_for_profile` is the only way the
/// `trimmed_full_profile_total_tokens()` reading agrees with the C5
/// budget gate (`tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_3500`).
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
/// `n ≤ 51` (v0.7 K8).
pub fn tool_size(name: &str) -> Option<&'static ToolSize> {
    tool_sizes().iter().find(|t| t.name == name)
}

fn compute_table(trimmed: bool) -> Vec<ToolSize> {
    let bpe = bpe();
    let mut defs = crate::mcp::tool_definitions();
    if trimmed {
        // v0.7 C4 — drop optional inputSchema properties (keep required +
        // C4 allow-list).
        crate::mcp::trim_optional_params(&mut defs);
    }
    let mut tools = defs
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if trimmed {
        // v0.7 C2 rebase — also drop the per-tool `docs` field and the
        // per-property `description` prose, matching the bare
        // `tools/list` payload produced by
        // `crate::mcp::tool_definitions_for_profile`. Without this the
        // trimmed total double-counts the long-form prose that the wire
        // never actually carries on the default path.
        crate::mcp::strip_docs_from_tools(&mut tools);
    }

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
        let n = tool_sizes().len();
        assert_eq!(
            n, 51,
            "expected exactly 51 tools (v0.6.3.1 baseline 43 + v0.7.0 I4 \
             `memory_replay` + v0.7 H4 `memory_verify` + v0.7 B1 \
             `memory_load_family` + v0.7 B2 `memory_smart_load` + v0.7 K7 \
             `memory_subscription_replay` + `memory_subscription_dlq_list` \
             + v0.7 J7 `memory_find_paths` + v0.7 K8 `memory_quota_status`, \
             source-anchored at src/mcp.rs::tool_definitions); got {n}. If \
             the count changed, update the family map and this assertion together."
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
    /// `tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_3500`.
    /// The savings *percentage* from `core` is unchanged; the
    /// always-on payload is now ~85% smaller than the source.
    #[test]
    fn full_profile_total_in_honest_measured_range() {
        let total = full_profile_total_tokens();
        assert!(
            (5_000..=10_000).contains(&total),
            "full-profile total {total} tokens is outside the measured \
             cl100k_base range (5K–10K, source-of-truth incl. v0.7 C2 \
             `docs` fields). If the schema grew, update the public \
             claim in RFC/README/roadmap and adjust this bound."
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

    /// v0.7 C4 acceptance gate: the trimmed `tools/list` payload (the
    /// shape an MCP host actually receives by default) must be
    /// materially smaller than the verbose ceiling AND must save at
    /// least ~30% of the bytes the host used to pay. Original v0.7 C4
    /// baseline (pre-C2): verbose ≈ 7416 tokens, trimmed ≈ 4545
    /// tokens (~39% saved). After the v0.7 C2 rebase (2026-05-06)
    /// added per-tool `docs` strings and the trim model in
    /// `compute_table(true)` was extended to also call
    /// `strip_docs_from_tools` — matching what
    /// `tool_definitions_for_profile` ships on the wire — the verbose
    /// ceiling rises (docs is now part of the source of truth) while
    /// the trimmed figure shrinks (docs + per-property prose is
    /// dropped on the wire). The pinned C5 budget gate
    /// (`tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_3500`)
    /// keeps the wire payload itself ≤ 3500 cl100k tokens, so the
    /// per-tool sum here lands well under the 5_000 soft ceiling.
    ///
    /// The bound is set at ≤ 5_000 so a few prose-heavy required-param
    /// descriptions can grow before this trips, but tight enough that
    /// doubling the keep-list lands red. The aspirational ~2500-token
    /// target from the v0.7 C4 spec lives further down the C-track.
    #[test]
    fn trimmed_full_profile_total_under_c4_target() {
        let trimmed = trimmed_full_profile_total_tokens();
        let verbose = full_profile_total_tokens();
        assert!(
            trimmed < verbose,
            "trimmed total ({trimmed}) must be strictly smaller than verbose ({verbose})"
        );
        let saved_pct = (verbose - trimmed) as f64 / verbose as f64 * 100.0;
        assert!(
            saved_pct >= 30.0,
            "v0.7 C4 trim should save >=30% of full-profile tokens; got {saved_pct:.1}% \
             (verbose={verbose}, trimmed={trimmed}). Audit C4_KEEP_OPTIONAL_PARAMS."
        );
        assert!(
            trimmed <= 5_000,
            "v0.7 C4 trimmed full-profile total {trimmed} > 5000-token soft ceiling. \
             If a tool genuinely needs more required params, update the bound; \
             if not, audit C4_KEEP_OPTIONAL_PARAMS for unintended growth."
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
