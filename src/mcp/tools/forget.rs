// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_forget` and `memory_stats` handlers.

use crate::db;
use crate::models::Tier;
use serde_json::{Value, json};
use std::path::Path;
pub(super) fn handle_forget(
    conn: &rusqlite::Connection,
    params: &Value,
    archive: bool,
) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let pattern = params["pattern"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let dry_run = params["dry_run"].as_bool().unwrap_or(false);

    if dry_run {
        let count =
            db::forget_count(conn, namespace, pattern, tier.as_ref()).map_err(|e| e.to_string())?;
        return Ok(json!({"would_delete": count, "dry_run": true}));
    }

    let deleted =
        db::forget(conn, namespace, pattern, tier.as_ref(), archive).map_err(|e| e.to_string())?;
    Ok(json!({"deleted": deleted, "archived": archive}))
}

pub(super) fn handle_stats(conn: &rusqlite::Connection, db_path: &Path) -> Result<Value, String> {
    let stats = db::stats(conn, db_path).map_err(|e| e.to_string())?;
    serde_json::to_value(stats).map_err(|e| e.to_string())
}
