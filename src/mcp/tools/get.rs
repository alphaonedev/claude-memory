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
