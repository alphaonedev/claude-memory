// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_check_agent_action` handler (issue #691).
//!
//! Read-only entry point into the substrate-level agent-action rules
//! engine. The harness's PreToolUse hook (type=`mcp_tool`) calls this
//! tool with the action it is about to execute and honors the
//! returned [`Decision`]. The engine never has authority to MODIFY
//! the action; it returns Allow / Refuse / Warn.
//!
//! # Why this is the only governance-write MCP tool
//!
//! Per issue #691 design revision 2026-05-13, MUTATION over MCP
//! stdio is explicitly disabled — `rule_add` / `rule_remove` /
//! `rule_enable` / `rule_disable` are NOT registered as MCP tools.
//! An MCP caller that tries to mutate must route through the CLI
//! (operator key on disk) or the HTTP admin endpoints
//! (`X-AI-Memory-Operator-Signature` header). `check_agent_action`
//! is the *read-side* MCP surface; it is the load-bearing tool the
//! PreToolUse hook calls on every Bash / Write / Edit dispatch.

use serde_json::{Value, json};

use crate::governance::agent_action::{AgentAction, check_agent_action};

/// Handler for `memory_check_agent_action`. Expects `arguments`:
///
/// ```json
/// {
///   "kind": "bash" | "filesystem_write" | "network_request" | "process_spawn" | "custom",
///   "command": "...",         // bash
///   "path": "...",            // filesystem_write
///   "host": "...",            // network_request
///   "binary": "...",          // process_spawn
///   "agent_id": "..."         // optional; defaults to the MCP-resolved id
/// }
/// ```
///
/// Returns a JSON object with the [`crate::governance::agent_action::Decision`]
/// shape (`{"decision":"allow"}` / `{"decision":"refuse","rule_id":...,"reason":...}`
/// / `{"decision":"warn","rule_id":...,"reason":...}`).
pub fn handle_check_agent_action(
    conn: &rusqlite::Connection,
    arguments: &Value,
) -> Result<Value, String> {
    let kind = arguments
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "kind is required".to_string())?;
    let action = build_action(kind, arguments)?;
    let agent_id = arguments
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("anonymous:mcp")
        .to_string();
    let decision = check_agent_action(conn, &agent_id, &action).map_err(|e| e.to_string())?;
    Ok(json!({
        "decision": decision,
        "kind": kind,
        "agent_id": agent_id,
    }))
}

fn build_action(kind: &str, arguments: &Value) -> Result<AgentAction, String> {
    use std::path::PathBuf;

    match kind {
        "bash" => {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "bash kind requires `command`".to_string())?
                .to_string();
            let cwd = arguments
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            Ok(AgentAction::Bash { command, cwd })
        }
        "filesystem_write" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "filesystem_write kind requires `path`".to_string())?
                .to_string();
            let byte_estimate = arguments.get("byte_estimate").and_then(Value::as_u64);
            Ok(AgentAction::FilesystemWrite {
                path: PathBuf::from(path),
                byte_estimate,
            })
        }
        "network_request" => {
            let host = arguments
                .get("host")
                .and_then(Value::as_str)
                .ok_or_else(|| "network_request kind requires `host`".to_string())?
                .to_string();
            let scheme = arguments
                .get("scheme")
                .and_then(Value::as_str)
                .unwrap_or("https")
                .to_string();
            Ok(AgentAction::NetworkRequest { host, scheme })
        }
        "process_spawn" => {
            let binary = arguments
                .get("binary")
                .and_then(Value::as_str)
                .ok_or_else(|| "process_spawn kind requires `binary`".to_string())?
                .to_string();
            let args = arguments
                .get("args")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(AgentAction::ProcessSpawn { binary, args })
        }
        "custom" => {
            let custom_kind = arguments
                .get("custom_kind")
                .or_else(|| arguments.get("kind_inner"))
                .and_then(Value::as_str)
                .ok_or_else(|| "custom kind requires `custom_kind`".to_string())?
                .to_string();
            Ok(AgentAction::Custom {
                custom_kind,
                payload: arguments.clone(),
            })
        }
        other => Err(format!("unknown kind `{other}`")),
    }
}

/// Reusable refusal value for rule-mutation tools that are
/// explicitly disabled over MCP. Wired by `mcp/mod.rs` if a future
/// caller tries to invoke a mutation tool name — today the
/// mutation tools are simply not registered, so the dispatch returns
/// "unknown tool". This constant is kept around for the wire-name
/// stability test in `tests/governance_immutability.rs`.
// Stable wire string consumed by `tests/governance_immutability.rs` to
// pin the error returned when a future caller tries to mutate rules
// over MCP. The mutation tools are NOT registered today, so the
// dispatch returns "unknown tool" instead — this constant documents
// the canonical error vocabulary the test suite asserts on.
#[allow(dead_code)]
pub const MCP_MUTATION_DISABLED_ERROR: &str = "governance.not_available_over_mcp: rule mutation is operator-only \
     (CLI `ai-memory rules` or HTTP `POST /api/v1/governance/rules`)";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::rules_store::{self, Rule};

    fn fresh_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE governance_rules (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 matcher TEXT NOT NULL,
                 severity TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 namespace TEXT NOT NULL DEFAULT '_global',
                 created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned'
             );
             CREATE TABLE signed_events (
                 id TEXT PRIMARY KEY,
                 agent_id TEXT NOT NULL,
                 event_type TEXT NOT NULL,
                 payload_hash BLOB NOT NULL,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned',
                 timestamp TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn missing_kind_errors() {
        let conn = fresh_conn();
        let r = handle_check_agent_action(&conn, &json!({}));
        assert!(r.is_err());
    }

    #[test]
    fn bash_kind_allows_when_no_rule() {
        let conn = fresh_conn();
        let r = handle_check_agent_action(&conn, &json!({"kind":"bash","command":"ls"})).unwrap();
        assert_eq!(r["decision"]["decision"], "allow");
    }

    #[test]
    fn filesystem_write_kind_refuses_on_glob() {
        let conn = fresh_conn();
        rules_store::insert(
            &conn,
            &Rule {
                id: "R001".into(),
                kind: "filesystem_write".into(),
                matcher: r#"{"glob":"/tmp/**"}"#.into(),
                severity: "refuse".into(),
                reason: "no /tmp".into(),
                namespace: "_global".into(),
                created_by: "test".into(),
                created_at: 0,
                enabled: true,
                signature: None,
                attest_level: "unsigned".into(),
            },
        )
        .unwrap();
        let r =
            handle_check_agent_action(&conn, &json!({"kind":"filesystem_write","path":"/tmp/foo"}))
                .unwrap();
        assert_eq!(r["decision"]["decision"], "refuse");
        assert_eq!(r["decision"]["rule_id"], "R001");
    }

    #[test]
    fn unknown_kind_errors() {
        let conn = fresh_conn();
        let r = handle_check_agent_action(&conn, &json!({"kind":"nope"}));
        assert!(r.is_err());
    }

    #[test]
    fn missing_required_field_errors() {
        let conn = fresh_conn();
        let r = handle_check_agent_action(&conn, &json!({"kind":"bash"}));
        assert!(r.is_err());
    }

    #[test]
    fn mutation_disabled_error_string_is_stable() {
        assert!(MCP_MUTATION_DISABLED_ERROR.starts_with("governance.not_available_over_mcp"));
    }
}
