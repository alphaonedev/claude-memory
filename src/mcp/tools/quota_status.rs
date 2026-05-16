// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_quota_status` handler.

use serde_json::{Value, json};
/// v0.7 K8 — MCP handler for `memory_quota_status`. Reports per-agent
/// quota usage (memories/day, storage bytes, links/day) for the
/// operator-facing surface. When `agent_id` is provided, returns a
/// single row (auto-inserting a default row if the agent has none).
/// When omitted, returns every quota row in the substrate, sorted by
/// agent_id ASC. Family: `Power` (operator-scoped, not data-plane).

pub fn handle_quota_status(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    if let Some(agent_id) = params.get("agent_id").and_then(Value::as_str) {
        let row = crate::quotas::get_status(conn, agent_id).map_err(|e| e.to_string())?;
        Ok(json!({
            "agent_id": agent_id,
            "quota": row,
        }))
    } else {
        let rows = crate::quotas::list_status(conn).map_err(|e| e.to_string())?;
        Ok(json!({
            "count": rows.len(),
            "quotas": rows,
        }))
    }
}

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for `handle_quota_status`.
    //!
    //! Two paths to cover:
    //! - per-agent: a missing row auto-inserts and surfaces the default quota
    //! - list: returns every row in the substrate

    use super::*;
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    // Per-agent path: auto-inserts a default row if absent.
    #[test]
    fn per_agent_returns_quota_for_unknown_id() {
        let conn = fresh_conn();
        let resp = handle_quota_status(&conn, &json!({"agent_id": "ai:alice"})).expect("ok");
        assert_eq!(resp["agent_id"].as_str(), Some("ai:alice"));
        let quota = &resp["quota"];
        assert!(quota.is_object());
        assert_eq!(quota["agent_id"].as_str(), Some("ai:alice"));
        // Defaults should set non-zero ceilings.
        assert!(quota["max_memories_per_day"].as_i64().unwrap_or(0) > 0);
    }

    // List path: omitted agent_id returns the count + rows shape.
    #[test]
    fn list_path_returns_count_and_rows() {
        let conn = fresh_conn();
        // Pre-populate via the per-agent path so list has data to show.
        let _ = handle_quota_status(&conn, &json!({"agent_id": "ai:bob"})).expect("seed bob");
        let _ = handle_quota_status(&conn, &json!({"agent_id": "ai:carol"})).expect("seed carol");
        let resp = handle_quota_status(&conn, &json!({})).expect("ok");
        assert!(resp["count"].as_u64().unwrap() >= 2);
        let quotas = resp["quotas"].as_array().expect("quotas array");
        assert!(quotas.len() >= 2);
    }

    // List path on empty DB returns count=0 and empty array.
    #[test]
    fn list_path_empty_db_returns_zero() {
        let conn = fresh_conn();
        let resp = handle_quota_status(&conn, &json!({})).expect("ok");
        assert_eq!(resp["count"].as_u64(), Some(0));
        assert_eq!(resp["quotas"].as_array().unwrap().len(), 0);
    }

    // Per-agent path on the same id twice is idempotent.
    #[test]
    fn per_agent_idempotent_repeated_reads() {
        let conn = fresh_conn();
        let one = handle_quota_status(&conn, &json!({"agent_id": "ai:dup"})).expect("ok1");
        let two = handle_quota_status(&conn, &json!({"agent_id": "ai:dup"})).expect("ok2");
        assert_eq!(one["agent_id"], two["agent_id"]);
        assert_eq!(
            one["quota"]["max_memories_per_day"],
            two["quota"]["max_memories_per_day"]
        );
    }
}
