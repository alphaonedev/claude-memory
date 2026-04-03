// Copyright (c) 2026 AlphaOne LLC. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root.

//! MCP (Model Context Protocol) server for ai-memory.
//! Exposes memory operations as tools for any MCP-compatible AI client over stdio JSON-RPC.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use crate::config::{AppConfig, FeatureTier, TierConfig};
use crate::db;
use crate::embeddings::Embedder;
use crate::hnsw::VectorIndex;
use crate::llm::OllamaClient;
use crate::models::*;
use crate::reranker::CrossEncoder;
use crate::validate;

// --- JSON-RPC types ---

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

fn ok_response(id: Value, result: Value) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(result),
        error: None,
    }
}

fn err_response(id: Value, code: i64, message: String) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(RpcError {
            code,
            message,
            data: None,
        }),
    }
}

// --- Tool definitions ---

fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "memory_store",
                "description": "Store a new memory. Deduplicates by title+namespace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Short descriptive title"},
                        "content": {"type": "string", "description": "Full memory content"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid"},
                        "namespace": {"type": "string", "description": "Project/topic namespace"},
                        "tags": {"type": "array", "items": {"type": "string"}, "default": []},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0, "default": 1.0},
                        "source": {"type": "string", "enum": ["user", "claude", "hook", "api", "cli", "import", "consolidation", "system"], "default": "claude"}
                    },
                    "required": ["title", "content"]
                }
            },
            {
                "name": "memory_recall",
                "description": "Recall memories relevant to a context. Uses fuzzy OR matching, ranks by relevance + priority + access frequency + tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "context": {"type": "string", "description": "What you're trying to remember"},
                        "namespace": {"type": "string", "description": "Filter by namespace"},
                        "limit": {"type": "integer", "default": 10, "maximum": 50},
                        "tags": {"type": "string", "description": "Filter by tag"},
                        "since": {"type": "string", "description": "Only memories created after this RFC3339 timestamp"},
                        "until": {"type": "string", "description": "Only memories created before this RFC3339 timestamp"}
                    },
                    "required": ["context"]
                }
            },
            {
                "name": "memory_search",
                "description": "Search memories by exact keyword match (AND semantics).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "namespace": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "limit": {"type": "integer", "default": 20, "maximum": 200}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "memory_list",
                "description": "List memories, optionally filtered by namespace or tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "limit": {"type": "integer", "default": 20, "maximum": 200}
                    }
                }
            },
            {
                "name": "memory_delete",
                "description": "Delete a memory by ID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_promote",
                "description": "Promote a memory to long-term (permanent).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_forget",
                "description": "Bulk delete memories matching a pattern, namespace, or tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string"},
                        "pattern": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]}
                    }
                }
            },
            {
                "name": "memory_stats",
                "description": "Get memory store statistics.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "memory_update",
                "description": "Update an existing memory by ID. Only provided fields are changed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to update"},
                        "title": {"type": "string"},
                        "content": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "namespace": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                        "expires_at": {"type": "string", "description": "Expiry timestamp (RFC3339), or null to clear"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_get",
                "description": "Get a specific memory by ID, including its links.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to retrieve"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_link",
                "description": "Create a link between two memories.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID"},
                        "target_id": {"type": "string", "description": "Target memory ID"},
                        "relation": {"type": "string", "enum": ["related_to", "supersedes", "contradicts", "derived_from"], "default": "related_to"}
                    },
                    "required": ["source_id", "target_id"]
                }
            },
            {
                "name": "memory_get_links",
                "description": "Get all links for a memory (both directions).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to get links for"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_consolidate",
                "description": "Consolidate multiple memories into one long-term summary. Deletes source memories and creates derived_from links. If summary is omitted and LLM is available (smart/autonomous tier), auto-generates a summary.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ids": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 100, "description": "Memory IDs to consolidate (minimum 2, maximum 100)"},
                        "title": {"type": "string", "description": "Title for the consolidated memory"},
                        "summary": {"type": "string", "description": "Summary content (optional — auto-generated via LLM if omitted at smart/autonomous tier)"},
                        "namespace": {"type": "string", "default": "global"}
                    },
                    "required": ["ids", "title"]
                }
            },
            {
                "name": "memory_capabilities",
                "description": "Report the active feature tier, loaded models, and available capabilities of the memory system.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "memory_expand_query",
                "description": "Use LLM to expand a search query into additional semantically related terms. Requires smart or autonomous tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The search query to expand"}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "memory_auto_tag",
                "description": "Use LLM to auto-generate tags for a memory. Requires smart or autonomous tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to auto-tag"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_detect_contradiction",
                "description": "Use LLM to check if two memories contradict each other. Requires smart or autonomous tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id_a": {"type": "string", "description": "First memory ID"},
                        "id_b": {"type": "string", "description": "Second memory ID"}
                    },
                    "required": ["id_a", "id_b"]
                }
            }
        ]
    })
}

// --- Tool handlers ---

fn handle_store(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&Embedder>,
    vector_index: Option<&VectorIndex>,
) -> Result<Value, String> {
    let title = params["title"].as_str().ok_or("title is required")?;
    let content = params["content"].as_str().ok_or("content is required")?;
    let tier_str = params["tier"].as_str().unwrap_or("mid");
    let tier = Tier::from_str(tier_str).ok_or(format!("invalid tier: {tier_str}"))?;
    let namespace = params["namespace"].as_str().unwrap_or("global").to_string();
    let source = params["source"].as_str().unwrap_or("claude").to_string();
    let priority = params["priority"].as_i64().unwrap_or(5) as i32;
    let confidence = params["confidence"].as_f64().unwrap_or(1.0);
    let tags: Vec<String> = params["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    validate::validate_title(title).map_err(|e| e.to_string())?;
    validate::validate_content(content).map_err(|e| e.to_string())?;
    validate::validate_namespace(&namespace).map_err(|e| e.to_string())?;
    validate::validate_source(&source).map_err(|e| e.to_string())?;
    validate::validate_tags(&tags).map_err(|e| e.to_string())?;
    validate::validate_priority(priority).map_err(|e| e.to_string())?;
    validate::validate_confidence(confidence).map_err(|e| e.to_string())?;

    let now = chrono::Utc::now();
    let expires_at = tier
        .default_ttl_secs()
        .map(|s| (now + chrono::Duration::seconds(s)).to_rfc3339());

    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace,
        title: title.to_string(),
        content: content.to_string(),
        tags,
        priority: priority.clamp(1, 10),
        confidence: confidence.clamp(0.0, 1.0),
        source,
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
    };

    // True dedup: check for exact title+namespace match (#97)
    let existing = db::find_contradictions(conn, &mem.title, &mem.namespace).unwrap_or_default();
    let exact_dup = existing.iter().find(|c| c.title == mem.title && c.namespace == mem.namespace);
    if let Some(dup) = exact_dup {
        // Update existing memory instead of creating a duplicate
        // update(conn, id, title, content, tier, namespace, tags, priority, confidence, expires_at)
        db::update(
            conn,
            &dup.id,
            None,                   // title (unchanged)
            Some(mem.content.as_str()), // content (update)
            Some(&mem.tier),        // tier
            None,                   // namespace (unchanged)
            Some(&mem.tags),        // tags
            Some(mem.priority),     // priority
            Some(mem.confidence),   // confidence
            None,                   // expires_at
        )
        .map_err(|e| e.to_string())?;
        return Ok(json!({
            "id": dup.id,
            "tier": mem.tier,
            "title": mem.title,
            "namespace": mem.namespace,
            "duplicate": true,
            "action": "updated existing memory"
        }));
    }

    let contradiction_ids: Vec<String> = existing
        .iter()
        .filter(|c| c.id != mem.id)
        .map(|c| c.id.clone())
        .collect();

    let actual_id = db::insert(conn, &mem).map_err(|e| e.to_string())?;

    // Generate and store embedding if embedder is available
    if let Some(emb) = embedder {
        let text = format!("{} {}", mem.title, mem.content);
        match emb.embed(&text) {
            Ok(embedding) => {
                if let Err(e) = db::set_embedding(conn, &actual_id, &embedding) {
                    tracing::warn!("failed to store embedding for {}: {}", &actual_id, e);
                }
                // Add to HNSW index for fast ANN search
                if let Some(idx) = vector_index {
                    idx.insert(actual_id.clone(), embedding);
                }
            }
            Err(e) => {
                tracing::warn!("failed to generate embedding for {}: {}", &actual_id, e);
            }
        }
    }

    let mut response =
        json!({"id": actual_id, "tier": mem.tier, "title": mem.title, "namespace": mem.namespace});
    if !contradiction_ids.is_empty() {
        response["potential_contradictions"] = json!(contradiction_ids);
    }
    Ok(response)
}

fn handle_recall(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&Embedder>,
    vector_index: Option<&VectorIndex>,
    reranker: Option<&CrossEncoder>,
) -> Result<Value, String> {
    let _ = db::gc_if_needed(conn);
    let context = params["context"].as_str().ok_or("context is required")?;
    let namespace = params["namespace"].as_str();
    let limit = params["limit"].as_u64().unwrap_or(10) as usize;
    let tags = params["tags"].as_str();
    let since = params["since"].as_str();
    let until = params["until"].as_str();

    // Helper: serialize scored memories with score field (#95)
    fn scored_memories(results: Vec<(Memory, f64)>) -> Vec<Value> {
        results
            .into_iter()
            .map(|(mem, score)| {
                let mut val = serde_json::to_value(&mem).unwrap_or_default();
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("score".to_string(), json!((score * 1000.0).round() / 1000.0));
                }
                val
            })
            .collect()
    }

    // Use hybrid recall if embedder is available
    if let Some(emb) = embedder {
        match emb.embed(context) {
            Ok(query_emb) => {
                let results = db::recall_hybrid(
                    conn,
                    context,
                    &query_emb,
                    namespace,
                    limit.min(50),
                    tags,
                    since,
                    until,
                    vector_index,
                )
                .map_err(|e| e.to_string())?;

                // Apply cross-encoder reranking if available
                if let Some(ce) = reranker {
                    let reranked = ce.rerank(context, results);
                    let memories = scored_memories(reranked);
                    return Ok(json!({"memories": memories, "count": memories.len(), "mode": "hybrid+rerank"}));
                }

                let memories = scored_memories(results);
                return Ok(json!({"memories": memories, "count": memories.len(), "mode": "hybrid"}));
            }
            Err(e) => {
                tracing::warn!("embedding failed, falling back to FTS: {}", e);
            }
        }
    }

    // Fallback to keyword-only recall
    let results = db::recall(conn, context, namespace, limit.min(50), tags, since, until)
        .map_err(|e| e.to_string())?;
    let memories = scored_memories(results);
    Ok(json!({"memories": memories, "count": memories.len(), "mode": "keyword"}))
}

fn handle_capabilities(tier_config: &TierConfig, reranker: Option<&CrossEncoder>) -> Result<Value, String> {
    let mut caps = tier_config.capabilities();
    // Report actual cross-encoder state, not just config (#93)
    if let Some(ce) = reranker {
        if !ce.is_neural() {
            caps.features.cross_encoder_reranking = false;
            caps.features.memory_reflection = false;
            caps.models.cross_encoder = "lexical-fallback (neural download failed)".to_string();
        }
    }
    serde_json::to_value(caps).map_err(|e| e.to_string())
}

fn handle_expand_query(
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm = llm.ok_or("query expansion requires smart or autonomous tier (Ollama LLM)")?;
    let query = params["query"].as_str().ok_or("query is required")?;
    let terms = llm.expand_query(query).map_err(|e| e.to_string())?;
    Ok(json!({"original": query, "expanded_terms": terms}))
}

fn handle_auto_tag(
    conn: &rusqlite::Connection,
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm = llm.ok_or("auto-tagging requires smart or autonomous tier (Ollama LLM)")?;
    let id = params["id"].as_str().ok_or("id is required")?;
    let mem = db::get(conn, id)
        .map_err(|e| e.to_string())?
        .ok_or("memory not found")?;
    let tags = llm
        .auto_tag(&mem.title, &mem.content)
        .map_err(|e| e.to_string())?;
    // Apply tags to the memory
    let mut all_tags = mem.tags.clone();
    for t in &tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }
    db::update(conn, id, None, None, None, None, Some(&all_tags), None, None, None)
        .map_err(|e| e.to_string())?;
    Ok(json!({"id": id, "new_tags": tags, "all_tags": all_tags}))
}

fn handle_detect_contradiction(
    conn: &rusqlite::Connection,
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm = llm.ok_or("contradiction detection requires smart or autonomous tier (Ollama LLM)")?;
    let id_a = params["id_a"].as_str().ok_or("id_a is required")?;
    let id_b = params["id_b"].as_str().ok_or("id_b is required")?;
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

fn handle_search(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let query = params["query"].as_str().ok_or("query is required")?;
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let limit = params["limit"].as_u64().unwrap_or(20) as usize;

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
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"results": results, "count": results.len()}))
}

fn handle_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let limit = params["limit"].as_u64().unwrap_or(20) as usize;

    let results = db::list(
        conn,
        namespace,
        tier.as_ref(),
        limit.min(200),
        0,
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"memories": results, "count": results.len()}))
}

fn handle_delete(
    conn: &rusqlite::Connection,
    params: &Value,
    vector_index: Option<&VectorIndex>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    let deleted = db::delete(conn, id).map_err(|e| e.to_string())?;
    if deleted {
        if let Some(idx) = vector_index {
            idx.remove(id);
        }
        Ok(json!({"deleted": true}))
    } else {
        Err("memory not found".into())
    }
}

fn handle_promote(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    db::update(
        conn,
        id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE memories SET expires_at = NULL WHERE id = ?1",
        rusqlite::params![id],
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"promoted": true, "id": id, "tier": "long"}))
}

fn handle_forget(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let pattern = params["pattern"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let deleted = db::forget(conn, namespace, pattern, tier.as_ref()).map_err(|e| e.to_string())?;
    Ok(json!({"deleted": deleted}))
}

fn handle_stats(conn: &rusqlite::Connection, db_path: &Path) -> Result<Value, String> {
    let stats = db::stats(conn, db_path).map_err(|e| e.to_string())?;
    serde_json::to_value(stats).map_err(|e| e.to_string())
}

fn handle_update(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    let title = params["title"].as_str();
    let content = params["content"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let namespace = params["namespace"].as_str();
    let tags: Option<Vec<String>> = params["tags"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let priority = params["priority"].as_i64().map(|p| p as i32);
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
        validate::validate_expires_at(Some(ts)).map_err(|e| e.to_string())?;
    }

    let updated = db::update(
        conn,
        id,
        title,
        content,
        tier.as_ref(),
        namespace,
        tags.as_ref(),
        priority,
        confidence,
        expires_at,
    )
    .map_err(|e| e.to_string())?;

    if !updated {
        return Err("memory not found".into());
    }

    let mem = db::get(conn, id).map_err(|e| e.to_string())?;
    Ok(json!({"updated": true, "memory": mem}))
}

fn handle_get(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    match db::get(conn, id).map_err(|e| e.to_string())? {
        Some(mem) => {
            let links = db::get_links(conn, id).unwrap_or_default();
            // Flatten: merge memory fields with links at top level (#96)
            let mut val = serde_json::to_value(&mem).map_err(|e| e.to_string())?;
            if let Some(obj) = val.as_object_mut() {
                obj.insert("links".to_string(), json!(links));
            }
            Ok(val)
        }
        None => Err("memory not found".into()),
    }
}

fn handle_link(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    let target_id = params["target_id"]
        .as_str()
        .ok_or("target_id is required")?;
    let relation = params["relation"].as_str().unwrap_or("related_to");

    validate::validate_link(source_id, target_id, relation).map_err(|e| e.to_string())?;
    db::create_link(conn, source_id, target_id, relation).map_err(|e| e.to_string())?;
    Ok(
        json!({"linked": true, "source_id": source_id, "target_id": target_id, "relation": relation}),
    )
}

fn handle_get_links(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    let links = db::get_links(conn, id).map_err(|e| e.to_string())?;
    Ok(json!({"links": links, "count": links.len()}))
}

fn handle_consolidate(
    conn: &rusqlite::Connection,
    params: &Value,
    llm: Option<&OllamaClient>,
) -> Result<Value, String> {
    let ids_arr = params["ids"]
        .as_array()
        .ok_or("ids is required (array of memory IDs)")?;
    let mut ids = Vec::with_capacity(ids_arr.len());
    for (i, v) in ids_arr.iter().enumerate() {
        match v.as_str() {
            Some(s) => ids.push(s.to_string()),
            None => return Err(format!("ids[{}] must be a string", i)),
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
                Ok(None) => return Err(format!("memory not found: {}", id)),
                Err(e) => return Err(e.to_string()),
            }
        }
        llm_client
            .summarize_memories(&memory_pairs)
            .map_err(|e| format!("LLM summarization failed: {e}"))?
    } else {
        return Err("summary is required (or use smart/autonomous tier for auto-summarization)".into());
    };

    validate::validate_consolidate(&ids, title, &summary, namespace).map_err(|e| e.to_string())?;

    let auto_generated = params["summary"].as_str().is_none();
    let new_id = db::consolidate(
        conn,
        &ids,
        title,
        &summary,
        namespace,
        &Tier::Long,
        "consolidation",
    )
    .map_err(|e| e.to_string())?;

    let mut result = json!({"id": new_id, "consolidated": ids.len()});
    if auto_generated {
        result["auto_summary"] = json!(true);
        result["summary_preview"] = json!(summary.chars().take(200).collect::<String>());
    }
    Ok(result)
}

// --- MCP protocol handler ---

fn handle_request(
    conn: &rusqlite::Connection,
    db_path: &Path,
    req: &RpcRequest,
    embedder: Option<&Embedder>,
    llm: Option<&OllamaClient>,
    reranker: Option<&CrossEncoder>,
    tier_config: &TierConfig,
    vector_index: Option<&VectorIndex>,
) -> RpcResponse {
    let id = req.id.clone().unwrap_or(Value::Null);

    // Validate JSON-RPC 2.0 version
    if req.jsonrpc != "2.0" {
        return err_response(
            id,
            -32600,
            "invalid JSON-RPC version (must be \"2.0\")".into(),
        );
    }

    match req.method.as_str() {
        "initialize" => ok_response(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "ai-memory",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "notifications/initialized" => ok_response(id, json!({})),
        "tools/list" => ok_response(id, tool_definitions()),
        "tools/call" => {
            let tool_name = match req.params["name"].as_str() {
                Some(name) if !name.is_empty() => name,
                _ => return err_response(id, -32602, "missing or empty tool name".into()),
            };
            let empty_obj = json!({});
            let arguments = if req.params["arguments"].is_object() {
                &req.params["arguments"]
            } else {
                &empty_obj
            };

            let result = match tool_name {
                "memory_store" => handle_store(conn, arguments, embedder, vector_index),
                "memory_recall" => handle_recall(conn, arguments, embedder, vector_index, reranker),
                "memory_search" => handle_search(conn, arguments),
                "memory_list" => handle_list(conn, arguments),
                "memory_delete" => handle_delete(conn, arguments, vector_index),
                "memory_promote" => handle_promote(conn, arguments),
                "memory_forget" => handle_forget(conn, arguments),
                "memory_stats" => handle_stats(conn, db_path),
                "memory_update" => handle_update(conn, arguments),
                "memory_get" => handle_get(conn, arguments),
                "memory_link" => handle_link(conn, arguments),
                "memory_get_links" => handle_get_links(conn, arguments),
                "memory_consolidate" => handle_consolidate(conn, arguments, llm),
                "memory_capabilities" => handle_capabilities(tier_config, reranker),
                "memory_expand_query" => handle_expand_query(llm, arguments),
                "memory_auto_tag" => handle_auto_tag(conn, llm, arguments),
                "memory_detect_contradiction" => handle_detect_contradiction(conn, llm, arguments),
                _ => Err(format!("unknown tool: {tool_name}")),
            };

            match result {
                Ok(val) => ok_response(
                    id,
                    json!({
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string_pretty(&val).unwrap_or_default()
                        }]
                    }),
                ),
                Err(e) => ok_response(
                    id,
                    json!({
                        "content": [{"type": "text", "text": e}],
                        "isError": true
                    }),
                ),
            }
        }
        "ping" => ok_response(id, json!({})),
        _ => err_response(id, -32601, format!("method not found: {}", req.method)),
    }
}

/// Run the MCP server over stdio. Blocks until stdin closes.
/// Initializes components based on the requested feature tier.
pub fn run_mcp_server(db_path: &Path, tier: FeatureTier, app_config: &AppConfig) -> anyhow::Result<()> {
    let conn = db::open(db_path)?;
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let tier_config = tier.config();
    eprintln!("ai-memory: requested tier = {}", tier.as_str());

    // --- Initialize LLM (smart tier and above) — before embedder so Ollama
    //     client can be shared with nomic embedder ---
    let llm: Option<Arc<OllamaClient>> = if let Some(ref llm_model) = tier_config.llm_model {
        let model_id = llm_model.ollama_model_id();
        eprintln!("ai-memory: connecting to Ollama for {} ...", llm_model.display_name());
        let ollama_url = app_config.effective_ollama_url();
        match OllamaClient::new_with_url(ollama_url, model_id) {
            Ok(client) => {
                eprintln!("ai-memory: Ollama connected, ensuring model {} is available...", model_id);
                if let Err(e) = client.ensure_model() {
                    eprintln!("ai-memory: model pull failed: {e} (LLM features disabled)");
                    None
                } else {
                    eprintln!("ai-memory: LLM ready ({})", llm_model.display_name());
                    Some(Arc::new(client))
                }
            }
            Err(e) => {
                eprintln!("ai-memory: Ollama not available: {e} (LLM features disabled)");
                None
            }
        }
    } else {
        None
    };

    // --- Initialize embedder (semantic tier and above) ---
    let embedder = if let Some(ref emb_model) = tier_config.embedding_model {
        match Embedder::for_model(*emb_model, llm.clone()) {
            Ok(emb) => {
                eprintln!("ai-memory: embedder loaded ({})", emb.model_description());
                // Backfill embeddings for memories that don't have them
                match db::get_unembedded_ids(&conn) {
                    Ok(unembedded) if !unembedded.is_empty() => {
                        eprintln!("ai-memory: backfilling {} memories...", unembedded.len());
                        let mut ok = 0usize;
                        for (id, title, content) in &unembedded {
                            let text = format!("{} {}", title, content);
                            match emb.embed(&text) {
                                Ok(embedding) => {
                                    if db::set_embedding(&conn, id, &embedding).is_ok() {
                                        ok += 1;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("ai-memory: embed failed for {}: {}", &id[..8.min(id.len())], e);
                                }
                            }
                        }
                        eprintln!("ai-memory: backfilled {}/{}", ok, unembedded.len());
                    }
                    _ => {}
                }
                Some(emb)
            }
            Err(e) => {
                eprintln!("ai-memory: embedder failed: {e}");
                None
            }
        }
    } else {
        None
    };

    // --- Build HNSW vector index (semantic tier and above) ---
    let vector_index = if embedder.is_some() {
        match db::get_all_embeddings(&conn) {
            Ok(entries) if !entries.is_empty() => {
                eprintln!("ai-memory: building HNSW index ({} vectors)...", entries.len());
                let idx = VectorIndex::build(entries);
                eprintln!("ai-memory: HNSW index ready ({} entries)", idx.len());
                Some(idx)
            }
            _ => {
                eprintln!("ai-memory: no embeddings for HNSW index, using linear scan");
                Some(VectorIndex::empty())
            }
        }
    } else {
        None
    };

    // --- Initialize cross-encoder reranker (autonomous tier) ---
    let reranker = if tier_config.cross_encoder {
        eprintln!("ai-memory: loading neural cross-encoder (ms-marco-MiniLM-L-6-v2)...");
        let ce = CrossEncoder::new_neural();
        if ce.is_neural() {
            eprintln!("ai-memory: neural cross-encoder ready");
        } else {
            eprintln!("ai-memory: using lexical cross-encoder fallback");
        }
        Some(ce)
    } else {
        None
    };

    // Report effective tier
    let effective_tier = if llm.is_some() && embedder.is_some() && reranker.is_some() {
        "autonomous"
    } else if llm.is_some() && embedder.is_some() {
        "smart"
    } else if embedder.is_some() {
        "semantic"
    } else {
        "keyword"
    };
    eprintln!("ai-memory MCP server started (stdio, tier={})", effective_tier);

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = err_response(Value::Null, -32700, format!("parse error: {e}"));
                let out = serde_json::to_string(&resp)?;
                writeln!(stdout, "{out}")?;
                stdout.flush()?;
                continue;
            }
        };

        // Notifications have no id — no response expected per JSON-RPC spec
        if req.id.is_none() || req.id == Some(Value::Null) {
            continue;
        }

        let resp = handle_request(
            &conn,
            db_path,
            &req,
            embedder.as_ref(),
            llm.as_deref(),
            reranker.as_ref(),
            &tier_config,
            vector_index.as_ref(),
        );
        let out = serde_json::to_string(&resp)?;
        writeln!(stdout, "{out}")?;
        stdout.flush()?;
    }

    let _ = db::checkpoint(&conn);
    eprintln!("ai-memory MCP server stopped");
    Ok(())
}
