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
