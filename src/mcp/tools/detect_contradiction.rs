// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_detect_contradiction` handler.

use crate::llm::OllamaClient;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_detect_contradiction(
    conn: &rusqlite::Connection,
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm =
        llm.ok_or("contradiction detection requires smart or autonomous tier (Ollama LLM)")?;
    let id_a = params["id_a"].as_str().ok_or("id_a is required")?;
    let id_b = params["id_b"].as_str().ok_or("id_b is required")?;
    validate::validate_id(id_a).map_err(|e| e.to_string())?;
    validate::validate_id(id_b).map_err(|e| e.to_string())?;
    let mem_a = db::get(conn, id_a)
        .map_err(|e| e.to_string())?
        .ok_or("memory A not found")?;
    let mem_b = db::get(conn, id_b)
        .map_err(|e| e.to_string())?
        .ok_or("memory B not found")?;
    let contradicts = llm
        .detect_contradiction(&mem_a.content, &mem_b.content)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "contradicts": contradicts,
        "memory_a": {"id": id_a, "title": mem_a.title},
        "memory_b": {"id": id_b, "title": mem_b.title}
    }))
}
