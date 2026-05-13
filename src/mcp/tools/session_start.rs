// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_session_start` handler.

use crate::db;
use crate::llm::OllamaClient;
use crate::validate;
use serde_json::{Value, json};
pub(crate) fn handle_session_start(
    conn: &rusqlite::Connection,
    params: &Value,
    llm: Option<&OllamaClient>,
) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    // B4 (R2-LOW) — every MCP entry point that accepts a `namespace`
    // arg must call `validate::validate_namespace` so a payload like
    // `{"namespace": "foo bar"}` is rejected with a typed error
    // instead of silently flowing through to `db::list` (where it
    // may interact with FTS5 escape semantics or downstream filters
    // in surprising ways). Skip when omitted — the handler defaults
    // to "all namespaces" in that case.
    if let Some(ns) = namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(10)).unwrap_or(usize::MAX);

    let results = db::list(
        conn,
        namespace,
        None,
        limit.min(50),
        0,
        None,
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;

    let memories: Vec<Value> = results
        .iter()
        .map(|mem| {
            let mut val = serde_json::to_value(mem).unwrap_or_default();
            if let Some(obj) = val.as_object_mut() {
                obj.insert("score".to_string(), json!(0.0));
            }
            val
        })
        .collect();

    let mut response = json!({
        "memories": memories,
        "count": memories.len(),
        "mode": "session_start",
    });

    if let Some(llm_client) = llm
        && !results.is_empty()
    {
        let pairs: Vec<(String, String)> = results
            .iter()
            .map(|m| (m.title.clone(), m.content.clone()))
            .collect();
        match llm_client.summarize_memories(&pairs) {
            Ok(summary) => {
                response["summary"] = json!(summary);
            }
            Err(e) => {
                tracing::warn!("session_start LLM summary failed: {}", e);
            }
        }
    }

    // Auto-register parent chain from filesystem path — disabled by default
    // to prevent filesystem structure leakage into the memory database.
    // Uncomment or gate behind a config flag if desired.

    // Auto-prepend namespace standard (after LLM summary, separate field)
    super::inject_namespace_standard(conn, namespace, &mut response);

    Ok(response)
}
