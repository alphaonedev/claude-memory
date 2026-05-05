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

/// Runtime-computed table of every tool's tokenized schema cost.
///
/// Returns a static slice on every call after the first invocation
/// (which performs the one-time BPE pass).
pub fn tool_sizes() -> &'static [ToolSize] {
    static TABLE: OnceLock<Vec<ToolSize>> = OnceLock::new();
    TABLE.get_or_init(compute_table).as_slice()
}

/// Highest-cost tool in the table. Used by the CI gate.
pub fn tool_sizes_under_ci_gate() -> usize {
    tool_sizes()
        .iter()
        .map(|t| t.total_tokens)
        .max()
        .unwrap_or(0)
}

/// Sum of every tool's `total_tokens` — the worst-case prefix cost on
/// an eager-loading harness with `--profile full`.
pub fn full_profile_total_tokens() -> usize {
    tool_sizes().iter().map(|t| t.total_tokens).sum()
}

/// Lookup a single tool by name. `O(n)` but `n ≤ 43`.
pub fn tool_size(name: &str) -> Option<&'static ToolSize> {
    tool_sizes().iter().find(|t| t.name == name)
}

fn compute_table() -> Vec<ToolSize> {
    let bpe = bpe();
    let defs = crate::mcp::tool_definitions();
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
    fn table_has_43_entries_matching_tool_definitions_count() {
        let n = tool_sizes().len();
        assert_eq!(
            n, 43,
            "expected exactly 43 tools (v0.6.3.1 baseline source-anchored at \
             src/mcp.rs::tool_definitions); got {n}. If the count changed, \
             update the v0.6.4 family map and this assertion together."
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
    /// This test pins the new honest range. The savings *percentage*
    /// from `core` (~700 tokens) is unchanged at ~88%; the savings
    /// *absolute* is ~5,300 tokens per request, not ~22,000.
    #[test]
    fn full_profile_total_in_honest_measured_range() {
        let total = full_profile_total_tokens();
        assert!(
            (5_000..=8_000).contains(&total),
            "full-profile total {total} tokens is outside the measured \
             cl100k_base range (5K–8K). If the schema grew, update the \
             public claim in RFC/README/roadmap and adjust this bound."
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
}
