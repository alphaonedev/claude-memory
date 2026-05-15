// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-3 (issue #668) — MCP
//! `memory_dependents_of_invalidated` handler.
//!
//! Returns the list of memories whose `reflects_on` edge points at a
//! given reflection — i.e. the dependents that were (or would be)
//! flagged by the L2-3 invalidation-propagation walker if/when that
//! reflection is superseded.
//!
//! Pure read-only — does not mutate the DB or trigger the walker. The
//! walker is invoked exclusively by `mcp::tools::link::handle_link`
//! when a Reflection→Reflection `supersedes` edge lands.

use serde_json::{Value, json};

/// MCP `memory_dependents_of_invalidated` handler.
///
/// Wire shape:
///
/// ```json
/// {
///   "memory_id": "<reflection-id>",
///   "count": 3,
///   "dependents": [
///     {"id": "...", "namespace": "team/alpha"},
///     {"id": "...", "namespace": "team/alpha"},
///     {"id": "...", "namespace": "team/beta"}
///   ]
/// }
/// ```
///
/// Errors:
/// * `memory_id is required` — caller omitted the parameter.
/// * `memory_id cannot be empty`.
/// * substrate errors are bubbled up verbatim.
pub(super) fn handle_dependents_of_invalidated(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let memory_id = params["memory_id"]
        .as_str()
        .ok_or("memory_id is required")?;
    if memory_id.is_empty() {
        return Err("memory_id cannot be empty".to_string());
    }
    let dependents =
        crate::notification::invalidation::list_dependents_of_invalidated(conn, memory_id)
            .map_err(|e| format!("dependents_of_invalidated substrate error: {e}"))?;
    let rendered: Vec<Value> = dependents
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "namespace": d.namespace,
            })
        })
        .collect();
    Ok(json!({
        "memory_id": memory_id,
        "count": rendered.len(),
        "dependents": rendered,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, MemoryKind, Tier};
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str, namespace: &str, kind: MemoryKind) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: title.to_string(),
            content: format!("body {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:tester"}),
            reflection_depth: if matches!(kind, MemoryKind::Reflection) {
                1
            } else {
                0
            },
            memory_kind: kind,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    #[test]
    fn missing_memory_id_returns_error() {
        let conn = fresh_conn();
        let err = handle_dependents_of_invalidated(&conn, &json!({})).unwrap_err();
        assert!(err.contains("memory_id"));
    }

    #[test]
    fn empty_memory_id_returns_error() {
        let conn = fresh_conn();
        let err = handle_dependents_of_invalidated(&conn, &json!({"memory_id": ""})).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn unknown_id_returns_empty_envelope() {
        let conn = fresh_conn();
        let out =
            handle_dependents_of_invalidated(&conn, &json!({"memory_id": "nope-id"})).unwrap();
        assert_eq!(out["count"].as_u64(), Some(0));
        assert_eq!(out["dependents"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn returns_only_inbound_reflects_on_edges() {
        let conn = fresh_conn();
        let r1 = make_mem("R1", "ns-a", MemoryKind::Reflection);
        let m1 = make_mem("M1", "ns-a", MemoryKind::Observation);
        let m2 = make_mem("M2", "ns-b", MemoryKind::Observation);
        let m3 = make_mem("M3", "ns-a", MemoryKind::Observation);
        let r1_id = db::insert(&conn, &r1).unwrap();
        let m1_id = db::insert(&conn, &m1).unwrap();
        let m2_id = db::insert(&conn, &m2).unwrap();
        let m3_id = db::insert(&conn, &m3).unwrap();
        db::create_link(&conn, &m1_id, &r1_id, "reflects_on").unwrap();
        db::create_link(&conn, &m2_id, &r1_id, "reflects_on").unwrap();
        db::create_link(&conn, &m3_id, &r1_id, "related_to").unwrap();

        let out = handle_dependents_of_invalidated(&conn, &json!({"memory_id": r1_id})).unwrap();
        assert_eq!(out["count"].as_u64(), Some(2));
        let deps = out["dependents"].as_array().unwrap();
        let ids: Vec<&str> = deps.iter().filter_map(|d| d["id"].as_str()).collect();
        assert!(ids.contains(&m1_id.as_str()));
        assert!(ids.contains(&m2_id.as_str()));
        assert!(!ids.contains(&m3_id.as_str()), "related_to leaked");
    }
}
