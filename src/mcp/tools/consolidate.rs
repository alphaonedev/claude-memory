// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_consolidate` handler.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::llm::OllamaClient;
use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;
pub(super) fn handle_consolidate(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    llm: Option<&OllamaClient>,
    embedder: Option<&dyn Embed>,
    vector_index: Option<&VectorIndex>,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let ids_arr = params["ids"]
        .as_array()
        .ok_or("ids is required (array of memory IDs)")?;
    let mut ids = Vec::with_capacity(ids_arr.len());
    for (i, v) in ids_arr.iter().enumerate() {
        match v.as_str() {
            Some(s) => {
                validate::validate_id(s).map_err(|e| e.to_string())?;
                ids.push(s.to_string());
            }
            None => return Err(format!("ids[{i}] must be a string")),
        }
    }
    let title = params["title"].as_str().ok_or("title is required")?;
    let namespace = params["namespace"].as_str().unwrap_or("global");

    // Auto-generate summary via LLM if not provided
    let summary: String = if let Some(s) = params["summary"].as_str() {
        s.to_string()
    } else if let Some(llm_client) = llm {
        // Fetch memory contents for LLM summarization
        let mut memory_pairs: Vec<(String, String)> = Vec::new();
        for id in &ids {
            match db::get(conn, id) {
                Ok(Some(mem)) => memory_pairs.push((mem.title, mem.content)),
                Ok(None) => return Err(format!("memory not found: {id}")),
                Err(e) => return Err(e.to_string()),
            }
        }
        llm_client
            .summarize_memories(&memory_pairs)
            .map_err(|e| format!("LLM summarization failed: {e}"))?
    } else {
        return Err(
            "summary is required (or use smart/autonomous tier for auto-summarization)".into(),
        );
    };

    validate::validate_consolidate(&ids, title, &summary, namespace).map_err(|e| e.to_string())?;

    // v0.7.0 K9 — unified permission pipeline (consolidate-side).
    {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
            .map_err(|e| e.to_string())?;
        let ctx = PermissionContext {
            op: Op::MemoryConsolidate,
            namespace: namespace.to_string(),
            agent_id,
            payload: json!({
                "title": title,
                "summary_chars": summary.len(),
                "source_ids": ids,
            }),
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("consolidate denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "consolidate",
                    "namespace": namespace,
                    "source_count": ids.len(),
                }));
            }
        }
    }

    let auto_generated = params["summary"].as_str().is_none();

    // Remove old entries from HNSW index before consolidation deletes them
    if let Some(idx) = vector_index {
        for id in &ids {
            idx.remove(id);
        }
    }

    // NHI: the caller (consolidator) owns the new memory's agent_id;
    // source authors are preserved as a forensic array by db::consolidate.
    let explicit_agent_id = params["agent_id"].as_str();
    let consolidator_agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;
    let new_id = db::consolidate(
        conn,
        &ids,
        title,
        &summary,
        namespace,
        &Tier::Long,
        "consolidation",
        &consolidator_agent_id,
    )
    .map_err(|e| e.to_string())?;

    // Generate embedding for the consolidated memory (#52)
    if let Some(emb) = embedder {
        let text = format!("{title} {summary}");
        match emb.embed(&text) {
            Ok(embedding) => {
                if let Err(e) = db::set_embedding(conn, &new_id, &embedding) {
                    tracing::warn!(
                        "failed to store embedding for consolidated {}: {}",
                        &new_id,
                        e
                    );
                }
                if let Some(idx) = vector_index {
                    // Remove old embeddings from HNSW index
                    for id in &ids {
                        idx.remove(id);
                    }
                    idx.insert(new_id.clone(), embedding);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to generate embedding for consolidated {}: {}",
                    &new_id,
                    e
                );
            }
        }
    }

    let mut result = json!({"id": new_id, "consolidated": ids.len()});
    if auto_generated {
        result["auto_summary"] = json!(true);
        result["summary_preview"] = json!(summary.chars().take(200).collect::<String>());
    }
    // Warn if any source memory was a namespace standard
    let standard_ids: Vec<&str> = ids
        .iter()
        .filter(|id| db::is_namespace_standard(conn, id))
        .map(std::string::String::as_str)
        .collect();
    if !standard_ids.is_empty() {
        result["warning"] = json!(format!(
            "consolidated memories included namespace standard(s): {}. Re-set the standard to the new memory ID: {}",
            standard_ids.join(", "),
            new_id
        ));
    }

    // P5 (G9): fire `memory_consolidated` webhook AFTER db::consolidate
    // commits the new memory. memory_id = the new consolidated id; the
    // details block carries the source ids that were merged.
    let details = serde_json::to_value(crate::subscriptions::ConsolidatedEventDetails {
        source_ids: ids.clone(),
        source_count: ids.len(),
    })
    .ok();
    crate::subscriptions::dispatch_event_with_details(
        conn,
        "memory_consolidated",
        &new_id,
        namespace,
        Some(&consolidator_agent_id),
        db_path,
        details,
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Namespace standard handlers
// ---------------------------------------------------------------------------
