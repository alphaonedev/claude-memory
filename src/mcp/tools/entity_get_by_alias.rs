// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_entity_get_by_alias` handler.

use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_entity_get_by_alias(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let alias = params["alias"].as_str().ok_or("alias is required")?;
    let namespace = params["namespace"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ns) = namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }

    match db::entity_get_by_alias(conn, alias, namespace).map_err(|e| e.to_string())? {
        Some(rec) => Ok(json!({
            "found": true,
            "entity_id": rec.entity_id,
            "canonical_name": rec.canonical_name,
            "namespace": rec.namespace,
            "aliases": rec.aliases,
        })),
        None => Ok(json!({
            "found": false,
            "entity_id": null,
            "canonical_name": null,
            "namespace": null,
            "aliases": [],
        })),
    }
}
