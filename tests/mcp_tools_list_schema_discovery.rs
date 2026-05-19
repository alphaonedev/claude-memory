// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// Issue #859 — regression suite for MCP `tools/list` schema-discovery.
//
// Pre-#859, the C4 trim dropped every optional property entry from
// the wire-form `tools/list` payload (keeping only the `required`
// keys + the allow-list `[namespace, format]`). That broke NHI
// runtime discovery: clients reading `tools/list` saw
// `memory_kg_query.inputSchema.properties = {source_id}` and had no
// way to learn that `max_depth`, `valid_at`, `allowed_agents`,
// `limit`, `include_invalidated` were valid params.
//
// Post-#859, the trim preserves every property entry on the wire
// (so clients can DISCOVER what knobs exist) and strips only the
// per-property `description` prose + the top-level `docs` field.
// The token-budget ceiling is still honored (verified by
// `tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_post_859_ceiling`
// and the budget gate added below).
//
// These tests pin the wire-form contract so a future regression can
// not silently re-drop the optionals.

use ai_memory::mcp::tool_definitions_for_profile;
use ai_memory::profile::Profile;
use ai_memory::sizes::{full_profile_total_tokens, trimmed_full_profile_total_tokens};
use serde_json::Value;

/// Issue #859 ceilings.
///
/// **Trimmed (`tools/list` wire-form):** 5000 cl100k tokens. Pre-#859
/// the wire dropped every optional property entry to fit a 3500 floor;
/// #859 restored those entries for client-side discovery, raising the
/// floor by ~1100 tokens (the structural metadata that was hidden
/// pre-fix). The wire still strips per-property `description` prose
/// and compacts the per-tool short description, so the 5000 ceiling
/// is the irreducible cost of a fully-discoverable schema surface.
///
/// **Verbose (`tool_definitions()` raw catalog):** 10000 cl100k
/// tokens. This is the cost an MCP client pays when calling
/// `memory_capabilities { family=<f>, include_schema=true,
/// verbose=true }` across every family in sequence. The full prose +
/// docs + per-property descriptions remain in the source catalog;
/// the ceiling tracks the post-#859 measurement with ~500 tokens of
/// headroom for future tool additions.
const TRIMMED_TOKEN_CEILING: usize = 5_000;
const VERBOSE_TOKEN_CEILING: usize = 10_000;

/// Look up a single tool's `inputSchema.properties` map under the
/// full profile's trimmed wire form.
fn wire_properties(tool_name: &str) -> serde_json::Map<String, Value> {
    let defs = tool_definitions_for_profile(&Profile::full());
    let tools = defs["tools"].as_array().expect("tools array");
    let tool = tools
        .iter()
        .find(|t| t["name"].as_str() == Some(tool_name))
        .unwrap_or_else(|| panic!("tool `{tool_name}` not present in full profile"));
    tool["inputSchema"]["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn issue_859_memory_kg_query_exposes_all_optionals_on_wire() {
    // The field-level proof from the issue body: pre-fix this returned
    // only `{source_id}`. Post-fix every optional is reachable.
    let props = wire_properties("memory_kg_query");
    for expected in [
        "source_id",
        "max_depth",
        "valid_at",
        "allowed_agents",
        "limit",
        "include_invalidated",
    ] {
        assert!(
            props.contains_key(expected),
            "#859: memory_kg_query.inputSchema.properties must expose `{expected}` on the wire \
             (got {:?})",
            props.keys().collect::<Vec<_>>()
        );
    }
    // max_depth — verify the structural metadata stays (NHI agents
    // need to know type/min/max/default to construct a valid call).
    let max_depth = props
        .get("max_depth")
        .and_then(Value::as_object)
        .expect("max_depth property must be an object");
    assert_eq!(
        max_depth.get("type").and_then(Value::as_str),
        Some("integer")
    );
    assert_eq!(max_depth.get("minimum").and_then(Value::as_u64), Some(1));
    assert_eq!(max_depth.get("maximum").and_then(Value::as_u64), Some(5));
    assert_eq!(max_depth.get("default").and_then(Value::as_u64), Some(1));
    // But per-property prose is dropped on the wire.
    assert!(
        !max_depth.contains_key("description"),
        "#859: per-property `description` prose must be stripped on the wire"
    );
}

#[test]
fn issue_859_memory_link_exposes_relation_enum_on_wire() {
    // Pre-fix this dropped the `relation` enum. Post-fix the full
    // five-variant enum is wire-visible so clients know which
    // relations are accepted.
    let props = wire_properties("memory_link");
    let relation = props
        .get("relation")
        .and_then(Value::as_object)
        .expect("memory_link must expose `relation` on the wire (#859)");
    let variants = relation
        .get("enum")
        .and_then(Value::as_array)
        .expect("relation must carry its `enum` array on the wire");
    let names: Vec<&str> = variants.iter().filter_map(Value::as_str).collect();
    for expected in [
        "related_to",
        "supersedes",
        "contradicts",
        "derived_from",
        "reflects_on",
    ] {
        assert!(
            names.contains(&expected),
            "#859: memory_link.relation.enum must include `{expected}` on the wire (got {names:?})"
        );
    }
    // Default should also stay so clients can omit `relation` and
    // know which variant they're getting.
    assert_eq!(
        relation.get("default").and_then(Value::as_str),
        Some("related_to")
    );
}

#[test]
fn issue_859_memory_update_exposes_all_optionals_on_wire() {
    // Pre-fix exposed only `{id, namespace}`. Post-fix all the
    // optionals are wire-visible.
    let props = wire_properties("memory_update");
    for expected in [
        "id",
        "title",
        "content",
        "tier",
        "namespace",
        "tags",
        "priority",
        "confidence",
        "expires_at",
        "metadata",
    ] {
        assert!(
            props.contains_key(expected),
            "#859: memory_update.inputSchema.properties must expose `{expected}` on the wire \
             (got {:?})",
            props.keys().collect::<Vec<_>>()
        );
    }
    // Tier enum must carry its variants.
    let tier = props
        .get("tier")
        .and_then(Value::as_object)
        .expect("tier property must be present on memory_update wire schema");
    let variants: Vec<&str> = tier
        .get("enum")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(variants.contains(&"short"));
    assert!(variants.contains(&"mid"));
    assert!(variants.contains(&"long"));
}

#[test]
fn issue_859_every_tool_keeps_required_array_on_wire() {
    // The `required` array is load-bearing for client-side validation
    // ("which of these params is mandatory?"). Pre- and post-fix this
    // array must be preserved verbatim on every tool that declares one.
    let defs = tool_definitions_for_profile(&Profile::full());
    let tools = defs["tools"].as_array().unwrap();
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        let Some(schema) = tool.get("inputSchema") else {
            continue;
        };
        if let Some(required) = schema.get("required") {
            assert!(
                required.is_array(),
                "tool `{name}` declares `required` but it is not an array on the wire"
            );
        }
    }
}

#[test]
fn issue_859_wire_form_drops_per_property_description_prose() {
    // The whole #859 trade is: keep property entries, strip the prose.
    // Walk every property of every tool and assert no `description`
    // string survives on the wire.
    let defs = tool_definitions_for_profile(&Profile::full());
    let tools = defs["tools"].as_array().unwrap();
    let mut leaks: Vec<String> = Vec::new();
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>").to_string();
        let Some(props) = tool["inputSchema"]
            .get("properties")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (prop_name, prop_value) in props {
            if let Some(prop_obj) = prop_value.as_object()
                && prop_obj.contains_key("description")
            {
                leaks.push(format!("{name}.{prop_name}"));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "#859: per-property `description` prose must be stripped on the wire; leaked: {leaks:?}"
    );
}

#[test]
fn issue_859_wire_form_drops_top_level_docs() {
    // The verbose `docs` field must not appear on the wire (matches
    // the existing C2 contract pinned in tests/c2_tool_docs_field.rs;
    // re-pinned here so a #859 regression that re-introduces docs at
    // the same time as re-dropping properties is still caught).
    let defs = tool_definitions_for_profile(&Profile::full());
    let tools = defs["tools"].as_array().unwrap();
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert!(
            tool.get("docs").is_none(),
            "tool `{name}` leaked `docs` on the wire (#859 + C2)"
        );
    }
}

#[test]
fn issue_859_trimmed_full_profile_under_post_fix_ceiling() {
    // CI assertion #1 — wire-form `tools/list` must stay under the
    // post-#859 token ceiling. Same gate is also pinned by
    // tests/c2_tool_docs_field.rs::c2_tools_list_token_budget_is_under_post_859_ceiling
    // and tests/budget_tokens.rs::full_profile_tools_list_under_3500_tokens
    // (now under_post_859_ceiling); the assertion lives in three
    // places so a regression in any one CI lane catches it.
    let total = trimmed_full_profile_total_tokens();
    assert!(
        total <= TRIMMED_TOKEN_CEILING,
        "#859: trimmed full-profile tools/list payload is {total} cl100k tokens \
         (ceiling: {TRIMMED_TOKEN_CEILING}). The #859 fix preserves every property \
         entry on the wire — if you grew a schema, audit per-property \
         `description` prose (must be stripped) and consider trimming a tool's \
         short-form `description`."
    );
}

#[test]
fn issue_859_verbose_full_profile_under_post_fix_ceiling() {
    // CI assertion #2 — the verbose `tool_definitions()` payload (the
    // source of truth read by `memory_capabilities { verbose=true }`)
    // must stay under 10000 cl100k tokens. Past v0.7 baseline ~7.4K;
    // post-#859 with extra discovery metadata: ~9500. The 10K ceiling
    // leaves ~500 tokens of headroom for future tools/prose.
    let total = full_profile_total_tokens();
    assert!(
        total <= VERBOSE_TOKEN_CEILING,
        "#859: verbose full-profile tool catalog is {total} cl100k tokens \
         (ceiling: {VERBOSE_TOKEN_CEILING}). Audit the largest tools' `docs` \
         fields via `cargo run -- doctor --tokens --raw-table`."
    );
}
