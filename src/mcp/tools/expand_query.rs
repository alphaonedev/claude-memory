// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_expand_query` handler.

use crate::llm::OllamaClient;
use serde_json::{Value, json};
pub(super) fn handle_expand_query(
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm = llm.ok_or("query expansion requires smart or autonomous tier (Ollama LLM)")?;
    let query = params["query"].as_str().ok_or("query is required")?;
    let terms = llm.expand_query(query).map_err(|e| e.to_string())?;
    Ok(json!({"original": query, "expanded_terms": terms}))
}
