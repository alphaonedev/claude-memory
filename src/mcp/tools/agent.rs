// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP agent-registration and agent-list handlers.

use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_agent_register(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let agent_id = params["agent_id"].as_str().ok_or("agent_id is required")?;
    let agent_type = params["agent_type"]
        .as_str()
        .ok_or("agent_type is required")?;
    let capabilities: Vec<String> = params["capabilities"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    validate::validate_agent_id(agent_id).map_err(|e| e.to_string())?;
    validate::validate_agent_type(agent_type).map_err(|e| e.to_string())?;
    validate::validate_capabilities(&capabilities).map_err(|e| e.to_string())?;

    let id =
        db::register_agent(conn, agent_id, agent_type, &capabilities).map_err(|e| e.to_string())?;

    Ok(json!({
        "registered": true,
        "id": id,
        "agent_id": agent_id,
        "agent_type": agent_type,
        "capabilities": capabilities,
    }))
}

pub(super) fn handle_agent_list(conn: &rusqlite::Connection) -> Result<Value, String> {
    let agents = db::list_agents(conn).map_err(|e| e.to_string())?;
    Ok(json!({
        "count": agents.len(),
        "agents": agents,
    }))
}

// --- v0.6.0.0 agent notify / inbox -----------------------------------------

/// Compose the canonical inbox namespace for a given `agent_id`.
///
/// Reuses the same sanitization regex that `validate_namespace` enforces
/// on writes, so any `agent_id` that passes `validate::validate_agent_id`
/// produces an acceptable namespace here.
pub(super) fn messages_namespace_for(agent_id: &str) -> String {
    format!("_messages/{agent_id}")
}
