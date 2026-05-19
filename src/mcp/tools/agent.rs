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

    // #913 (security-medium / SOC2, 2026-05-19) — admin/state-change
    // audit. Registering an agent_id mints a new principal in the
    // `_agents` namespace; emit the forensic-chain row BEFORE the
    // storage write so the audit trail captures intent regardless of
    // downstream storage outcome. Mirrors the #911 HTTP fix.
    let caller = crate::identity::resolve_agent_id(params["caller_agent_id"].as_str(), None)
        .unwrap_or_else(|_| "anonymous:invalid".to_string());
    crate::governance::audit::record_decision(
        &caller,
        "allow",
        "register_agent",
        "",
        json!({
            "new_agent_id": agent_id,
            "agent_type": agent_type,
            "capabilities": &capabilities,
        }),
    );

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

// ---- C-5 (#699): unit coverage for the `pub(super)` handlers. The MCP
// dispatch layer covers the happy paths; these focus on the validator
// `.map_err(...)` arms that map domain errors into `Err(String)` for the
// MCP envelope — the missing branches at lib-tier 91.30%. ----
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_conn() -> rusqlite::Connection {
        crate::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    #[test]
    fn handle_agent_register_missing_agent_id_errors() {
        let conn = open_conn();
        let err = handle_agent_register(&conn, &json!({"agent_type": "ai:bot"})).unwrap_err();
        assert!(err.contains("agent_id"), "got: {err}");
    }

    #[test]
    fn handle_agent_register_missing_agent_type_errors() {
        let conn = open_conn();
        let err = handle_agent_register(&conn, &json!({"agent_id": "alice"})).unwrap_err();
        assert!(err.contains("agent_type"), "got: {err}");
    }

    #[test]
    fn handle_agent_register_invalid_agent_id_maps_validator_error() {
        // Empty agent_id is parsed (as_str returns Some("")) and then
        // validated; the validator rejects empty IDs. Covers the
        // `validate_agent_id(...).map_err(...)` Err arm.
        let conn = open_conn();
        let err = handle_agent_register(&conn, &json!({"agent_id": "", "agent_type": "ai:bot"}))
            .unwrap_err();
        assert!(err.contains("agent_id"), "got: {err}");
    }

    #[test]
    fn handle_agent_register_invalid_capabilities_maps_validator_error() {
        // Capability strings have validation rules; an empty string is
        // rejected. Covers `validate_capabilities(...).map_err(...)`.
        let conn = open_conn();
        let err = handle_agent_register(
            &conn,
            &json!({
                "agent_id": "alice",
                "agent_type": "ai:bot",
                "capabilities": [""],
            }),
        )
        .unwrap_err();
        // Either capability-specific or empty-string complaint.
        assert!(!err.is_empty(), "expected non-empty error message");
    }

    #[test]
    fn handle_agent_register_capabilities_defaults_when_absent() {
        // When `capabilities` is absent, the `.unwrap_or_default()`
        // branch fires (line 23). Together with a happy-path
        // registration this hits the success-return JSON body.
        let conn = open_conn();
        let result =
            handle_agent_register(&conn, &json!({"agent_id": "bob", "agent_type": "ai:bot"}))
                .expect("register should succeed without capabilities");
        assert_eq!(result["registered"], true);
        assert_eq!(result["agent_id"], "bob");
        assert!(result["capabilities"].is_array());
        assert_eq!(result["capabilities"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn handle_agent_list_on_empty_db_returns_zero_count() {
        let conn = open_conn();
        let result = handle_agent_list(&conn).expect("list should succeed");
        assert_eq!(result["count"], 0);
        assert!(result["agents"].is_array());
    }

    #[test]
    fn messages_namespace_for_prepends_messages_prefix() {
        assert_eq!(messages_namespace_for("alice"), "_messages/alice");
        assert_eq!(
            messages_namespace_for("ai:claude@host:pid-1"),
            "_messages/ai:claude@host:pid-1"
        );
        // Empty input is allowed by this helper (validator runs elsewhere).
        assert_eq!(messages_namespace_for(""), "_messages/");
    }
}
