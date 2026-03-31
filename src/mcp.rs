//! MCP (Model Context Protocol) server for claude-memory.
//! Exposes memory operations as native Claude Code tools over stdio JSON-RPC.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::db;
use crate::models::*;
use crate::validate;

// --- JSON-RPC types ---

#[derive(Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
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
                        "source": {"type": "string", "enum": ["user", "claude", "hook", "api", "cli", "system"], "default": "claude"}
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
                        "since": {"type": "string", "description": "Only memories created after this RFC3339 timestamp"}
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
                        "limit": {"type": "integer", "default": 20}
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
                        "limit": {"type": "integer", "default": 20}
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
            }
        ]
    })
}

// --- Tool handlers ---

fn handle_store(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
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

    let actual_id = db::insert(conn, &mem).map_err(|e| e.to_string())?;
    Ok(json!({"id": actual_id, "tier": mem.tier, "title": mem.title, "namespace": mem.namespace}))
}

fn handle_recall(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let context = params["context"].as_str().ok_or("context is required")?;
    let namespace = params["namespace"].as_str();
    let limit = params["limit"].as_u64().unwrap_or(10) as usize;
    let tags = params["tags"].as_str();
    let since = params["since"].as_str();

    let results = db::recall(conn, context, namespace, limit.min(50), tags, since)
        .map_err(|e| e.to_string())?;
    Ok(json!({"memories": results, "count": results.len()}))
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

fn handle_delete(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    let deleted = db::delete(conn, id).map_err(|e| e.to_string())?;
    if deleted {
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

// --- MCP protocol handler ---

fn handle_request(conn: &rusqlite::Connection, db_path: &Path, req: &RpcRequest) -> RpcResponse {
    let id = req.id.clone().unwrap_or(Value::Null);

    match req.method.as_str() {
        "initialize" => ok_response(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "claude-memory",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "notifications/initialized" => ok_response(id, json!({})),
        "tools/list" => ok_response(id, tool_definitions()),
        "tools/call" => {
            let tool_name = req.params["name"].as_str().unwrap_or("");
            let arguments = &req.params["arguments"];

            let result = match tool_name {
                "memory_store" => handle_store(conn, arguments),
                "memory_recall" => handle_recall(conn, arguments),
                "memory_search" => handle_search(conn, arguments),
                "memory_list" => handle_list(conn, arguments),
                "memory_delete" => handle_delete(conn, arguments),
                "memory_promote" => handle_promote(conn, arguments),
                "memory_forget" => handle_forget(conn, arguments),
                "memory_stats" => handle_stats(conn, db_path),
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
pub fn run_mcp_server(db_path: &Path) -> anyhow::Result<()> {
    let conn = db::open(db_path)?;
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    eprintln!("claude-memory MCP server started (stdio)");

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

        // Skip notifications (no id = no response expected)
        if req.id.is_none() || req.id == Some(Value::Null) {
            // Still process initialize notifications
            if req.method == "notifications/initialized" {
                continue;
            }
        }

        let resp = handle_request(&conn, db_path, &req);
        let out = serde_json::to_string(&resp)?;
        writeln!(stdout, "{out}")?;
        stdout.flush()?;
    }

    eprintln!("claude-memory MCP server stopped");
    Ok(())
}
