// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP namespace standard-policy handlers and governance helpers.

use crate::models::GovernancePolicy;
use crate::{db, validate};
use serde_json::{Value, json};
pub(crate) fn handle_namespace_set_standard(
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
    let governance_val = params.get("governance").filter(|v| !v.is_null());
    if let Some(g) = governance_val {
        let policy: crate::models::GovernancePolicy =
            serde_json::from_value(g.clone()).map_err(|e| format!("invalid governance: {e}"))?;
        validate::validate_governance_policy(&policy).map_err(|e| e.to_string())?;

        // Load the standard memory, merge metadata.governance, write back.
        let mut mem = db::get(conn, id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("memory not found: {id}"))?;
        let mut metadata = if mem.metadata.is_object() {
            mem.metadata.clone()
        } else {
            serde_json::json!({})
        };
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "governance".to_string(),
                serde_json::to_value(&policy).map_err(|e| e.to_string())?,
            );
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

// ---------------------------------------------------------------------------
// Archive tool handlers
// ---------------------------------------------------------------------------
