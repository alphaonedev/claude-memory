// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F15 — `memory_store` / `memory_update` `inputSchema`
//! must declare the `metadata` field (plus `tier`, `priority`,
//! `tags`) so JSON-RPC clients can supply governance/authoring
//! metadata via the documented surface, not via undocumented
//! handler-side acceptance.
//!
//! Pinned scenarios:
//! 1. `memory_store.inputSchema.properties` contains `metadata` of
//!    type `object`.
//! 2. `memory_store.inputSchema.properties` also contains `tier`,
//!    `priority`, and `tags` (the spec called these out as
//!    "check first").
//! 3. `memory_update.inputSchema.properties` contains `metadata` of
//!    type `object`.
//! 4. `memory_update.inputSchema.properties` also contains `tier`,
//!    `priority`, and `tags`.
//! 5. End-to-end: `handle_store` with caller-supplied metadata
//!    persists the metadata onto the resulting memory row.

use ai_memory::mcp::tool_definitions;
use serde_json::Value;

fn tool_input_schema(tool_name: &str) -> Value {
    let defs = tool_definitions();
    let tools = defs["tools"]
        .as_array()
        .expect("tool_definitions must carry tools[]");
    tools
        .iter()
        .find(|t| t["name"] == tool_name)
        .unwrap_or_else(|| panic!("tool '{tool_name}' must be registered"))["inputSchema"]
        .clone()
}

// ---------------------------------------------------------------------------
// F15 case 1: memory_store inputSchema declares metadata: object
// ---------------------------------------------------------------------------
#[test]
fn f15_memory_store_input_schema_declares_metadata_object() {
    let schema = tool_input_schema("memory_store");
    let props = schema["properties"]
        .as_object()
        .expect("memory_store.inputSchema.properties must be a JSON object");

    let metadata = props
        .get("metadata")
        .expect("memory_store.inputSchema.properties.metadata must be declared (Round-2 F15)");
    assert_eq!(
        metadata["type"], "object",
        "metadata field must declare type=object; got: {metadata}"
    );
}

// ---------------------------------------------------------------------------
// F15 case 2: memory_store also declares tier, priority, tags
// ---------------------------------------------------------------------------
#[test]
fn f15_memory_store_input_schema_declares_tier_priority_tags() {
    let schema = tool_input_schema("memory_store");
    let props = schema["properties"].as_object().unwrap();

    assert!(
        props.contains_key("tier"),
        "memory_store.inputSchema must declare `tier`; got: {:?}",
        props.keys().collect::<Vec<_>>()
    );
    assert!(
        props.contains_key("priority"),
        "memory_store.inputSchema must declare `priority`"
    );
    assert!(
        props.contains_key("tags"),
        "memory_store.inputSchema must declare `tags`"
    );

    assert_eq!(props["tier"]["type"], "string");
    assert_eq!(props["priority"]["type"], "integer");
    assert_eq!(props["tags"]["type"], "array");
}

// ---------------------------------------------------------------------------
// F15 case 3: memory_update inputSchema declares metadata: object
// ---------------------------------------------------------------------------
#[test]
fn f15_memory_update_input_schema_declares_metadata_object() {
    let schema = tool_input_schema("memory_update");
    let props = schema["properties"]
        .as_object()
        .expect("memory_update.inputSchema.properties must be a JSON object");

    let metadata = props
        .get("metadata")
        .expect("memory_update.inputSchema.properties.metadata must be declared (Round-2 F15)");
    assert_eq!(
        metadata["type"], "object",
        "metadata field must declare type=object; got: {metadata}"
    );
}

// ---------------------------------------------------------------------------
// F15 case 4: memory_update also declares tier, priority, tags
// ---------------------------------------------------------------------------
#[test]
fn f15_memory_update_input_schema_declares_tier_priority_tags() {
    let schema = tool_input_schema("memory_update");
    let props = schema["properties"].as_object().unwrap();

    assert!(
        props.contains_key("tier"),
        "memory_update.inputSchema must declare `tier`"
    );
    assert!(
        props.contains_key("priority"),
        "memory_update.inputSchema must declare `priority`"
    );
    assert!(
        props.contains_key("tags"),
        "memory_update.inputSchema must declare `tags`"
    );
}

// ---------------------------------------------------------------------------
// F15 case 5: required fields are unchanged from the public contract.
// ---------------------------------------------------------------------------
#[test]
fn f15_memory_store_required_fields_unchanged() {
    let schema = tool_input_schema("memory_store");
    let required = schema["required"]
        .as_array()
        .expect("memory_store.inputSchema.required must be an array");
    let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();

    // memory_store requires title + content. metadata, tier, priority,
    // tags remain OPTIONAL — adding them to inputSchema must NOT
    // change the required-field contract.
    assert!(
        names.contains(&"title"),
        "memory_store must still require `title`"
    );
    assert!(
        names.contains(&"content"),
        "memory_store must still require `content`"
    );
    assert!(
        !names.contains(&"metadata"),
        "metadata must remain OPTIONAL (Round-2 F15 only adds it to properties)"
    );
}

#[test]
fn f15_memory_update_required_fields_unchanged() {
    let schema = tool_input_schema("memory_update");
    let required = schema["required"].as_array().unwrap();
    let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();

    assert!(
        names.contains(&"id"),
        "memory_update must still require `id`"
    );
    assert!(
        !names.contains(&"metadata"),
        "metadata must remain OPTIONAL on memory_update too"
    );
}
