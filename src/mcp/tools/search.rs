// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_search` handler.

use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_search(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let query = params["query"].as_str();
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
    // v0.7.0 WT-1-E — atom-preference search semantics. See
    // `mcp::tools::recall::handle_recall` for the full contract.
    let include_archived = params["include_archived"].as_bool().unwrap_or(false);
    // v0.7.0 Provenance Gap 6 (#889) — reciprocal source filter.
    // When `source_uri` is supplied + non-empty, results are
    // narrowed to memories whose `source_uri` column exactly matches.
    // The partial `idx_memories_source_uri` index (v38) covers the
    // lookup so the reciprocal "everything from this document"
    // query is O(log N), not O(N) JSON-path scan.
    let source_uri = params["source_uri"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(uri) = source_uri {
        validate::validate_source_uri(uri).map_err(|e| e.to_string())?;
    }

    // When `query` is empty but `source_uri` is supplied, route through
    // the index-only `list_by_source_uri` so callers can ask "give me
    // every memory from this document" without typing a query token.
    if query.unwrap_or("").trim().is_empty() {
        if let Some(uri) = source_uri {
            let results = db::list_by_source_uri(conn, uri, namespace, Some(limit.min(200)))
                .map_err(|e| e.to_string())?;
            return Ok(json!({"results": results, "count": results.len()}));
        }
        return Err("query is required".into());
    }

    let results = db::search_with_source_uri(
        conn,
        query.unwrap_or(""),
        namespace,
        tier.as_ref(),
        limit.min(200),
        None,
        None,
        None,
        None,
        agent_id,
        as_agent,
        include_archived,
        source_uri,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"results": results, "count": results.len()}))
}
