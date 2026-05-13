// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_check_duplicate` handler.

use crate::embeddings::Embed;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_check_duplicate(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&dyn Embed>,
) -> Result<Value, String> {
    let title = params["title"].as_str().ok_or("title is required")?;
    let content = params["content"].as_str().ok_or("content is required")?;
    let namespace = params["namespace"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // Float defaults are awkward in JSON schema land — accept either an
    // explicit threshold or fall back to the tuned default. The hard
    // floor is enforced inside `db::check_duplicate`.
    #[allow(clippy::cast_possible_truncation)]
    let threshold = params["threshold"]
        .as_f64()
        .map_or(db::DUPLICATE_THRESHOLD_DEFAULT, |t| t as f32);

    validate::validate_title(title).map_err(|e| e.to_string())?;
    validate::validate_content(content).map_err(|e| e.to_string())?;
    if let Some(ns) = namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }

    let emb = embedder
        .ok_or("memory_check_duplicate requires the embedder; enable semantic tier or above")?;
    let text = format!("{title} {content}");
    let query_embedding = emb.embed(&text).map_err(|e| e.to_string())?;

    // Round-2 F18 — short-circuit on raw-content hash equality before
    // falling through to embedding cosine similarity. Catches byte-
    // identical duplicates that the embedding pipeline would otherwise
    // cap at ~0.92 due to nomic prefix normalisation.
    let check = db::check_duplicate_with_text(conn, &query_embedding, &text, namespace, threshold)
        .map_err(|e| e.to_string())?;

    // Round similarity to 3 decimals at the response edge — keeps the
    // JSON readable without leaking the f32's full quantisation noise.
    let nearest_json = check.nearest.as_ref().map(|m| {
        json!({
            "id": m.id,
            "title": m.title,
            "namespace": m.namespace,
            "similarity": (m.similarity * 1000.0).round() / 1000.0,
        })
    });
    let suggested_merge = if check.is_duplicate {
        check.nearest.as_ref().map(|m| m.id.clone())
    } else {
        None
    };

    Ok(json!({
        "is_duplicate": check.is_duplicate,
        "threshold": check.threshold,
        "nearest": nearest_json,
        "suggested_merge": suggested_merge,
        "candidates_scanned": check.candidates_scanned,
    }))
}
