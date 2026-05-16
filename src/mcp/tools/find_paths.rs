// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_find_paths` handler.

use crate::{db, validate};
use serde_json::{Value, json};

/// v0.7 J7 — `memory_find_paths` handler. Enumerates up to `max_results`
/// paths through the KG between two memories using BFS with cycle
/// detection. Backend dispatch lives in the SAL — the SQLite path goes
/// through `db::find_paths` (recursive CTE); a Postgres deployment
/// would route through `PostgresStore::find_paths` which dispatches on
/// the resolved [`crate::store::KgBackend`] (Cypher when AGE is
/// installed, recursive CTE otherwise). The wire shape is identical
/// across backends: `paths` is a list of id chains where each chain
/// has `source_id` first and `target_id` last.

pub fn handle_find_paths(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    let target_id = params["target_id"]
        .as_str()
        .ok_or("target_id is required")?;
    validate::validate_id(source_id).map_err(|e| e.to_string())?;
    validate::validate_id(target_id).map_err(|e| e.to_string())?;

    let max_depth = params["max_depth"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());
    let max_results = params["max_results"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());
    // NHI-P3-T7 (v0.7.0 NHI testing): default to "current view" —
    // exclude edges whose `valid_until` lies in the past. Caller can
    // pass `include_invalidated=true` to traverse the full historical
    // link graph (still covered by `memory_kg_timeline`).
    let include_invalidated = params["include_invalidated"].as_bool().unwrap_or(false);

    let paths = db::find_paths(
        conn,
        source_id,
        target_id,
        max_depth,
        max_results,
        include_invalidated,
    )
    .map_err(|e| {
        // Match the kg_query convention: depth-budget violations
        // surface their error message verbatim so callers can
        // distinguish "you asked for too much" from a real fault.
        e.to_string()
    })?;

    Ok(json!({
        "source_id": source_id,
        "target_id": target_id,
        "paths": paths,
        "count": paths.len(),
    }))
}
