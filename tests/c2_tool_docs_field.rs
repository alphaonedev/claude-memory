// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 C2 — tool description / docs split for token budget.
//
// These tests pin the structural contract:
//   1. The bare `tools/list` response (any profile) carries no `docs`
//      field — every tool entry exposes only the short-form
//      `description`.
//   2. A verbose `memory_capabilities` drilldown
//      (`family=<name>`, `include_schema=true`, `verbose=true`) restores
//      `docs` on at least 5 tool entries — the verbose payload is
//      strictly additive to the short form.
//   3. Every short-form `description` is ≤ 50 cl100k_base tokens, the
//      C2 budget invariant.
//
// The matching token-budget gate for the *full* `tools/list` payload
// lives in `tests/budget_tokens.rs` so the always-on shape stays
// inside the C5 ceiling (3500 cl100k tokens) by default.

use ai_memory::db::count_tokens_cl100k;
use ai_memory::mcp::{handle_capabilities_family, tool_definitions, tool_definitions_for_profile};
use ai_memory::profile::Profile;
use serde_json::Value;

/// C2 spec — short-form `description` budget per tool entry.
const SHORT_DESCRIPTION_BUDGET_TOKENS: usize = 50;

#[test]
fn c2_bare_tools_list_has_no_docs_field() {
    // Acceptance #1 — the bare `tools/list` payload (full profile)
    // must omit the verbose `docs` field on every tool entry. Default
    // `description` is the only natural-language payload on the wire.
    let defs = tool_definitions_for_profile(&Profile::full());
    let tools = defs["tools"].as_array().expect("tools must be an array");
    assert!(!tools.is_empty(), "fixture sanity: tools must be populated");

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert!(
            tool.get("docs").is_none(),
            "tool '{name}' leaked a `docs` field on the bare tools/list \
             payload (verbose content must move under verbose=true)"
        );
        assert!(
            tool.get("description")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.is_empty()),
            "tool '{name}' must always carry a non-empty short `description`"
        );
    }
}

#[test]
fn c2_verbose_capabilities_response_populates_docs_on_5plus_tools() {
    // Acceptance #2 — a `memory_capabilities { family=<name>,
    // include_schema=true, verbose=true }` drilldown must surface the
    // long-form `docs` field on at least 5 tools across the families.
    // We sample two distinct families (core + graph) so a regression
    // that strips docs from one branch but not the other still trips
    // the gate.
    let profile = Profile::full();

    let mut docs_seen = 0usize;
    for family in ["core", "graph", "lifecycle"] {
        let resp = handle_capabilities_family(
            family, /* include_schema = */ true, /* verbose       = */ true, &profile,
            None, None, None,
        )
        .unwrap_or_else(|e| panic!("verbose drilldown for '{family}' failed: {e}"));

        let tools = resp["tools"]
            .as_array()
            .expect("verbose response must carry tools[]");
        for tool in tools {
            if tool
                .get("docs")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.is_empty())
            {
                docs_seen += 1;
            }
        }
    }

    assert!(
        docs_seen >= 5,
        "verbose=true must surface `docs` on >=5 tools across core+graph+lifecycle; got {docs_seen}"
    );

    // Negative control — repeat with verbose=false and assert that the
    // same drilldown drops every `docs` field (no leakage on the
    // default path).
    let resp = handle_capabilities_family(
        "core", /* include_schema = */ true, /* verbose       = */ false, &profile, None,
        None, None,
    )
    .expect("non-verbose drilldown must succeed");
    let tools = resp["tools"].as_array().unwrap();
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert!(
            tool.get("docs").is_none(),
            "tool '{name}' leaked `docs` on the non-verbose drilldown"
        );
    }
}

#[test]
fn c2_every_short_description_is_under_50_tokens() {
    // Acceptance #3 — every `description` on the bare tools/list
    // response must be ≤ 50 cl100k_base tokens. This is the C2 budget
    // invariant — without it the C5 token-budget gate cannot hold.
    let defs = tool_definitions_for_profile(&Profile::full());
    let tools = defs["tools"].as_array().unwrap();

    let mut violations: Vec<(String, usize)> = Vec::new();
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>").to_string();
        let desc = tool["description"]
            .as_str()
            .unwrap_or_else(|| panic!("tool '{name}' missing description"));
        let tokens = count_tokens_cl100k(desc);
        if tokens > SHORT_DESCRIPTION_BUDGET_TOKENS {
            violations.push((name, tokens));
        }
    }

    assert!(
        violations.is_empty(),
        "C2 budget violation — these tools' short `description` exceed \
         {SHORT_DESCRIPTION_BUDGET_TOKENS} cl100k tokens: {violations:?}"
    );
}

#[test]
fn c2_tools_list_token_budget_is_under_post_859_ceiling() {
    // C5 budget gate (the structural side of C2) — the full
    // `tools/list` response (no verbose, full profile) must serialize
    // to ≤ 5000 cl100k_base tokens. This is the always-on payload
    // every MCP client pays per session, so the cap matters.
    //
    // **Post-#859 update (was: 3500).** Pre-#859 the C4 trim dropped
    // every optional property entry from the wire (keeping only
    // `required` + an allow-list of `[namespace, format]`), which let
    // the wire payload sit at ~3456 tokens for 71 tools. That trim
    // broke NHI runtime discovery: clients reading `tools/list` had
    // no way to learn that `max_depth`, `relation`, `valid_at`, etc.
    // were valid params. #859 restored every property entry on the
    // wire (with per-property `description` prose stripped + the
    // top-level tool description compacted to the first sentence) so
    // clients can discover the call surface. The irreducible cost of
    // that fully-discoverable schema is ~4500-4700 tokens; the new
    // 5000 ceiling pins that with ~300 tokens of headroom for future
    // tool additions.
    let defs = tool_definitions_for_profile(&Profile::full());
    let serialized = serde_json::to_string(&defs).expect("tool defs must serialize");
    let tokens = count_tokens_cl100k(&serialized);

    assert!(
        tokens <= 5000,
        "tools/list bare payload exceeded the post-#859 budget — got {tokens} cl100k tokens, \
         ceiling is 5000. Audit per-property prose (must be stripped on the wire), top-level \
         tool descriptions (compacted to first sentence), and consider whether a new tool's \
         schema can move under `family=power` instead of the always-on core."
    );
}

#[test]
#[ignore = "diagnostic — run with --ignored to print the per-tool token breakdown"]
fn c2_diagnostic_print_token_distribution() {
    // Diagnostic scaffold — run with `cargo test
    // c2_diagnostic_print_token_distribution -- --ignored --nocapture`
    // to see which tools consume the most tokens on the bare payload.
    let defs = tool_definitions_for_profile(&Profile::full());
    let tools = defs["tools"].as_array().unwrap();
    let mut rows: Vec<(usize, &str)> = tools
        .iter()
        .map(|t| {
            let s = serde_json::to_string(t).unwrap();
            let tokens = count_tokens_cl100k(&s);
            (tokens, t["name"].as_str().unwrap())
        })
        .collect();
    rows.sort_unstable_by_key(|row| std::cmp::Reverse(row.0));
    let total: usize = rows.iter().map(|(t, _)| *t).sum();
    eprintln!("=== C2 per-tool token distribution (bare tools/list) ===");
    eprintln!("Total: {total} tokens across {} tools", rows.len());
    for (toks, name) in &rows {
        eprintln!("  {toks:>5}  {name}");
    }
}

#[test]
fn c2_tool_definitions_source_keeps_docs_on_overbudget_tools() {
    // Sanity — the canonical `tool_definitions()` (the source of truth
    // before any stripping) must still carry `docs` on at least 10
    // tools. Otherwise we've accidentally deleted the long-form prose
    // entirely instead of moving it.
    let defs = tool_definitions();
    let tools = defs["tools"].as_array().unwrap();

    let with_docs = tools
        .iter()
        .filter(|t| {
            t.get("docs")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.is_empty())
        })
        .count();
    assert!(
        with_docs >= 10,
        "tool_definitions() should retain `docs` on >=10 tools (this is the \
         source of truth that verbose=true reads from); got {with_docs}"
    );
}
