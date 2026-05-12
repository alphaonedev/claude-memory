// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_kg_timeline` handler.

use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_kg_timeline(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    validate::validate_id(source_id).map_err(|e| e.to_string())?;
    let since = params["since"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let until = params["until"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(s) = since {
        validate::validate_expires_at_format(s).map_err(|e| e.to_string())?;
    }
    if let Some(u) = until {
        validate::validate_expires_at_format(u).map_err(|e| e.to_string())?;
    }
    let limit = params["limit"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());

    let events =
        db::kg_timeline(conn, source_id, since, until, limit).map_err(|e| e.to_string())?;

    let events_json: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "target_id": e.target_id,
                "relation": e.relation,
                "valid_from": e.valid_from,
                "valid_until": e.valid_until,
                "observed_by": e.observed_by,
                "title": e.title,
                "target_namespace": e.target_namespace,
            })
        })
        .collect();

    Ok(json!({
        "source_id": source_id,
        "events": events_json,
        "count": events.len(),
    }))
}
