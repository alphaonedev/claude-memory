// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_auto_tag` handler.

use crate::llm::OllamaClient;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_auto_tag(
    conn: &rusqlite::Connection,
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm = llm.ok_or("auto-tagging requires smart or autonomous tier (Ollama LLM)")?;
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let mem = db::get(conn, id)
        .map_err(|e| e.to_string())?
        .ok_or("memory not found")?;
    let tags = llm
        .auto_tag(&mem.title, &mem.content, None)
        .map_err(|e| e.to_string())?;
    // Apply tags to the memory
    let mut all_tags = mem.tags.clone();
    for t in &tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }
    db::update(
        conn,
        id,
        None,
        None,
        None,
        None,
        Some(&all_tags),
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"id": id, "new_tags": tags, "all_tags": all_tags}))
}
