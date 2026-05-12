// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_search` handler.

use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_search(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let query = params["query"].as_str().ok_or("query is required")?;
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    // Ultrareview #339: saturate instead of panic on 32-bit targets
    // where u64 may exceed usize::MAX. A malicious client passing
    // limit=2^63 would otherwise take down the daemon.
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(20)).unwrap_or(usize::MAX);

    let agent_id = params["agent_id"].as_str();
    if let Some(aid) = agent_id {
        validate::validate_agent_id(aid).map_err(|e| e.to_string())?;
    }
    let as_agent = params["as_agent"].as_str();
    if let Some(a) = as_agent {
        validate::validate_namespace(a).map_err(|e| e.to_string())?;
    }
    let results = db::search(
        conn,
        query,
        namespace,
        tier.as_ref(),
        limit.min(200),
        None,
        None,
        None,
        None,
        agent_id,
        as_agent,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"results": results, "count": results.len()}))
}
