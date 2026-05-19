// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #912 — MCP schema/handler parity pins for the last two
//! `#892`-class schema-drift cases surfaced by the v0.7.0 final
//! code+security review.
//!
//! Pre-#912:
//!
//! - `memory_subscribe` handler (`src/mcp/tools/subscribe.rs:41`)
//!   read `params["event_types"]` (P5 / G9 structured per-event-type
//!   opt-in) but `inputSchema.properties` omitted it; NHIs could not
//!   discover the property via `tools/list`.
//! - `memory_replay` handler (`src/mcp/tools/replay.rs:112`) read
//!   `params["agent_id"]` (used by the K9 permission gate to scope
//!   per-transcript authorisation) but `inputSchema.properties`
//!   omitted it; same #892-class gap.
//!
//! Fix mirrors the established #904/#908 pattern — add the missing
//! schema properties since the handler reads them and they are
//! load-bearing for NHI discovery. (The alternative — remove the
//! handler read — would break the documented P5 + K9 behaviour and
//! is the wrong direction per the #906 operator decision.)

use ai_memory::mcp::tool_definitions;

fn schema_props_for<'a>(
    defs: &'a serde_json::Value,
    tool_name: &str,
) -> &'a serde_json::Map<String, serde_json::Value> {
    let tools = defs["tools"].as_array().expect("tools array");
    let entry = tools
        .iter()
        .find(|t| t["name"] == tool_name)
        .unwrap_or_else(|| panic!("{tool_name} must be registered in tool_definitions"));
    entry["inputSchema"]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{tool_name}.inputSchema.properties must be an object"))
}

#[test]
fn memory_subscribe_input_schema_declares_event_types_912() {
    let defs = tool_definitions();
    let props = schema_props_for(&defs, "memory_subscribe");
    assert!(
        props.contains_key("event_types"),
        "#912: memory_subscribe.inputSchema must declare `event_types` (handler reads \
         `params[\"event_types\"].as_array()` at src/mcp/tools/subscribe.rs:41). \
         Got props: {:?}",
        props.keys().collect::<Vec<_>>()
    );
    // Shape sanity: array of strings (P5/G9 contract).
    assert_eq!(
        props["event_types"]["type"], "array",
        "event_types must be an array property"
    );
    assert_eq!(
        props["event_types"]["items"]["type"], "string",
        "event_types items must be strings"
    );
}

#[test]
fn memory_replay_input_schema_declares_agent_id_912() {
    let defs = tool_definitions();
    let props = schema_props_for(&defs, "memory_replay");
    assert!(
        props.contains_key("agent_id"),
        "#912: memory_replay.inputSchema must declare `agent_id` (handler reads \
         `params[\"agent_id\"].as_str()` at src/mcp/tools/replay.rs:112 to drive the \
         K9 permission gate). Got props: {:?}",
        props.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        props["agent_id"]["type"], "string",
        "agent_id must be a string property"
    );
}
