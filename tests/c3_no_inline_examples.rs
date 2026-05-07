// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 C3 — strip redundant inline examples from tool descriptions.
//
// The bare `tools/list` payload (and the prompt-args payload it
// neighbours) must not embed `e.g. ...` parentheticals or
// `<example>...</example>` blocks any more. C2 already moved the
// long-form prose into a separate `docs` field that surfaces only when
// the caller asks for it via `verbose=true`; the inline examples were
// the last duplication left on the always-on wire shape.
//
// These assertions run on the post-strip wire payload — i.e. the
// `tools/list` shape every MCP client pays per session — and on the
// `prompts/list` payload (the prompt argument descriptions that share
// the same surface). They keep the C3 win from regressing as new tools
// land.

use ai_memory::mcp::{prompt_definitions, tool_definitions_for_profile};
use ai_memory::profile::Profile;
use serde_json::Value;

/// Substrings that count as a banned inline example marker. The check
/// is case-insensitive and walks every string node in the payload.
const FORBIDDEN_MARKERS: &[&str] = &["e.g.", "<example>", "</example>"];

/// Walk every string-valued node under `value` and call `visit` with
/// the JSON pointer-style path so the failure message points at the
/// offending field.
fn walk_strings<F: FnMut(&str, &str)>(value: &Value, path: &str, visit: &mut F) {
    match value {
        Value::String(s) => visit(path, s),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let child = format!("{path}/{i}");
                walk_strings(item, &child, visit);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                let child = format!("{path}/{k}");
                walk_strings(v, &child, visit);
            }
        }
        _ => {}
    }
}

fn find_violations(payload: &Value, label: &str) -> Vec<String> {
    let mut violations: Vec<String> = Vec::new();
    walk_strings(payload, label, &mut |path, s| {
        let lowered = s.to_ascii_lowercase();
        for marker in FORBIDDEN_MARKERS {
            if lowered.contains(marker) {
                violations.push(format!("{path}: contains `{marker}` -> {s:?}"));
            }
        }
    });
    violations
}

#[test]
fn c3_bare_tools_list_has_no_inline_examples() {
    // Acceptance — every string node on the bare `tools/list` payload
    // (the always-on wire shape, full profile) must be free of the
    // inline-example markers C2 left behind.
    let defs = tool_definitions_for_profile(&Profile::full());
    let violations = find_violations(&defs, "#");
    assert!(
        violations.is_empty(),
        "C3 — bare tools/list payload still carries inline-example markers \
         that should live in the verbose `docs` field instead:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn c3_prompt_definitions_have_no_inline_examples() {
    // Acceptance — the prompt-args descriptions ride the same wire as
    // tool descriptions and must observe the same rule.
    let defs = prompt_definitions();
    let violations = find_violations(&defs, "#");
    assert!(
        violations.is_empty(),
        "C3 — prompt_definitions() payload still carries inline-example markers:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn c3_tools_list_token_budget_is_under_3500() {
    // C5 hard ceiling — restate it on the C3 surface so a regression
    // here is caught locally, not just by the cross-cutting C5 gate.
    let defs = tool_definitions_for_profile(&Profile::full());
    let serialized = serde_json::to_string(&defs).expect("tool defs must serialize");
    let tokens = ai_memory::db::count_tokens_cl100k(&serialized);

    assert!(
        tokens <= 3500,
        "tools/list bare payload exceeded the C5 budget — got {tokens} cl100k tokens, \
         ceiling is 3500. C3 stripped inline examples; if this fires the surface grew \
         elsewhere."
    );
}
