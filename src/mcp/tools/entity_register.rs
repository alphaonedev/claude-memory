// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_entity_register` handler.

use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_entity_register(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let canonical_name = params["canonical_name"]
        .as_str()
        .ok_or("canonical_name is required")?;
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    let aliases: Vec<String> = params["aliases"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let extra_metadata = if params["metadata"].is_object() {
        params["metadata"].clone()
    } else {
        json!({})
    };
    let explicit_agent_id = params["agent_id"].as_str();

    validate::validate_title(canonical_name).map_err(|e| e.to_string())?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;
    if let Some(aid) = explicit_agent_id {
        validate::validate_agent_id(aid).map_err(|e| e.to_string())?;
    }

    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;

    let reg = db::entity_register(
        conn,
        canonical_name,
        namespace,
        &aliases,
        &extra_metadata,
        Some(&agent_id),
    )
    .map_err(|e| e.to_string())?;

    Ok(json!({
        "entity_id": reg.entity_id,
        "canonical_name": reg.canonical_name,
        "namespace": reg.namespace,
        "aliases": reg.aliases,
        "created": reg.created,
    }))
}

// ---- C-5 (#699): close the lib-tier gap in entity_register.rs
// (currently 94.34%). Higher-level dispatcher tests cover the
// canonical_name/namespace required arms; these focus on the
// validator `.map_err(...)` branches and the metadata-object/
// agent_id presence paths. ----
#[cfg(test)]
mod tests {
    use super::*;

    fn open_conn() -> rusqlite::Connection {
        crate::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    #[test]
    fn handle_entity_register_invalid_title_maps_validator_error() {
        // Line 34: `validate_title(canonical_name).map_err(...)`. An
        // empty title is rejected by the validator.
        let conn = open_conn();
        let err = handle_entity_register(
            &conn,
            &json!({
                "canonical_name": "",
                "namespace": "test-ns",
            }),
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty(), "expected non-empty validator error");
    }

    #[test]
    fn handle_entity_register_invalid_agent_id_maps_validator_error() {
        // Line 37: `validate_agent_id(aid).map_err(...)`. The explicit
        // `agent_id` is provided but contains a forbidden character.
        let conn = open_conn();
        let err = handle_entity_register(
            &conn,
            &json!({
                "canonical_name": "Alice",
                "namespace": "test-ns",
                "agent_id": "bad agent id with spaces",
            }),
            None,
        )
        .unwrap_err();
        assert!(err.contains("agent_id"), "got: {err}");
    }

    #[test]
    fn handle_entity_register_happy_path_with_metadata_and_aliases() {
        // Drives lines 27-31 (metadata.is_object() arm), the aliases
        // filter_map collection, and the final success-return JSON.
        let conn = open_conn();
        let result = handle_entity_register(
            &conn,
            &json!({
                "canonical_name": "Bob the Builder",
                "namespace": "characters",
                "aliases": ["bob", "builder", 42 /* non-string is filtered */],
                "metadata": {"role": "construction"},
                "agent_id": "alice",
            }),
            None,
        )
        .expect("entity_register should succeed");
        assert_eq!(result["canonical_name"], "Bob the Builder");
        assert_eq!(result["namespace"], "characters");
        assert_eq!(result["created"], true);
        let aliases = result["aliases"].as_array().expect("aliases array");
        // The non-string `42` was filtered by the filter_map.
        assert!(aliases.iter().all(|v| v.is_string()));
    }
}
