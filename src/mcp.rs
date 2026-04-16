// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP (Model Context Protocol) server for ai-memory.
//! Exposes memory operations as tools for any MCP-compatible AI client over stdio JSON-RPC.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use crate::config::{AppConfig, FeatureTier, TierConfig};
use crate::db;
use crate::embeddings::Embedder;
use crate::hnsw::VectorIndex;
use crate::llm::OllamaClient;
use crate::models::{Memory, Tier};
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

#[allow(clippy::too_many_lines)]
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
                        "source": {"type": "string", "enum": ["user", "claude", "hook", "api", "cli", "import", "consolidation", "system"], "default": "claude"},
                        "metadata": {"type": "object", "description": "Arbitrary JSON metadata", "default": {}},
                        "agent_id": {"type": "string", "description": "Agent identifier. If omitted, the server synthesizes an NHI-hardened default (ai:<client>@<host>:pid-<pid>, host:<host>:pid-<pid>-<uuid8>, or anonymous:pid-<pid>-<uuid8>)."}
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
                        "until": {"type": "string", "description": "Only memories created before this RFC3339 timestamp"},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. Default 'toon_compact' saves 79% tokens vs JSON. 'toon' includes timestamps. 'json' for structured parsing."}
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
                        "limit": {"type": "integer", "default": 20, "maximum": 200},
                        "agent_id": {"type": "string", "description": "Filter by metadata.agent_id (exact match)."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. Default 'toon_compact' saves 79% tokens. 'json' for structured parsing."}
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
                        "limit": {"type": "integer", "default": 20, "maximum": 200},
                        "agent_id": {"type": "string", "description": "Filter by metadata.agent_id (exact match)."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. Default 'toon_compact' saves 79% tokens. 'json' for structured parsing."}
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
                "description": "Bulk delete memories matching a pattern, namespace, or tier. Archives before deletion. Use dry_run to preview.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string"},
                        "pattern": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "dry_run": {"type": "boolean", "default": false, "description": "If true, report what would be deleted without deleting"}
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
                        "expires_at": {"type": "string", "description": "Expiry timestamp (RFC3339), or null to clear"},
                        "metadata": {"type": "object", "description": "Arbitrary JSON metadata"}
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
            },
            {
                "name": "memory_archive_list",
                "description": "List archived (expired) memories. Archived memories are preserved before GC deletion.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Filter by namespace"},
                        "limit": {"type": "integer", "description": "Max results (default 50, max 1000)"},
                        "offset": {"type": "integer", "description": "Pagination offset"}
                    }
                }
            },
            {
                "name": "memory_archive_restore",
                "description": "Restore an archived memory back to the active memory store (expires_at cleared).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "ID of the archived memory to restore"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_archive_purge",
                "description": "Permanently delete archived memories. Optionally only those older than N days.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "older_than_days": {"type": "integer", "description": "Only purge entries archived more than N days ago. Omit to purge all."}
                    }
                }
            },
            {
                "name": "memory_archive_stats",
                "description": "Show archive statistics: total count and breakdown by namespace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_gc",
                "description": "Trigger garbage collection on expired memories. Archives them before deletion. Supports dry_run mode.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "dry_run": {"type": "boolean", "default": false, "description": "If true, report what would be collected without deleting"}
                    }
                }
            },
            {
                "name": "memory_session_start",
                "description": "Auto-recall recent memories on session start. Returns the most recently accessed/updated memories. If LLM is available (smart/autonomous tier), returns an AI-generated summary.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Optional namespace to scope recall"},
                        "limit": {"type": "integer", "default": 10, "maximum": 50},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact"}
                    }
                }
            },
            {
                "name": "memory_namespace_set_standard",
                "description": "Set a memory as the standard/policy for a namespace. Auto-prepended to recall and session_start. Supports rule layering: set a parent namespace to inherit its standard too (global '*' + parent + namespace).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to set the standard for"},
                        "id": {"type": "string", "description": "Memory ID to use as the standard"},
                        "parent": {"type": "string", "description": "Optional parent namespace to inherit standards from (rule layering)"}
                    },
                    "required": ["namespace", "id"]
                }
            },
            {
                "name": "memory_namespace_get_standard",
                "description": "Get the standard/policy memory for a namespace, if one is set.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to get the standard for"}
                    },
                    "required": ["namespace"]
                }
            },
            {
                "name": "memory_namespace_clear_standard",
                "description": "Clear the standard/policy for a namespace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to clear the standard for"}
                    },
                    "required": ["namespace"]
                }
            }
        ]
    })
}

// --- MCP Prompts ---

/// Return the list of available prompts.
fn prompt_definitions() -> Value {
    json!({
        "prompts": [
            {
                "name": "recall-first",
                "description": "System prompt for AI clients: proactive memory recall, TOON format, tier strategy.",
                "arguments": [
                    {
                        "name": "namespace",
                        "description": "Optional namespace to scope recall (e.g. project name)",
                        "required": false
                    }
                ]
            },
            {
                "name": "memory-workflow",
                "description": "Quick reference card for memory tool usage patterns."
            }
        ]
    })
}

/// Return the content of a specific prompt.
fn prompt_content(name: &str, params: &Value) -> Result<Value, String> {
    match name {
        "recall-first" => {
            let ns_hint = params
                .get("arguments")
                .and_then(|a| a.get("namespace"))
                .and_then(|v| v.as_str())
                .map(|ns| format!(" Scope recall to namespace \"{ns}\" when relevant."))
                .unwrap_or_default();

            Ok(json!({
                "messages": [{
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": format!(
            "You have access to a persistent memory system (ai-memory). Follow these rules:\n\
            1. RECALL FIRST: At conversation start, call memory_recall with the user's apparent topic. Before answering any question about prior work, recall first.\n\
            2. STORE LEARNINGS: When the user corrects you or teaches something, call memory_store with tier:long, priority:9.\n\
            3. TOON FORMAT: All recall/list/search responses default to TOON compact (79% smaller than JSON). Pass format:\"json\" only if you need structured parsing.\n\
            4. TIERS: short=6h ephemeral, mid=7d working knowledge, long=permanent. Mid auto-promotes to long at 5 accesses.\n\
            5. DEDUP: Storing with an existing title+namespace updates the existing memory, not a duplicate.\n\
            6. NAMESPACES: Organize by project/topic. Always pass namespace when storing and recalling.\n\
            7. CAPABILITIES: Call memory_capabilities once per session to discover available features (tier-dependent).\n\
            8. TAGS: Use tags for cross-cutting concerns. memory_auto_tag can generate them if available.{ns_hint}")
                    }
                }]
            }))
        }
        "memory-workflow" => Ok(json!({
            "messages": [{
                "role": "user",
                "content": {
                    "type": "text",
                    "text": "\
        STORE: memory_store(title, content, tier, namespace, tags, priority) — dedup by title+ns\n\
        RECALL: memory_recall(context, namespace) → ranked results (TOON compact default)\n\
        SEARCH: memory_search(query, namespace) → exact AND match (TOON compact default)\n\
        LIST: memory_list(namespace, tier) → browse with filters (TOON compact default)\n\
        GET: memory_get(id) → single memory with links\n\
        PROMOTE: memory_promote(id) — mid→long, clears expiry\n\
        CONSOLIDATE: memory_consolidate(ids, title) — merge N→1, LLM summary if available\n\
        LINK: memory_link(source_id, target_id, relation) — related_to|supersedes|contradicts|derived_from\n\
        TAG: memory_auto_tag(id) — LLM generates tags (smart+ tier)\n\
        EXPAND: memory_expand_query(query) — LLM broadens search terms (smart+ tier)\n\
        CONTRADICT: memory_detect_contradiction(id_a, id_b) — LLM checks conflict (smart+ tier)"
                }
            }]
        })),
        _ => Err(format!("unknown prompt: {name}")),
    }
}

// --- Tool handlers ---

#[allow(clippy::too_many_lines)]
fn handle_store(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&Embedder>,
    vector_index: Option<&VectorIndex>,
    resolved_ttl: &crate::config::ResolvedTtl,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let title = params["title"].as_str().ok_or("title is required")?;
    let content = params["content"].as_str().ok_or("content is required")?;
    let tier_str = params["tier"].as_str().unwrap_or("mid");
    let tier = Tier::from_str(tier_str).ok_or(format!("invalid tier: {tier_str}"))?;
    let namespace = params["namespace"].as_str().unwrap_or("global").to_string();
    let source = params["source"].as_str().unwrap_or("claude").to_string();
    let priority = i32::try_from(params["priority"].as_i64().unwrap_or(5)).expect("i64 as i32");
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

    let mut metadata = if params["metadata"].is_object() {
        params["metadata"].clone()
    } else {
        serde_json::json!({})
    };
    // Resolve agent_id via the NHI-hardened precedence chain and merge into
    // metadata. Explicit values win in this order:
    //   1. top-level `agent_id` param
    //   2. embedded `metadata.agent_id` (backward compatible with callers
    //      that supply it inline)
    //   3. env / MCP clientInfo / host / anonymous (handled inside `identity`)
    let explicit_agent_id = params["agent_id"]
        .as_str()
        .or_else(|| metadata.get("agent_id").and_then(serde_json::Value::as_str));
    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(agent_id.clone()),
        );
    }
    validate::validate_metadata(&metadata).map_err(|e| e.to_string())?;

    let now = chrono::Utc::now();
    let expires_at = resolved_ttl
        .ttl_for_tier(&tier)
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
        metadata,
    };

    // True dedup: check for exact title+namespace match (#97)
    let existing = db::find_contradictions(conn, &mem.title, &mem.namespace).unwrap_or_default();
    let exact_dup = existing
        .iter()
        .find(|c| c.title == mem.title && c.namespace == mem.namespace);
    if let Some(dup) = exact_dup {
        // Update existing memory instead of creating a duplicate.
        // Preserve the original agent_id (provenance is immutable) — the
        // existing memory's metadata.agent_id wins over anything in the
        // incoming store.
        let preserved_metadata = crate::identity::preserve_agent_id(&dup.metadata, &mem.metadata);
        let (_found, content_changed) = db::update(
            conn,
            &dup.id,
            None,                       // title (unchanged)
            Some(mem.content.as_str()), // content (update)
            Some(&mem.tier),            // tier
            None,                       // namespace (unchanged)
            Some(&mem.tags),            // tags
            Some(mem.priority),         // priority
            Some(mem.confidence),       // confidence
            None,                       // expires_at
            Some(&preserved_metadata),  // metadata (agent_id preserved)
        )
        .map_err(|e| e.to_string())?;
        // Regenerate embedding if content changed during dedup update
        if content_changed && let Some(emb) = embedder {
            let text = format!("{} {}", mem.title, mem.content);
            if let Ok(embedding) = emb.embed(&text) {
                let _ = db::set_embedding(conn, &dup.id, &embedding);
                if let Some(idx) = vector_index {
                    idx.remove(&dup.id);
                    idx.insert(dup.id.clone(), embedding);
                }
            }
        }
        return Ok(json!({
            "id": dup.id,
            "tier": mem.tier,
            "title": mem.title,
            "namespace": mem.namespace,
            "duplicate": true,
            "action": "updated existing memory"
        }));
    }

    let actual_id = db::insert(conn, &mem).map_err(|e| e.to_string())?;

    // Exclude self-ID from contradictions (both proposed and actual, since upsert may reuse existing ID)
    let contradiction_ids: Vec<String> = existing
        .iter()
        .filter(|c| c.id != mem.id && c.id != actual_id)
        .map(|c| c.id.clone())
        .collect();

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

/// Inject namespace standard into a `recall/session_start` response.
/// Inject namespace standards into a `recall/session_start` response.
/// Three-level rule layering: global ("*") + parent chain + namespace-specific.
/// Max depth 5 to prevent cycles.
fn inject_namespace_standard(
    conn: &rusqlite::Connection,
    namespace: Option<&str>,
    response: &mut Value,
) {
    let mut standards: Vec<Value> = Vec::new();
    let mut standard_ids: Vec<String> = Vec::new();

    // Helper: add a standard if not already present (dedup by memory ID)
    let add_standard = |std: Value, ids: &mut Vec<String>, stds: &mut Vec<Value>| {
        let id = std["id"].as_str().unwrap_or_default().to_string();
        if !ids.contains(&id) {
            ids.push(id);
            stds.push(std);
        }
    };

    // Level 1: global standard ("*") — always applies
    if let Some(global) = lookup_namespace_standard(conn, "*") {
        add_standard(global, &mut standard_ids, &mut standards);
    }

    // Level 2+: walk parent chain from namespace, then add namespace itself
    if let Some(ns) = namespace
        && ns != "*"
    {
        // Collect the parent chain (bottom-up), then reverse to get top-down order
        let mut chain: Vec<String> = Vec::new();
        let mut current = ns.to_string();
        for _ in 0..5 {
            // max depth 5
            if let Some(parent) = db::get_namespace_parent(conn, &current) {
                if parent == "*" || chain.contains(&parent) {
                    break; // don't re-add global or cycle
                }
                chain.push(parent.clone());
                current = parent;
            } else {
                break;
            }
        }
        // Add parents top-down (grandparent first, then parent)
        for ancestor in chain.into_iter().rev() {
            if let Some(std) = lookup_namespace_standard(conn, &ancestor) {
                add_standard(std, &mut standard_ids, &mut standards);
            }
        }
        // Add the namespace's own standard last (most specific)
        if let Some(ns_std) = lookup_namespace_standard(conn, ns) {
            add_standard(ns_std, &mut standard_ids, &mut standards);
        }
    }

    if standards.is_empty() {
        return;
    }

    // Deduplicate: remove standard memories from results array
    if let Some(memories) = response["memories"].as_array_mut() {
        memories.retain(|m| {
            let mid = m["id"].as_str().unwrap_or_default();
            !standard_ids.iter().any(|sid| sid == mid)
        });
        response["count"] = json!(memories.len());
    }

    // Return as single object if one standard, array if multiple
    if standards.len() == 1 {
        response["standard"] = standards.into_iter().next().unwrap();
    } else {
        response["standards"] = json!(standards);
    }
}

fn handle_recall(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&Embedder>,
    vector_index: Option<&VectorIndex>,
    reranker: Option<&CrossEncoder>,
    archive_on_gc: bool,
    resolved_ttl: &crate::config::ResolvedTtl,
) -> Result<Value, String> {
    // Helper: serialize scored memories with score field (#95)
    fn scored_memories(results: Vec<(Memory, f64)>) -> Vec<Value> {
        results
            .into_iter()
            .map(|(mem, score)| {
                let mut val = serde_json::to_value(&mem).unwrap_or_default();
                if let Some(obj) = val.as_object_mut() {
                    obj.insert(
                        "score".to_string(),
                        json!((score * 1000.0).round() / 1000.0),
                    );
                }
                val
            })
            .collect()
    }

    let _ = db::gc_if_needed(conn, archive_on_gc);
    let context = params["context"].as_str().ok_or("context is required")?;
    let namespace = params["namespace"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(10)).expect("u64 as usize");
    let tags = params["tags"].as_str();
    let since = params["since"].as_str();
    let until = params["until"].as_str();

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
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                )
                .map_err(|e| e.to_string())?;

                // Apply cross-encoder reranking if available
                if let Some(ce) = reranker {
                    let ce_reranked = ce.rerank(context, results);
                    let memories = scored_memories(ce_reranked);
                    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "hybrid+rerank"});
                    inject_namespace_standard(conn, namespace, &mut resp);
                    return Ok(resp);
                }

                let memories = scored_memories(results);
                let mut resp =
                    json!({"memories": memories, "count": memories.len(), "mode": "hybrid"});
                inject_namespace_standard(conn, namespace, &mut resp);
                return Ok(resp);
            }
            Err(e) => {
                tracing::warn!("embedding failed, falling back to FTS: {}", e);
            }
        }
    }

    // Fallback to keyword-only recall
    let results = db::recall(
        conn,
        context,
        namespace,
        limit.min(50),
        tags,
        since,
        until,
        resolved_ttl.short_extend_secs,
        resolved_ttl.mid_extend_secs,
    )
    .map_err(|e| e.to_string())?;
    let memories = scored_memories(results);
    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "keyword"});
    inject_namespace_standard(conn, namespace, &mut resp);
    Ok(resp)
}

fn handle_capabilities(
    tier_config: &TierConfig,
    reranker: Option<&CrossEncoder>,
) -> Result<Value, String> {
    let mut caps = tier_config.capabilities();
    // Report actual cross-encoder state, not just config (#93)
    if let Some(ce) = reranker
        && !ce.is_neural()
    {
        caps.features.cross_encoder_reranking = false;
        caps.features.memory_reflection = false;
        caps.models.cross_encoder = "lexical-fallback (neural download failed)".to_string();
    }
    serde_json::to_value(caps).map_err(|e| e.to_string())
}

fn handle_expand_query(llm: Option<&OllamaClient>, params: &Value) -> Result<Value, String> {
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
    validate::validate_id(id).map_err(|e| e.to_string())?;
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

fn handle_detect_contradiction(
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

fn handle_search(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let query = params["query"].as_str().ok_or("query is required")?;
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(20)).expect("u64 as usize");

    let agent_id = params["agent_id"].as_str();
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
        agent_id,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"results": results, "count": results.len()}))
}

fn handle_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(20)).expect("u64 as usize");
    let agent_id = params["agent_id"].as_str();

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
        agent_id,
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
    validate::validate_id(id).map_err(|e| e.to_string())?;
    // Try exact delete first; fall back to prefix resolution
    let deleted = db::delete(conn, id).map_err(|e| e.to_string())?;
    if deleted {
        if let Some(idx) = vector_index {
            idx.remove(id);
        }
        Ok(json!({"deleted": true}))
    } else if let Some(mem) = db::get_by_prefix(conn, id).map_err(|e| e.to_string())? {
        let full_id = mem.id.clone();
        let deleted = db::delete(conn, &full_id).map_err(|e| e.to_string())?;
        if deleted {
            if let Some(idx) = vector_index {
                idx.remove(&full_id);
            }
            Ok(json!({"deleted": true}))
        } else {
            Err("memory not found".into())
        }
    } else {
        Err("memory not found".into())
    }
}

fn handle_promote(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
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
    let (found, _) = db::update(
        conn,
        &resolved_id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        Some(""), // empty string clears expires_at
        None,
    )
    .map_err(|e| e.to_string())?;
    if !found {
        return Err("memory not found".into());
    }
    Ok(json!({"promoted": true, "id": resolved_id, "tier": "long"}))
}

fn handle_forget(
    conn: &rusqlite::Connection,
    params: &Value,
    archive: bool,
) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let pattern = params["pattern"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let dry_run = params["dry_run"].as_bool().unwrap_or(false);

    if dry_run {
        let count =
            db::forget_count(conn, namespace, pattern, tier.as_ref()).map_err(|e| e.to_string())?;
        return Ok(json!({"would_delete": count, "dry_run": true}));
    }

    let deleted =
        db::forget(conn, namespace, pattern, tier.as_ref(), archive).map_err(|e| e.to_string())?;
    Ok(json!({"deleted": deleted, "archived": archive}))
}

fn handle_stats(conn: &rusqlite::Connection, db_path: &Path) -> Result<Value, String> {
    let stats = db::stats(conn, db_path).map_err(|e| e.to_string())?;
    serde_json::to_value(stats).map_err(|e| e.to_string())
}

fn handle_update(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&Embedder>,
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
    let priority = params["priority"]
        .as_i64()
        .map(|p| i32::try_from(p).expect("i64 as i32"));
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

fn handle_get(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    match db::resolve_id(conn, id).map_err(|e| e.to_string())? {
        Some(mem) => {
            let links = db::get_links(conn, &mem.id).unwrap_or_default();
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
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let links = db::get_links(conn, id).map_err(|e| e.to_string())?;
    Ok(json!({"links": links, "count": links.len()}))
}

fn handle_consolidate(
    conn: &rusqlite::Connection,
    params: &Value,
    llm: Option<&OllamaClient>,
    embedder: Option<&Embedder>,
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
    Ok(result)
}

// ---------------------------------------------------------------------------
// Namespace standard handlers
// ---------------------------------------------------------------------------

fn handle_namespace_set_standard(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let parent = params["parent"].as_str();
    if let Some(p) = parent {
        validate::validate_namespace(p).map_err(|e| e.to_string())?;
    }
    db::set_namespace_standard(conn, namespace, id, parent).map_err(|e| e.to_string())?;
    let mut resp = json!({"set": true, "namespace": namespace, "standard_id": id});
    if let Some(p) = parent {
        resp["parent"] = json!(p);
    }
    Ok(resp)
}

fn handle_namespace_get_standard(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;
    let standard_id = db::get_namespace_standard(conn, namespace).map_err(|e| e.to_string())?;
    match standard_id {
        Some(id) => {
            let mem = db::get(conn, &id).map_err(|e| e.to_string())?;
            match mem {
                Some(m) => Ok(json!({
                    "namespace": namespace,
                    "standard_id": id,
                    "title": m.title,
                    "content": m.content,
                    "priority": m.priority
                })),
                None => Ok(
                    json!({"namespace": namespace, "standard_id": id, "warning": "standard memory not found — may have been deleted"}),
                ),
            }
        }
        None => Ok(json!({"namespace": namespace, "standard_id": null})),
    }
}

fn handle_namespace_clear_standard(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;
    let cleared = db::clear_namespace_standard(conn, namespace).map_err(|e| e.to_string())?;
    Ok(json!({"cleared": cleared, "namespace": namespace}))
}

/// Look up the namespace standard and return it as a serialized Memory, or None.
fn lookup_namespace_standard(conn: &rusqlite::Connection, namespace: &str) -> Option<Value> {
    let standard_id = db::get_namespace_standard(conn, namespace).ok()??;
    let mem = db::get(conn, &standard_id).ok()??;
    serde_json::to_value(&mem).ok()
}

/// Auto-register namespace parent chain from the filesystem path.
/// Walks from cwd up to home dir, checks if each directory name has a namespace
/// standard set, and registers the parent chain.
///
/// Example: cwd = /home/user/monorepo/frontend
///   → checks "frontend" (cwd), "monorepo" (parent), stops at home dir
///   → if "monorepo" has a standard, sets `parent_namespace` of "frontend" to "monorepo"
#[allow(dead_code)]
fn auto_register_path_hierarchy(conn: &rusqlite::Connection, namespace: &str) {
    // Only run if this namespace doesn't already have an explicit parent
    if db::get_namespace_parent(conn, namespace).is_some() {
        return;
    }
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let home = dirs::home_dir().unwrap_or_default();
    // Walk up from parent of cwd (cwd itself IS the namespace)
    let mut current = cwd.parent().map(std::path::Path::to_path_buf);
    while let Some(dir) = current {
        // Stop at or above home directory
        if dir == home || !dir.starts_with(&home) {
            break;
        }
        if let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()) {
            // Check if this directory name has a namespace standard
            if db::get_namespace_standard(conn, dir_name)
                .ok()
                .flatten()
                .is_some()
            {
                // Found a parent with a standard — register it
                let now = chrono::Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "UPDATE namespace_meta SET parent_namespace = ?1, updated_at = ?2 WHERE namespace = ?3 AND parent_namespace IS NULL",
                    rusqlite::params![dir_name, now, namespace],
                );
                tracing::info!(
                    "auto-registered parent namespace: {} -> {}",
                    namespace,
                    dir_name
                );
                break;
            }
        }
        current = dir.parent().map(std::path::Path::to_path_buf);
    }
}

// ---------------------------------------------------------------------------
// Archive tool handlers
// ---------------------------------------------------------------------------

fn handle_archive_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(50)).expect("u64 as usize");
    let offset = usize::try_from(params["offset"].as_u64().unwrap_or(0)).expect("u64 as usize");
    let items =
        db::list_archived(conn, namespace, limit.min(1000), offset).map_err(|e| e.to_string())?;
    Ok(json!({"archived": items, "count": items.len()}))
}

fn handle_archive_restore(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    crate::validate::validate_id(id).map_err(|e| e.to_string())?;
    let restored = db::restore_archived(conn, id).map_err(|e| e.to_string())?;
    if !restored {
        return Err("not found in archive".into());
    }
    Ok(json!({"restored": true, "id": id}))
}

fn handle_archive_purge(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let older_than_days = params["older_than_days"].as_i64();
    let purged = db::purge_archive(conn, older_than_days).map_err(|e| e.to_string())?;
    Ok(json!({"purged": purged}))
}

fn handle_archive_stats(conn: &rusqlite::Connection) -> Result<Value, String> {
    db::archive_stats(conn).map_err(|e| e.to_string())
}

fn handle_gc(conn: &rusqlite::Connection, params: &Value, archive: bool) -> Result<Value, String> {
    let dry_run = params["dry_run"].as_bool().unwrap_or(false);
    if dry_run {
        // Just count expired without deleting
        let now = chrono::Utc::now().to_rfc3339();
        let count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
                rusqlite::params![now],
                |r| r.get(0),
            )
            .unwrap_or(0);
        return Ok(json!({"collected": count, "dry_run": true}));
    }
    let count = db::gc(conn, archive).map_err(|e| e.to_string())?;
    Ok(json!({"collected": count, "dry_run": false}))
}

fn handle_session_start(
    conn: &rusqlite::Connection,
    params: &Value,
    llm: Option<&OllamaClient>,
) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(10)).expect("u64 as usize");

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
    inject_namespace_standard(conn, namespace, &mut response);

    Ok(response)
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn handle_request(
    conn: &rusqlite::Connection,
    db_path: &Path,
    req: &RpcRequest,
    embedder: Option<&Embedder>,
    llm: Option<&OllamaClient>,
    reranker: Option<&CrossEncoder>,
    tier_config: &TierConfig,
    vector_index: Option<&VectorIndex>,
    resolved_ttl: &crate::config::ResolvedTtl,
    archive_on_gc: bool,
    mcp_client: Option<&str>,
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
                "capabilities": { "tools": {}, "prompts": {} },
                "serverInfo": {
                    "name": "ai-memory",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "notifications/initialized" | "ping" => ok_response(id, json!({})),
        "tools/list" => ok_response(id, tool_definitions()),
        "prompts/list" => ok_response(id, prompt_definitions()),
        "prompts/get" => {
            let prompt_name = match req.params["name"].as_str() {
                Some(name) if !name.is_empty() => name,
                _ => return err_response(id, -32602, "missing or empty prompt name".into()),
            };
            match prompt_content(prompt_name, &req.params) {
                Ok(val) => ok_response(id, val),
                Err(e) => err_response(id, -32602, e),
            }
        }
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
                "memory_store" => handle_store(
                    conn,
                    arguments,
                    embedder,
                    vector_index,
                    resolved_ttl,
                    mcp_client,
                ),
                "memory_recall" => handle_recall(
                    conn,
                    arguments,
                    embedder,
                    vector_index,
                    reranker,
                    archive_on_gc,
                    resolved_ttl,
                ),
                "memory_search" => handle_search(conn, arguments),
                "memory_list" => handle_list(conn, arguments),
                "memory_delete" => handle_delete(conn, arguments, vector_index),
                "memory_promote" => handle_promote(conn, arguments),
                "memory_forget" => handle_forget(conn, arguments, archive_on_gc),
                "memory_stats" => handle_stats(conn, db_path),
                "memory_update" => handle_update(conn, arguments, embedder, vector_index),
                "memory_get" => handle_get(conn, arguments),
                "memory_link" => handle_link(conn, arguments),
                "memory_get_links" => handle_get_links(conn, arguments),
                "memory_consolidate" => {
                    handle_consolidate(conn, arguments, llm, embedder, vector_index, mcp_client)
                }
                "memory_capabilities" => handle_capabilities(tier_config, reranker),
                "memory_expand_query" => handle_expand_query(llm, arguments),
                "memory_auto_tag" => handle_auto_tag(conn, llm, arguments),
                "memory_detect_contradiction" => handle_detect_contradiction(conn, llm, arguments),
                "memory_archive_list" => handle_archive_list(conn, arguments),
                "memory_archive_restore" => handle_archive_restore(conn, arguments),
                "memory_archive_purge" => handle_archive_purge(conn, arguments),
                "memory_archive_stats" => handle_archive_stats(conn),
                "memory_gc" => handle_gc(conn, arguments, archive_on_gc),
                "memory_session_start" => handle_session_start(conn, arguments, llm),
                "memory_namespace_set_standard" => handle_namespace_set_standard(conn, arguments),
                "memory_namespace_get_standard" => handle_namespace_get_standard(conn, arguments),
                "memory_namespace_clear_standard" => {
                    handle_namespace_clear_standard(conn, arguments)
                }
                _ => Err(format!("unknown tool: {tool_name}")),
            };

            match result {
                Ok(val) => {
                    // Check if TOON format requested for recall/search/list
                    let format_str = arguments
                        .get("format")
                        .and_then(|v| v.as_str())
                        .unwrap_or("toon_compact");
                    let text = match format_str {
                        "toon"
                            if matches!(
                                tool_name,
                                "memory_recall" | "memory_list" | "memory_session_start"
                            ) =>
                        {
                            crate::toon::memories_to_toon(&val, false)
                        }
                        "toon_compact"
                            if matches!(
                                tool_name,
                                "memory_recall" | "memory_list" | "memory_session_start"
                            ) =>
                        {
                            crate::toon::memories_to_toon(&val, true)
                        }
                        "toon" if tool_name == "memory_search" => {
                            crate::toon::search_to_toon(&val, false)
                        }
                        "toon_compact" if tool_name == "memory_search" => {
                            crate::toon::search_to_toon(&val, true)
                        }
                        _ => serde_json::to_string_pretty(&val).unwrap_or_default(),
                    };
                    ok_response(
                        id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": text
                            }]
                        }),
                    )
                }
                Err(e) => ok_response(
                    id,
                    json!({
                        "content": [{"type": "text", "text": e}],
                        "isError": true
                    }),
                ),
            }
        }
        _ => err_response(id, -32601, format!("method not found: {}", req.method)),
    }
}

/// Run the MCP server over stdio. Blocks until stdin closes.
/// Initializes components based on the requested feature tier.
#[allow(clippy::too_many_lines)]
pub fn run_mcp_server(
    db_path: &Path,
    tier: FeatureTier,
    app_config: &AppConfig,
) -> anyhow::Result<()> {
    let conn = db::open(db_path)?;
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut tier_config = tier.config();
    eprintln!("ai-memory: requested tier = {}", tier.as_str());

    // Apply config.toml overrides — tiers gate features, models are independently configurable
    // Only override if the tier actually uses an LLM (smart/autonomous)
    if tier_config.llm_model.is_some()
        && let Some(ref llm_override) = app_config.llm_model
    {
        match llm_override.as_str() {
            "gemma4:e2b" => {
                tier_config.llm_model = Some(crate::config::LlmModel::Gemma4E2B);
                eprintln!("ai-memory: llm_model override from config: gemma4:e2b");
            }
            "gemma4:e4b" => {
                tier_config.llm_model = Some(crate::config::LlmModel::Gemma4E4B);
                eprintln!("ai-memory: llm_model override from config: gemma4:e4b");
            }
            other => eprintln!("ai-memory: unknown llm_model '{other}', using tier default"),
        }
    }

    // Apply embedding model override from config.toml
    if tier_config.embedding_model.is_some()
        && let Some(ref emb_override) = app_config.embedding_model
    {
        match emb_override.as_str() {
            "mini_lm_l6_v2" => {
                tier_config.embedding_model = Some(crate::config::EmbeddingModel::MiniLmL6V2);
                eprintln!("ai-memory: embedding_model override from config: mini_lm_l6_v2 (local)");
            }
            "nomic_embed_v15" => {
                tier_config.embedding_model = Some(crate::config::EmbeddingModel::NomicEmbedV15);
                eprintln!(
                    "ai-memory: embedding_model override from config: nomic_embed_v15 (Ollama)"
                );
            }
            other => {
                eprintln!("ai-memory: unknown embedding_model '{other}', using tier default");
            }
        }
    }

    // --- Initialize LLM (smart tier and above) — before embedder so Ollama
    //     client can be shared with nomic embedder ---
    let llm: Option<Arc<OllamaClient>> = if let Some(ref llm_model) = tier_config.llm_model {
        let model_id = llm_model.ollama_model_id();
        eprintln!(
            "ai-memory: connecting to Ollama for {} ...",
            llm_model.display_name()
        );
        let ollama_url = app_config.effective_ollama_url();
        match OllamaClient::new_with_url(ollama_url, model_id) {
            Ok(client) => {
                eprintln!("ai-memory: Ollama connected, ensuring model {model_id} is available...");
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
    // Use a separate embed client if embed_url is configured differently from ollama_url
    let embed_client: Option<Arc<OllamaClient>> = {
        let embed_url = app_config.effective_embed_url();
        let ollama_url = app_config.effective_ollama_url();
        if embed_url == ollama_url {
            llm.clone()
        } else {
            // Separate embed URL configured — create a dedicated client for embeddings
            eprintln!("ai-memory: using separate embed URL: {embed_url}");
            match OllamaClient::new_with_url(embed_url, "nomic-embed-text") {
                Ok(client) => Some(Arc::new(client)),
                Err(e) => {
                    eprintln!("ai-memory: embed client failed: {e}, falling back to LLM client");
                    llm.clone()
                }
            }
        }
    };
    let embedder = if let Some(ref emb_model) = tier_config.embedding_model {
        match Embedder::for_model(*emb_model, embed_client) {
            Ok(emb) => {
                eprintln!("ai-memory: embedder loaded ({})", emb.model_description());
                // Backfill embeddings for memories that don't have them
                match db::get_unembedded_ids(&conn) {
                    Ok(unembedded) if !unembedded.is_empty() => {
                        eprintln!("ai-memory: backfilling {} memories...", unembedded.len());
                        let mut ok = 0usize;
                        for (id, title, content) in &unembedded {
                            let text = format!("{title} {content}");
                            match emb.embed(&text) {
                                Ok(embedding) => {
                                    if db::set_embedding(&conn, id, &embedding).is_ok() {
                                        ok += 1;
                                    }
                                }
                                Err(e) => {
                                    eprintln!(
                                        "ai-memory: embed failed for {}: {}",
                                        &id[..8.min(id.len())],
                                        e
                                    );
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
                eprintln!(
                    "ai-memory: building HNSW index ({} vectors)...",
                    entries.len()
                );
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
    eprintln!("ai-memory MCP server started (stdio, tier={effective_tier})");

    // Captured from the MCP `initialize` handshake's `clientInfo.name`.
    // Used by `crate::identity` to synthesize an `ai:<client>@<host>:pid-<pid>`
    // agent_id when the caller doesn't supply one explicitly.
    let mut mcp_client_name: Option<String> = None;

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

        // Capture clientInfo.name on initialize (even if id is Null / notification-style).
        if req.method == "initialize"
            && let Some(name) = req.params["clientInfo"]["name"].as_str()
            && !name.is_empty()
        {
            mcp_client_name = Some(name.to_string());
        }

        // Notifications have no id — no response expected per JSON-RPC spec
        if req.id.is_none() || req.id == Some(Value::Null) {
            continue;
        }

        let resolved_ttl = app_config.effective_ttl();
        let archive_on_gc = app_config.effective_archive_on_gc();
        let resp = handle_request(
            &conn,
            db_path,
            &req,
            embedder.as_ref(),
            llm.as_deref(),
            reranker.as_ref(),
            &tier_config,
            vector_index.as_ref(),
            &resolved_ttl,
            archive_on_gc,
            mcp_client_name.as_deref(),
        );
        let out = serde_json::to_string(&resp)?;
        writeln!(stdout, "{out}")?;
        stdout.flush()?;
    }

    let _ = db::checkpoint(&conn);
    eprintln!("ai-memory MCP server stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_definitions_returns_26_tools() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 26);
    }

    #[test]
    fn tool_definitions_all_have_names() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        for tool in tools {
            assert!(tool["name"].as_str().unwrap().starts_with("memory_"));
        }
    }

    #[test]
    fn tool_definitions_recall_has_toon_default() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        let recall = tools.iter().find(|t| t["name"] == "memory_recall").unwrap();
        let format_schema = &recall["inputSchema"]["properties"]["format"];
        assert_eq!(format_schema["default"], "toon_compact");
    }

    #[test]
    fn prompt_definitions_returns_2() {
        let defs = prompt_definitions();
        let prompts = defs["prompts"].as_array().unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0]["name"], "recall-first");
        assert_eq!(prompts[1]["name"], "memory-workflow");
    }

    #[test]
    fn prompt_definitions_recall_first_has_arguments() {
        let defs = prompt_definitions();
        let prompts = defs["prompts"].as_array().unwrap();
        let recall_first = &prompts[0];
        let args = recall_first["arguments"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0]["name"], "namespace");
    }

    #[test]
    fn prompt_content_recall_first() {
        let params = json!({});
        let result = prompt_content("recall-first", &params).unwrap();
        let msgs = result["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        let text = msgs[0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("RECALL FIRST"));
        assert!(text.contains("TOON"));
        assert!(text.contains("memory_recall"));
        assert!(text.contains("memory_store"));
        assert!(text.contains("DEDUP"));
    }

    #[test]
    fn prompt_content_recall_first_with_namespace() {
        let params = json!({"arguments": {"namespace": "my-project"}});
        let result = prompt_content("recall-first", &params).unwrap();
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("my-project"));
    }

    #[test]
    fn prompt_content_memory_workflow() {
        let params = json!({});
        let result = prompt_content("memory-workflow", &params).unwrap();
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("STORE"));
        assert!(text.contains("RECALL"));
        assert!(text.contains("SEARCH"));
        assert!(text.contains("CONSOLIDATE"));
        assert!(text.contains("TOON"));
    }

    #[test]
    fn prompt_content_unknown() {
        let params = json!({});
        let result = prompt_content("nonexistent", &params);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown prompt"));
    }

    #[test]
    fn prompt_content_role_is_user() {
        let params = json!({});
        let result = prompt_content("recall-first", &params).unwrap();
        assert_eq!(result["messages"][0]["role"], "user");
    }

    #[test]
    fn ok_response_structure() {
        let resp = ok_response(json!(1), json!({"test": true}));
        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, json!(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn err_response_structure() {
        let resp = err_response(json!(1), -32600, "test error".to_string());
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "test error");
    }
}
