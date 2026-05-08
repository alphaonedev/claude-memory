// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F16 — `memory_agent_register.inputSchema.agent_type`
//! must match the daemon's actual permissive validation: a closed
//! enum was the wire-shape lie because the handler accepts any
//! `ai:<name>` form (alnum/_-.) up to 64 chars on top of the
//! curated short-list.
//!
//! Choice: open the schema layer (preferred — daemon is forward-
//! compat for new `ai:<model>` labels). The handler still rejects
//! values outside the curated list and the `ai:` namespace, so
//! `agent_type=service` still fails — but that's a handler-side
//! 400, not a wire-shape rejection.
//!
//! Pinned scenarios:
//! 1. The schema declares `agent_type: {type: "string"}` with NO
//!    `enum` constraint.
//! 2. The schema's description names the curated short-list AND the
//!    open `ai:<name>` namespace so a well-behaved client picks
//!    sensible defaults.
//! 3. `validate::validate_agent_type` accepts `ai:claude-opus-4.8`
//!    (a model that didn't exist when the closed enum was authored)
//!    — proves the daemon is permissive in the way the open schema
//!    advertises.

use ai_memory::mcp::tool_definitions;
use ai_memory::validate::validate_agent_type;
use serde_json::Value;

fn agent_register_schema() -> Value {
    let defs = tool_definitions();
    let tools = defs["tools"].as_array().expect("tools array");
    tools
        .iter()
        .find(|t| t["name"] == "memory_agent_register")
        .expect("memory_agent_register must be registered")
        .clone()
}

// ---------------------------------------------------------------------------
// F16 case 1: agent_type field is NOT a closed enum.
// ---------------------------------------------------------------------------
#[test]
fn f16_agent_type_field_is_open_string_not_enum() {
    let tool = agent_register_schema();
    let agent_type = &tool["inputSchema"]["properties"]["agent_type"];

    assert_eq!(
        agent_type["type"], "string",
        "agent_type must be type=string; got: {agent_type}"
    );
    assert!(
        agent_type.get("enum").is_none(),
        "agent_type must NOT carry an `enum` constraint (Round-2 F16 — daemon is \
         forward-compat for `ai:<future-model>`); got: {agent_type}"
    );
}

// ---------------------------------------------------------------------------
// F16 case 2: schema description guides clients to the curated set.
// ---------------------------------------------------------------------------
#[test]
fn f16_agent_type_description_names_curated_and_open_forms() {
    let tool = agent_register_schema();
    let description = tool["inputSchema"]["properties"]["agent_type"]["description"]
        .as_str()
        .expect("agent_type must carry a description");

    // Description should mention BOTH the curated labels (so clients
    // pick sensible defaults) AND the open `ai:<name>` form (so
    // forward-compat is documented). We don't pin exact wording — just
    // the two anchor strings.
    assert!(
        description.contains("human") || description.contains("system"),
        "description must name at least one curated label; got: {description}"
    );
    assert!(
        description.contains("ai:"),
        "description must name the open `ai:<name>` form; got: {description}"
    );
}

// ---------------------------------------------------------------------------
// F16 case 3: required fields unchanged.
// ---------------------------------------------------------------------------
#[test]
fn f16_agent_register_required_fields_unchanged() {
    let tool = agent_register_schema();
    let required = tool["inputSchema"]["required"]
        .as_array()
        .expect("required must be an array");
    let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();

    assert!(names.contains(&"agent_id"));
    assert!(names.contains(&"agent_type"));
}

// ---------------------------------------------------------------------------
// F16 case 4: daemon validation accepts forward-compat `ai:<future>`.
// This proves the closed enum was the lagging surface — the daemon has
// always been permissive for the `ai:` namespace.
// ---------------------------------------------------------------------------
#[test]
fn f16_daemon_accepts_forward_compat_ai_models() {
    // Curated labels.
    assert!(validate_agent_type("human").is_ok());
    assert!(validate_agent_type("system").is_ok());
    assert!(validate_agent_type("ai:claude-opus-4.7").is_ok());

    // Forward-compat `ai:<future-model>` labels — the open schema
    // documents these, the daemon accepts them.
    assert!(validate_agent_type("ai:claude-opus-4.8").is_ok());
    assert!(validate_agent_type("ai:gpt-5").is_ok());
    assert!(validate_agent_type("ai:gemini-2.5").is_ok());
}

// ---------------------------------------------------------------------------
// F16 case 5: anything OUTSIDE the curated list and `ai:` namespace is
// still rejected by the handler. The schema is open, the handler is
// not — that's documented in the description.
// ---------------------------------------------------------------------------
#[test]
fn f16_daemon_still_rejects_non_curated_non_ai_labels() {
    // These labels are NOT in the curated set and NOT in the `ai:`
    // namespace — the handler rejects with a clear diagnostic.
    assert!(
        validate_agent_type("service").is_err(),
        "`service` is not curated and not `ai:`; handler must reject"
    );
    assert!(
        validate_agent_type("ci").is_err(),
        "`ci` is not curated and not `ai:`; handler must reject"
    );
    assert!(
        validate_agent_type("test").is_err(),
        "`test` is not curated and not `ai:`; handler must reject"
    );
    // Empty agent_type is rejected too.
    assert!(validate_agent_type("").is_err());
}
