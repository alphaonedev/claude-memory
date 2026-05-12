// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_list` handler.

use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    // Ultrareview #339: saturate instead of panic (see handle_search).
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(20)).unwrap_or(usize::MAX);
    let agent_id = params["agent_id"].as_str();
    if let Some(aid) = agent_id {
        validate::validate_agent_id(aid).map_err(|e| e.to_string())?;
    }

    let results = db::list(
        conn,
        namespace,
        tier.as_ref(),
        limit.min(200),
        0,
        None,
        None,
        None,
        None,
        agent_id,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"memories": results, "count": results.len()}))
}
