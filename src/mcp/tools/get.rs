// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_get` handler.

use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_get(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    match db::resolve_id(conn, id).map_err(|e| e.to_string())? {
        Some(mem) => {
            let links = db::get_links(conn, &mem.id).unwrap_or_default();
            // Flatten: merge memory fields with links at top level (#96)
            let mut val = serde_json::to_value(&mem).map_err(|e| e.to_string())?;
            if let Some(obj) = val.as_object_mut() {
                obj.insert("links".to_string(), json!(links));
            }
            Ok(val)
        }
        None => Err("memory not found".into()),
    }
}

#[cfg(test)]
mod tests {
    //! L0.7-3 Tier B chunk-A — coverage tests for `handle_get`.
    //!
    //! Six-category template (playbook §4):
    //! A. happy path (full id) + flattened `links`
    //! B. input validation errors (missing / empty / invalid id)
    //! D. state-dependent error (id not found)
    //! E. idempotency (twice = same)
    //! plus prefix-resolution branch + link flattening when links exist.

    use super::*;
    use crate::models::{Memory, Tier};
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "test".to_string(),
            title: title.to_string(),
            content: format!("content for {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:test"}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    // A. happy path
    #[test]
    fn returns_memory_with_links_flattened() {
        let conn = fresh_conn();
        let mem = make_mem("hello");
        let id = db::insert(&conn, &mem).expect("insert");
        let out = handle_get(&conn, &json!({"id": id})).expect("ok");
        assert_eq!(out["title"].as_str(), Some("hello"));
        assert!(out["links"].is_array(), "links must be flattened in");
        assert_eq!(out["links"].as_array().unwrap().len(), 0);
    }

    // A. happy path with non-empty links
    #[test]
    fn returns_memory_with_populated_links() {
        let conn = fresh_conn();
        let a = make_mem("a");
        let b = make_mem("b");
        let a_id = db::insert(&conn, &a).expect("ins a");
        let b_id = db::insert(&conn, &b).expect("ins b");
        db::create_link(&conn, &a_id, &b_id, "related_to").expect("link");
        let out = handle_get(&conn, &json!({"id": a_id})).expect("ok");
        let links = out["links"].as_array().expect("links arr");
        assert_eq!(links.len(), 1);
    }

    // Prefix-resolution branch (resolve_id falls through to get_by_prefix)
    #[test]
    fn resolves_by_8char_prefix() {
        let conn = fresh_conn();
        let mut mem = make_mem("prefixed");
        mem.id = "01234567-aaaa-bbbb-cccc-ddddeeeeffff".to_string();
        let _ = db::insert(&conn, &mem).expect("insert");
        let out = handle_get(&conn, &json!({"id": "01234567"})).expect("prefix resolve");
        assert_eq!(out["id"].as_str(), Some(mem.id.as_str()));
    }

    // B. input validation — missing id
    #[test]
    fn missing_id_returns_error() {
        let conn = fresh_conn();
        let err = handle_get(&conn, &json!({})).unwrap_err();
        assert!(err.contains("id is required"), "got: {err}");
    }

    // B. input validation — invalid id format
    #[test]
    fn invalid_id_format_returns_error() {
        let conn = fresh_conn();
        // bad chars (space, control) → validate_id rejects
        let err = handle_get(&conn, &json!({"id": " "})).unwrap_err();
        assert!(!err.is_empty(), "validation error expected, got empty");
    }

    // D. state-dependent error — id valid but row absent
    #[test]
    fn unknown_id_returns_not_found() {
        let conn = fresh_conn();
        let err = handle_get(
            &conn,
            &json!({"id": "11111111-2222-3333-4444-555555555555"}),
        )
        .unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    // E. idempotency — calling twice yields the same answer
    #[test]
    fn idempotent_repeated_calls() {
        let conn = fresh_conn();
        let mem = make_mem("idem");
        let id = db::insert(&conn, &mem).expect("insert");
        let one = handle_get(&conn, &json!({"id": &id})).expect("ok 1");
        let two = handle_get(&conn, &json!({"id": &id})).expect("ok 2");
        assert_eq!(one["id"], two["id"]);
        assert_eq!(one["title"], two["title"]);
    }
}
