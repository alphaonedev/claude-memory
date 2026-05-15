// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP namespace standard-policy handlers and governance helpers.

use crate::models::GovernancePolicy;
use crate::{db, validate};
use serde_json::{Value, json};
pub fn handle_namespace_set_standard(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let parent = params["parent"].as_str();
    if let Some(p) = parent {
        validate::validate_namespace(p).map_err(|e| e.to_string())?;
    }

    // Task 1.8: optional governance policy merged into the standard memory's
    // metadata.governance. Policy is deserialized + validated before write.
    //
    // v0.7.0 G-PHASE-E-2 (#707) — DO NOT strip "extra" fields from the
    // governance blob. The pre-#707 path round-tripped the incoming
    // governance JSON through the typed `GovernancePolicy` struct, which
    // only carries the whitelist (write/promote/delete/approver/inherit/
    // max_reflection_depth). Any other key — most notably
    // `require_approval_above_depth`, which is a free-function look-up
    // (`storage::resolve_require_approval_above_depth`) outside the
    // typed struct — was silently dropped on re-serialisation. Operators
    // who set `require_approval_above_depth` on a memory and later
    // touched `memory_namespace_set_standard` for any reason lost that
    // gate without any error or log.
    //
    // The fix: take the existing standard memory's `metadata.governance`
    // (if any) as the base, layer the incoming `g` on top key-by-key,
    // validate the merged blob's typed shape, and write the FULL merged
    // JSON back — so unknown-to-the-struct fields on either side survive
    // the round-trip.
    let governance_val = params.get("governance").filter(|v| !v.is_null());
    if let Some(g) = governance_val {
        // Load the standard memory first so we can read its existing
        // governance blob and merge.
        let mut mem = db::get(conn, id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("memory not found: {id}"))?;
        // Compute the merged governance JSON: existing fields preserved,
        // incoming overrides applied per-key.
        let merged = merge_governance_fields(mem.metadata.get("governance"), g);
        // Validate the typed shape of the result. Deserialising drops
        // unknown fields but the typed sub-set must still parse + pass
        // policy validation — this catches operator typos in known
        // fields without rejecting extras like
        // `require_approval_above_depth`.
        let policy: crate::models::GovernancePolicy = serde_json::from_value(merged.clone())
            .map_err(|e| format!("invalid governance: {e}"))?;
        validate::validate_governance_policy(&policy).map_err(|e| e.to_string())?;

        let mut metadata = if mem.metadata.is_object() {
            mem.metadata.clone()
        } else {
            serde_json::json!({})
        };
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("governance".to_string(), merged);
        }
        let (found, _) = db::update(
            conn,
            &mem.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&metadata),
        )
        .map_err(|e| e.to_string())?;
        if !found {
            return Err(format!("memory not found during governance merge: {id}"));
        }
        mem.metadata = metadata;
    }

    db::set_namespace_standard(conn, namespace, id, parent).map_err(|e| e.to_string())?;
    let mut resp = json!({"set": true, "namespace": namespace, "standard_id": id});
    if let Some(p) = parent {
        resp["parent"] = json!(p);
    }
    if let Some(g) = governance_val {
        resp["governance"] = g.clone();
    }
    Ok(resp)
}

pub(crate) fn handle_namespace_get_standard(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;

    // Task 1.6: --inherit returns the full resolved chain, most-general-first.
    let inherit = params["inherit"].as_bool().unwrap_or(false);
    if inherit {
        let chain = super::build_namespace_chain(conn, namespace);
        let mut standards: Vec<Value> = Vec::new();
        for link in &chain {
            if let Some(std) = super::lookup_namespace_standard(conn, link) {
                let gov = extract_governance(&std);
                let entry = json!({
                    "namespace": link,
                    "standard_id": std["id"].clone(),
                    "title": std["title"].clone(),
                    "content": std["content"].clone(),
                    "priority": std["priority"].clone(),
                    "governance": gov,
                });
                standards.push(entry);
            }
        }
        return Ok(json!({
            "namespace": namespace,
            "chain": chain,
            "standards": standards,
            "count": standards.len(),
        }));
    }

    let standard_id = db::get_namespace_standard(conn, namespace).map_err(|e| e.to_string())?;
    match standard_id {
        Some(id) => {
            let mem = db::get(conn, &id).map_err(|e| e.to_string())?;
            match mem {
                Some(m) => {
                    // Task 1.8: surface metadata.governance (or default policy).
                    let gov = GovernancePolicy::from_metadata(&m.metadata)
                        .map(Result::unwrap_or_default)
                        .unwrap_or_default();
                    Ok(json!({
                        "namespace": namespace,
                        "standard_id": id,
                        "title": m.title,
                        "content": m.content,
                        "priority": m.priority,
                        "governance": gov,
                    }))
                }
                None => Ok(
                    json!({"namespace": namespace, "standard_id": id, "warning": "standard memory not found — may have been deleted"}),
                ),
            }
        }
        None => Ok(json!({"namespace": namespace, "standard_id": null})),
    }
}

/// Task 1.8 — extract metadata.governance from a serialized memory value,
/// resolving to the default policy when missing or invalid. Used by the
/// `--inherit` get-standard path and tool responses.
pub(super) fn extract_governance(mem_val: &Value) -> Value {
    let default = serde_json::to_value(GovernancePolicy::default()).unwrap_or(Value::Null);
    let Some(meta) = mem_val.get("metadata") else {
        return default;
    };
    match GovernancePolicy::from_metadata(meta) {
        Some(Ok(p)) => serde_json::to_value(&p).unwrap_or(default),
        _ => default,
    }
}

pub(crate) fn handle_namespace_clear_standard(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;
    let cleared = db::clear_namespace_standard(conn, namespace).map_err(|e| e.to_string())?;
    Ok(json!({"cleared": cleared, "namespace": namespace}))
}

/// Auto-register namespace parent chain from the filesystem path.
/// Walks from cwd up to home dir, checks if each directory name has a namespace
/// standard set, and registers the parent chain.
///
/// Example: cwd = /home/user/monorepo/frontend
///   → checks "frontend" (cwd), "monorepo" (parent), stops at home dir
///   → if "monorepo" has a standard, sets `parent_namespace` of "frontend" to "monorepo"
#[allow(dead_code)]
pub(super) fn auto_register_path_hierarchy(conn: &rusqlite::Connection, namespace: &str) {
    // Only run if this namespace doesn't already have an explicit parent
    if db::get_namespace_parent(conn, namespace).is_some() {
        return;
    }
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let home = dirs::home_dir().unwrap_or_default();
    // Walk up from parent of cwd (cwd itself IS the namespace)
    let mut current = cwd.parent().map(std::path::Path::to_path_buf);
    while let Some(dir) = current {
        // Stop at or above home directory
        if dir == home || !dir.starts_with(&home) {
            break;
        }
        if let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()) {
            // Check if this directory name has a namespace standard
            if db::get_namespace_standard(conn, dir_name)
                .ok()
                .flatten()
                .is_some()
            {
                // Found a parent with a standard — register it
                let now = chrono::Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "UPDATE namespace_meta SET parent_namespace = ?1, updated_at = ?2 WHERE namespace = ?3 AND parent_namespace IS NULL",
                    rusqlite::params![dir_name, now, namespace],
                );
                tracing::info!(
                    "auto-registered parent namespace: {} -> {}",
                    namespace,
                    dir_name
                );
                break;
            }
        }
        current = dir.parent().map(std::path::Path::to_path_buf);
    }
}

/// v0.7.0 G-PHASE-E-2 (#707) — merge an incoming governance JSON blob
/// onto an existing one, key-by-key. The incoming blob's keys override
/// the existing ones; keys present only on the existing blob (e.g. an
/// operator-set `require_approval_above_depth`) survive untouched.
///
/// Both sides are treated as JSON objects — non-object inputs (or
/// missing `existing`) collapse to "use the incoming side wholesale".
/// Returns a fresh `serde_json::Value::Object` so the caller can
/// re-serialise without aliasing the input slots.
fn merge_governance_fields(
    existing: Option<&serde_json::Value>,
    incoming: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = serde_json::Map::new();
    if let Some(existing_obj) = existing.and_then(serde_json::Value::as_object) {
        for (k, v) in existing_obj {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Some(incoming_obj) = incoming.as_object() {
        for (k, v) in incoming_obj {
            merged.insert(k.clone(), v.clone());
        }
    } else {
        // Incoming is not an object — fall back to the incoming value
        // wholesale so callers can still pass primitives through (the
        // typed deserialise on the caller side will reject anything
        // structurally wrong).
        return incoming.clone();
    }
    serde_json::Value::Object(merged)
}

// ---------------------------------------------------------------------------
// Archive tool handlers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for the namespace-standard MCP handlers
    //! and the private `extract_governance` helper.

    use super::*;
    use crate::models::{Memory, Tier};
    use crate::storage as db;
    use serde_json::json;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn insert_one(conn: &rusqlite::Connection, ns: &str, title: &str) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("body for {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        db::insert(conn, &mem).expect("insert")
    }

    // set_standard: happy path without governance.
    #[test]
    fn set_standard_happy_path() {
        let conn = fresh_conn();
        let id = insert_one(&conn, "ns-a", "standard");
        let resp = handle_namespace_set_standard(&conn, &json!({"namespace": "ns-a", "id": id}))
            .expect("ok");
        assert_eq!(resp["set"], true);
    }

    // set_standard: with parent.
    #[test]
    fn set_standard_with_parent_echoed() {
        let conn = fresh_conn();
        // Seed the parent standard first.
        let parent_id = insert_one(&conn, "parent", "p-standard");
        db::set_namespace_standard(&conn, "parent", &parent_id, None).unwrap();
        let id = insert_one(&conn, "child", "c-standard");
        let resp = handle_namespace_set_standard(
            &conn,
            &json!({"namespace": "child", "id": id, "parent": "parent"}),
        )
        .expect("ok");
        assert_eq!(resp["parent"].as_str(), Some("parent"));
    }

    // set_standard: missing namespace → typed error.
    #[test]
    fn set_standard_missing_namespace_errors() {
        let conn = fresh_conn();
        let err = handle_namespace_set_standard(&conn, &json!({"id": "x"})).unwrap_err();
        assert!(err.contains("namespace"), "got: {err}");
    }

    // set_standard: missing id → typed error.
    #[test]
    fn set_standard_missing_id_errors() {
        let conn = fresh_conn();
        let err = handle_namespace_set_standard(&conn, &json!({"namespace": "x"})).unwrap_err();
        assert!(err.contains("id"), "got: {err}");
    }

    // set_standard: invalid namespace rejected (validate_namespace).
    #[test]
    fn set_standard_invalid_namespace_rejected() {
        let conn = fresh_conn();
        let err =
            handle_namespace_set_standard(&conn, &json!({"namespace": "has spaces", "id": "x"}))
                .unwrap_err();
        assert!(!err.is_empty());
    }

    // set_standard: invalid parent namespace rejected.
    #[test]
    fn set_standard_invalid_parent_rejected() {
        let conn = fresh_conn();
        let id = insert_one(&conn, "ns-parent-bad", "p");
        let err = handle_namespace_set_standard(
            &conn,
            &json!({"namespace": "ns-parent-bad", "id": id, "parent": "has spaces"}),
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    // set_standard: with governance — merged into metadata + echoed.
    #[test]
    fn set_standard_with_governance_merged() {
        let conn = fresh_conn();
        let id = insert_one(&conn, "ns-gov", "p");
        // GovernancePolicy schema requires `write`; other fields default.
        let governance = json!({"write": "any"});
        let resp = handle_namespace_set_standard(
            &conn,
            &json!({"namespace": "ns-gov", "id": id, "governance": governance.clone()}),
        )
        .expect("ok");
        assert_eq!(resp["governance"], governance);
        // The merged metadata must round-trip through db::get.
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert!(mem.metadata.get("governance").is_some());
    }

    // set_standard: invalid governance (deserialization fails).
    #[test]
    fn set_standard_with_invalid_governance_rejected() {
        let conn = fresh_conn();
        let id = insert_one(&conn, "ns-bad-gov", "p");
        let err = handle_namespace_set_standard(
            &conn,
            &json!({
                "namespace": "ns-bad-gov",
                "id": id,
                "governance": "this is not an object",
            }),
        )
        .unwrap_err();
        assert!(err.contains("invalid governance"), "got: {err}");
    }

    // set_standard: governance specified but memory id does not exist.
    #[test]
    fn set_standard_with_governance_unknown_id_errors() {
        let conn = fresh_conn();
        let err = handle_namespace_set_standard(
            &conn,
            &json!({
                "namespace": "ns-missing-id",
                "id": "11111111-2222-3333-4444-555555555555",
                "governance": {"write": "any"},
            }),
        )
        .unwrap_err();
        assert!(err.contains("memory not found"), "got: {err}");
    }

    // get_standard: missing namespace → typed error.
    #[test]
    fn get_standard_missing_namespace_errors() {
        let conn = fresh_conn();
        let err = handle_namespace_get_standard(&conn, &json!({})).unwrap_err();
        assert!(err.contains("namespace"), "got: {err}");
    }

    // get_standard: unknown namespace → standard_id null.
    #[test]
    fn get_standard_unknown_namespace_returns_null() {
        let conn = fresh_conn();
        let resp =
            handle_namespace_get_standard(&conn, &json!({"namespace": "no-such"})).expect("ok");
        assert!(resp["standard_id"].is_null());
    }

    // get_standard: happy path.
    #[test]
    fn get_standard_happy_path() {
        let conn = fresh_conn();
        let id = insert_one(&conn, "ns-get", "got");
        db::set_namespace_standard(&conn, "ns-get", &id, None).unwrap();
        let resp =
            handle_namespace_get_standard(&conn, &json!({"namespace": "ns-get"})).expect("ok");
        assert_eq!(resp["standard_id"].as_str(), Some(id.as_str()));
        assert_eq!(resp["title"].as_str(), Some("got"));
        // governance defaults filled in.
        assert!(resp["governance"].is_object());
    }

    // get_standard: standard_id present but memory deleted — warning surfaced.
    #[test]
    fn get_standard_dangling_id_surfaces_warning() {
        let conn = fresh_conn();
        let id = insert_one(&conn, "ns-dangling", "g");
        db::set_namespace_standard(&conn, "ns-dangling", &id, None).unwrap();
        // Now physically delete the memory row.
        conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![&id])
            .unwrap();
        let resp =
            handle_namespace_get_standard(&conn, &json!({"namespace": "ns-dangling"})).expect("ok");
        assert!(resp["warning"].is_string());
    }

    // get_standard: --inherit returns chain + standards array.
    #[test]
    fn get_standard_inherit_returns_chain() {
        let conn = fresh_conn();
        let global_id = insert_one(&conn, "*", "global");
        db::set_namespace_standard(&conn, "*", &global_id, None).unwrap();
        let leaf_id = insert_one(&conn, "leaf-ns", "leaf");
        db::set_namespace_standard(&conn, "leaf-ns", &leaf_id, None).unwrap();
        let resp =
            handle_namespace_get_standard(&conn, &json!({"namespace": "leaf-ns", "inherit": true}))
                .expect("ok");
        assert!(resp["chain"].is_array());
        assert!(resp["count"].as_u64().unwrap() >= 1);
    }

    // clear_standard: happy.
    #[test]
    fn clear_standard_happy() {
        let conn = fresh_conn();
        let id = insert_one(&conn, "ns-clear", "c");
        db::set_namespace_standard(&conn, "ns-clear", &id, None).unwrap();
        let resp =
            handle_namespace_clear_standard(&conn, &json!({"namespace": "ns-clear"})).expect("ok");
        assert_eq!(resp["cleared"], true);
    }

    // clear_standard: missing namespace → error.
    #[test]
    fn clear_standard_missing_namespace_errors() {
        let conn = fresh_conn();
        let err = handle_namespace_clear_standard(&conn, &json!({})).unwrap_err();
        assert!(err.contains("namespace"), "got: {err}");
    }

    // clear_standard: invalid namespace rejected.
    #[test]
    fn clear_standard_invalid_namespace_rejected() {
        let conn = fresh_conn();
        let err = handle_namespace_clear_standard(&conn, &json!({"namespace": "has spaces"}))
            .unwrap_err();
        assert!(!err.is_empty());
    }

    // extract_governance: empty metadata returns default policy.
    #[test]
    fn extract_governance_default_when_missing_metadata() {
        let val = json!({"id": "x"});
        let gov = extract_governance(&val);
        assert!(gov.is_object());
    }

    // extract_governance: full metadata with valid governance.
    #[test]
    fn extract_governance_round_trips_valid_policy() {
        let val = json!({
            "metadata": {
                "governance": {"min_priority": 0}
            }
        });
        let gov = extract_governance(&val);
        assert!(
            gov.is_object(),
            "expected default-or-resolved policy object"
        );
    }

    // auto_register_path_hierarchy: no-op when parent already set.
    #[test]
    fn auto_register_noop_when_parent_already_set() {
        let conn = fresh_conn();
        // Seed parent_namespace via direct SQL to bypass auto-detect.
        conn.execute(
            "INSERT INTO namespace_meta (namespace, standard_id, updated_at, parent_namespace)
             VALUES ('child-ns', NULL, '2026-01-01T00:00:00Z', 'set-parent')",
            [],
        )
        .unwrap();
        // Should be a no-op: assert the function does not panic and the
        // parent_namespace value survives.
        auto_register_path_hierarchy(&conn, "child-ns");
        let p: Option<String> = conn
            .query_row(
                "SELECT parent_namespace FROM namespace_meta WHERE namespace = 'child-ns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(p.as_deref(), Some("set-parent"));
    }

    // auto_register_path_hierarchy: no namespace_meta row → no-op (no panic).
    #[test]
    fn auto_register_handles_missing_row_gracefully() {
        let conn = fresh_conn();
        // Should not panic when nothing is set up.
        auto_register_path_hierarchy(&conn, "non-existent-ns");
    }
}
