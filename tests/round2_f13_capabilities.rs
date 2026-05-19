// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F13 — `memory_capabilities` schema/behavior drift.
//!
//! Pins the v0.7.0 Round-2 fixes for the capabilities surface:
//!
//! 1. The MCP `inputSchema` for `memory_capabilities` declares the
//!    four parameters the server has always accepted (`accept`,
//!    `family`, `include_schema`, `verbose`).
//! 2. The summary string and the `to_describe_to_user` string agree
//!    on the substantive memory-tool count (50/50 for `--profile
//!    full`, not "51 of 51 tools" alongside "50 memory tools").
//! 3. `effective_tier_label(has_llm, has_embedder, has_reranker)`
//!    derives the runtime-effective tier from live handles —
//!    matches the boot-banner string emitted by `serve_mcp` so the
//!    capabilities response and the daemon log converge on a single
//!    tier source-of-truth.
//! 4. The `tool_definitions()` entry for `memory_capabilities`
//!    declares the four parameters in `inputSchema.properties` (the
//!    Round-1 schema declared zero properties even though the
//!    server accepted all four).
//! 5. The `overlay_tool_payloads` helper injects per-tool
//!    `inputSchema` (when `include_schema=true`) and `docstring`
//!    (when `verbose=true`) into a v3 capabilities response.

use ai_memory::mcp::{
    build_capabilities_describe_to_user, build_capabilities_summary, effective_tier_label,
    overlay_tool_payloads, tool_definitions,
};
use ai_memory::profile::Profile;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// 1. memory_capabilities inputSchema declares accept/family/include_schema/verbose.
// ---------------------------------------------------------------------------
#[test]
fn f13_capabilities_input_schema_declares_all_four_params() {
    let defs = tool_definitions();
    let tools = defs["tools"].as_array().expect("tools array");
    let cap = tools
        .iter()
        .find(|t| t["name"] == "memory_capabilities")
        .expect("memory_capabilities tool must be registered");

    let props = cap["inputSchema"]["properties"]
        .as_object()
        .expect("memory_capabilities.inputSchema.properties must be a JSON object");

    for required_param in ["accept", "family", "include_schema", "verbose"] {
        assert!(
            props.contains_key(required_param),
            "memory_capabilities.inputSchema must declare `{required_param}` (Round-2 F13); \
             got props: {:?}",
            props.keys().collect::<Vec<_>>()
        );
    }

    // Type sanity: accept enum, include_schema/verbose bool, family string.
    assert_eq!(props["accept"]["type"], "string");
    assert_eq!(
        props["include_schema"]["type"], "boolean",
        "include_schema must be boolean"
    );
    assert_eq!(
        props["verbose"]["type"], "boolean",
        "verbose must be boolean"
    );
    assert_eq!(props["family"]["type"], "string");
}

// ---------------------------------------------------------------------------
// 2. summary and describe_to_user agree on the substantive count.
// ---------------------------------------------------------------------------
#[test]
fn f13_summary_and_describe_to_user_agree_on_count_full_profile() {
    let summary = build_capabilities_summary(&Profile::full());
    let describe = build_capabilities_describe_to_user(&Profile::full());

    // Both must report "71" for the full profile (substantive memory
    // tools, excluding the always-on `memory_capabilities` bootstrap).
    // v0.7.0 issues #224 + #311 added memory_share to Family::Power,
    // pulled forward from v0.8 Phase 3 Memory Sharing & Sync RFC per
    // operator directive `28860423-d12c-4959-bc8b-8fa9a94a33d9` —
    // bumping the substantive total to 71.
    assert!(
        summary.contains("72 of 72 memory tools"),
        "summary must report 72 of 72 memory tools; got: {summary}"
    );
    assert!(
        describe.contains("all 72 memory tools"),
        "describe_to_user must report all 72 memory tools; got: {describe}"
    );
}

#[test]
fn f13_summary_and_describe_to_user_agree_on_count_core_profile() {
    let summary = build_capabilities_summary(&Profile::core());
    let describe = build_capabilities_describe_to_user(&Profile::core());

    // Core profile loads `Family::Core` (7 tools). Bootstrap excluded.
    // Total memory tools = 71 (72 - bootstrap). v0.7.0 issues #224 +
    // #311 added memory_share to Family::Power, pulled forward from
    // v0.8 Phase 3 Memory Sharing & Sync RFC per operator directive
    // `28860423-d12c-4959-bc8b-8fa9a94a33d9`.
    assert!(
        summary.contains("7 of 72 memory tools"),
        "summary must report 7 of 72 memory tools; got: {summary}"
    );
    assert!(
        describe.contains("7 memory tools"),
        "describe_to_user must report 7 memory tools; got: {describe}"
    );
}

// ---------------------------------------------------------------------------
// 3. effective_tier_label maps runtime handles to the canonical label.
// ---------------------------------------------------------------------------
#[test]
fn f13_effective_tier_label_matches_boot_banner() {
    // Mirror the `serve_mcp` boot banner branches.
    assert_eq!(effective_tier_label(true, true, true), "autonomous");
    assert_eq!(effective_tier_label(true, true, false), "smart");
    assert_eq!(effective_tier_label(false, true, false), "semantic");
    assert_eq!(effective_tier_label(false, false, false), "keyword");
    // Reranker without embedder is not a configurable runtime state,
    // but the function should still degrade cleanly.
    assert_eq!(effective_tier_label(false, false, true), "keyword");
}

// ---------------------------------------------------------------------------
// 4. include_schema=true populates inputSchema on tool entries.
// ---------------------------------------------------------------------------
#[test]
fn f13_overlay_tool_payloads_with_include_schema_injects_input_schemas() {
    let mut response = json!({
        "tools": [
            {"name": "memory_store", "family": "core", "loaded": true, "callable_now": true},
            {"name": "memory_recall", "family": "core", "loaded": true, "callable_now": true},
        ]
    });
    let obj = response.as_object_mut().expect("response is object");
    overlay_tool_payloads(
        obj,
        &Profile::full(),
        /* include_schema = */ true,
        /* verbose = */ false,
    );

    let tools = response["tools"].as_array().expect("tools array");
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let schema = tool.get("inputSchema");
        assert!(
            schema.is_some(),
            "tool '{name}' must carry inputSchema after include_schema overlay; got: {tool}"
        );
        // Sanity: docstring NOT injected when verbose=false.
        assert!(
            tool.get("docstring").is_none(),
            "tool '{name}' must NOT have docstring when verbose=false; got: {tool}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. verbose=true populates docstring on tool entries.
// ---------------------------------------------------------------------------
#[test]
fn f13_overlay_tool_payloads_with_verbose_injects_docstrings() {
    let mut response = json!({
        "tools": [
            {"name": "memory_store", "family": "core", "loaded": true, "callable_now": true},
            {"name": "memory_recall", "family": "core", "loaded": true, "callable_now": true},
        ]
    });
    let obj = response.as_object_mut().expect("response is object");
    overlay_tool_payloads(
        obj,
        &Profile::full(),
        /* include_schema = */ false,
        /* verbose = */ true,
    );

    let tools = response["tools"].as_array().expect("tools array");
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let docstring = tool["docstring"].as_str();
        assert!(
            docstring.is_some_and(|d| !d.is_empty()),
            "tool '{name}' must carry non-empty docstring after verbose overlay; got: {tool}"
        );
        // Sanity: inputSchema NOT injected when include_schema=false.
        assert!(
            tool.get("inputSchema").is_none(),
            "tool '{name}' must NOT have inputSchema when include_schema=false; got: {tool}"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Both verbose and include_schema together inject both fields.
// ---------------------------------------------------------------------------
#[test]
fn f13_overlay_tool_payloads_with_both_flags_injects_both_fields() {
    let mut response = json!({
        "tools": [
            {"name": "memory_store", "family": "core", "loaded": true, "callable_now": true},
        ]
    });
    let obj = response.as_object_mut().expect("response is object");
    overlay_tool_payloads(
        obj,
        &Profile::full(),
        /* include_schema = */ true,
        /* verbose = */ true,
    );

    let tools = response["tools"].as_array().expect("tools array");
    let store = &tools[0];
    assert!(
        store.get("inputSchema").is_some(),
        "inputSchema must be present"
    );
    assert!(
        store.get("docstring").is_some(),
        "docstring must be present"
    );
}

// ---------------------------------------------------------------------------
// 7. Default (both flags false) is a no-op.
// ---------------------------------------------------------------------------
#[test]
fn f13_overlay_tool_payloads_default_is_noop() {
    let original: Value = json!({
        "tools": [
            {"name": "memory_store", "family": "core", "loaded": true, "callable_now": true},
        ]
    });
    let mut response = original.clone();
    let obj = response.as_object_mut().expect("response is object");
    overlay_tool_payloads(
        obj,
        &Profile::full(),
        /* include_schema = */ false,
        /* verbose = */ false,
    );

    assert_eq!(
        response, original,
        "no flags set must be a structural no-op; got: {response}"
    );
}

// ---------------------------------------------------------------------------
// 8. Family drilldown still gets full inputSchema with include_schema=true
// (canonical behavior, anchored as a regression guard).
// ---------------------------------------------------------------------------
#[test]
fn f13_family_drilldown_with_include_schema_carries_schemas() {
    use ai_memory::mcp::handle_capabilities_family;

    let resp = handle_capabilities_family(
        "core",
        /* include_schema = */ true,
        /* verbose       = */ true,
        &Profile::full(),
        None,
        None,
        None,
    )
    .expect("family drilldown must succeed");

    // Schema_version must be present (negotiated wire-version).
    assert!(
        resp.get("schema_version").is_some(),
        "schema_version must be present even on the family path; got: {resp}"
    );

    let tools = resp["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty(), "core family must have tools");
    for tool in tools {
        assert!(
            tool.get("inputSchema").is_some(),
            "tool must carry inputSchema with include_schema=true; got: {tool}"
        );
    }
}
