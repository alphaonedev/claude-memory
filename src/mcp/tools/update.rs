// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_update` handler.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_update(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&dyn Embed>,
    vector_index: Option<&VectorIndex>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    // Resolve prefix if exact ID not found
    let resolved_id = if db::get(conn, id).map_err(|e| e.to_string())?.is_some() {
        id.to_string()
    } else if let Some(mem) = db::get_by_prefix(conn, id).map_err(|e| e.to_string())? {
        mem.id
    } else {
        return Err("memory not found".into());
    };
    let title = params["title"].as_str();
    let content = params["content"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let namespace = params["namespace"].as_str();
    let tags: Option<Vec<String>> = params["tags"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    // B4 (R2-LOW) — clamp instead of panic. Validation below enforces 1-10.
    let priority = params["priority"]
        .as_i64()
        .map(|p| i32::try_from(p).unwrap_or(i32::MAX));
    let confidence = params["confidence"].as_f64();
    let expires_at = params["expires_at"].as_str();

    if let Some(t) = title {
        validate::validate_title(t).map_err(|e| e.to_string())?;
    }
    if let Some(c) = content {
        validate::validate_content(c).map_err(|e| e.to_string())?;
    }
    if let Some(ns) = &namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }
    if let Some(ref t) = tags {
        validate::validate_tags(t).map_err(|e| e.to_string())?;
    }
    if let Some(p) = priority {
        validate::validate_priority(p).map_err(|e| e.to_string())?;
    }
    if let Some(c) = confidence {
        validate::validate_confidence(c).map_err(|e| e.to_string())?;
    }
    if let Some(ts) = expires_at {
        // Allow past dates in update for programmatic TTL management and GC testing
        validate::validate_expires_at_format(ts).map_err(|e| e.to_string())?;
    }

    let metadata = if params["metadata"].is_object() {
        let m = params["metadata"].clone();
        validate::validate_metadata(&m).map_err(|e| e.to_string())?;
        // Preserve existing metadata.agent_id — provenance is immutable.
        // Without this, any MCP caller could rewrite the author of any memory.
        let existing = db::get(conn, &resolved_id)
            .map_err(|e| e.to_string())?
            .map_or_else(|| serde_json::json!({}), |m| m.metadata);
        Some(crate::identity::preserve_agent_id(&existing, &m))
    } else {
        None
    };

    let (found, content_changed) = db::update(
        conn,
        &resolved_id,
        title,
        content,
        tier.as_ref(),
        namespace,
        tags.as_ref(),
        priority,
        confidence,
        expires_at,
        metadata.as_ref(),
    )
    .map_err(|e| e.to_string())?;

    if !found {
        return Err("memory not found".into());
    }

    // Regenerate embedding when title or content changed
    if content_changed && let Some(emb) = embedder {
        let mem = db::get(conn, &resolved_id).map_err(|e| e.to_string())?;
        if let Some(ref m) = mem {
            let text = format!("{} {}", m.title, m.content);
            if let Ok(embedding) = emb.embed(&text) {
                let _ = db::set_embedding(conn, &resolved_id, &embedding);
                if let Some(idx) = vector_index {
                    idx.remove(&resolved_id);
                    idx.insert(resolved_id.clone(), embedding);
                }
            }
        }
    }

    let mem = db::get(conn, &resolved_id).map_err(|e| e.to_string())?;
    Ok(json!({"updated": true, "memory": mem}))
}
