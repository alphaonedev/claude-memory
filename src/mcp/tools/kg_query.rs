// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_kg_query` handler.

use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_kg_query(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    // v0.7.0 Provenance Gap 6 (#889) — reciprocal "subgraph rooted at
    // every memory sharing source_uri" entrypoint. When
    // `by_source_uri` is supplied, every memory carrying that URI is
    // returned alongside its outbound links so callers see the full
    // forest rooted at the document. The traversal is unbounded (one
    // hop, since the goal is "what else is from this document") and
    // bypasses the `source_id`-required argument check.
    let by_source_uri = params["by_source_uri"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(uri) = by_source_uri {
        validate::validate_source_uri(uri).map_err(|e| e.to_string())?;
        let namespace = params["namespace"].as_str();
        let limit = params["limit"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());
        let roots =
            db::list_by_source_uri(conn, uri, namespace, limit).map_err(|e| e.to_string())?;
        let memories_json: Vec<Value> = roots
            .iter()
            .map(|m| {
                json!({
                    "target_id": m.id,
                    "title": m.title,
                    "target_namespace": m.namespace,
                    "depth": 0,
                    "source_uri": m.source_uri,
                })
            })
            .collect();
        return Ok(json!({
            "by_source_uri": uri,
            "memories": memories_json,
            "count": roots.len(),
        }));
    }

    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    validate::validate_id(source_id).map_err(|e| e.to_string())?;

    let max_depth = params["max_depth"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(1);

    let valid_at = params["valid_at"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(t) = valid_at {
        validate::validate_expires_at_format(t).map_err(|e| e.to_string())?;
    }

    let allowed_agents: Option<Vec<String>> = params["allowed_agents"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_string)
            .collect()
    });
    if let Some(agents) = allowed_agents.as_ref() {
        for a in agents {
            validate::validate_agent_id(a).map_err(|e| e.to_string())?;
        }
    }

    let limit = params["limit"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());

    // NHI-P3-T7 (v0.7.0 NHI testing): default to "current view" —
    // exclude edges whose `valid_until` lies in the past. Pass
    // `include_invalidated=true` to traverse the full historical graph.
    let include_invalidated = params["include_invalidated"].as_bool().unwrap_or(false);

    let nodes = db::kg_query(
        conn,
        source_id,
        max_depth,
        valid_at,
        allowed_agents.as_deref(),
        limit,
        include_invalidated,
    )
    .map_err(|e| e.to_string())?;

    let memories_json: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "target_id": n.target_id,
                "relation": n.relation,
                "valid_from": n.valid_from,
                "valid_until": n.valid_until,
                "observed_by": n.observed_by,
                "title": n.title,
                "target_namespace": n.target_namespace,
                "depth": n.depth,
                "path": n.path,
            })
        })
        .collect();
    let paths_json: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();

    Ok(json!({
        "source_id": source_id,
        "max_depth": max_depth,
        "memories": memories_json,
        "paths": paths_json,
        "count": nodes.len(),
    }))
}
