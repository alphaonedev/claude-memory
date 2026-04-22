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
use crate::models::{GovernancePolicy, Memory, Tier};
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

/// Version tag for the `tools/list` response schema. Bumped whenever
/// an existing tool's shape changes in a breaking way (renamed params,
/// tightened schemas, removed options). Adding a new tool is additive
/// and does NOT require a bump. Ultrareview #351.
const TOOLS_VERSION: &str = "2026-04-22";

#[allow(clippy::too_many_lines)]
fn tool_definitions() -> Value {
    json!({
        "toolsVersion": TOOLS_VERSION,
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
                        "source": {"type": "string", "enum": ["user", "claude", "hook", "api", "cli", "import", "consolidation", "system", "chaos"], "default": "claude"},
                        "metadata": {"type": "object", "description": "Arbitrary JSON metadata", "default": {}},
                        "agent_id": {"type": "string", "description": "Agent identifier. If omitted, the server synthesizes an NHI-hardened default (ai:<client>@<host>:pid-<pid>, host:<host>:pid-<pid>-<uuid8>, or anonymous:pid-<pid>-<uuid8>)."},
                        "scope": {"type": "string", "enum": ["private", "team", "unit", "org", "collective"], "description": "Task 1.5 visibility scope. Defaults to private when unset. Stored as metadata.scope."}
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
                        "as_agent": {"type": "string", "description": "Querying agent's namespace position (Task 1.5). Enables scope-based visibility filtering — results include private memories at this namespace, team/unit/org memories at ancestor subtrees, and collective memories globally."},
                        "budget_tokens": {"type": "integer", "minimum": 1, "description": "Task 1.11 — context-budget-aware recall. Return the top-ranked memories whose cumulative estimated tokens (title+content, ~4 chars/token) fit in N. Response includes tokens_used + budget_tokens."},
                        "context_tokens": {"type": "array", "items": {"type": "string"}, "description": "v0.6.0.0 contextual recall — recent conversation tokens used to bias the query embedding at 70/30 (primary/context). Pulls results toward memories that match both the explicit query and nearby conversation topics."},
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
                        "as_agent": {"type": "string", "description": "Querying agent's namespace position (Task 1.5) for scope-based visibility filtering."},
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
                "description": "Promote a memory. Default: bump tier to long-term (permanent, clears expiry). Task 1.7: when 'to_namespace' is supplied, clone the memory to a hierarchical-ancestor namespace and link clone → source with 'derived_from'. Original is untouched.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "to_namespace": {"type": "string", "description": "Task 1.7: hierarchical-ancestor namespace to clone this memory into. Must be a proper ancestor (per namespace_ancestors()). Original memory stays put; a new memory with derived_from link is created at the target namespace."}
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
                "description": "Set a memory as the standard/policy for a namespace. Auto-prepended to recall and session_start. Supports rule layering (global '*' + parent chain + namespace). Task 1.8: accepts optional `governance` policy object merged into the standard memory's metadata.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to set the standard for"},
                        "id": {"type": "string", "description": "Memory ID to use as the standard"},
                        "parent": {"type": "string", "description": "Optional parent namespace to inherit standards from (rule layering)"},
                        "governance": {
                            "type": "object",
                            "description": "Task 1.8 governance policy. Stored in metadata.governance on the standard memory. Consumed by Task 1.9 enforcement + 1.10 approver types.",
                            "properties": {
                                "write":    {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "promote":  {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "delete":   {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "approver": {"description": "ApproverType: \"human\" | {\"agent\": \"<id>\"} | {\"consensus\": <n>}"}
                            }
                        }
                    },
                    "required": ["namespace", "id"]
                }
            },
            {
                "name": "memory_namespace_get_standard",
                "description": "Get the standard/policy memory for a namespace, if one is set. With inherit=true returns the full N-level resolved chain (Task 1.6).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to get the standard for"},
                        "inherit": {"type": "boolean", "default": false, "description": "Task 1.6: when true, return the full inheritance chain (global * → ancestors → namespace) as a list instead of the single namespace's standard."}
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
            },
            {
                "name": "memory_pending_list",
                "description": "List pending governance-queued actions (Task 1.9). Filter by status: pending (default) / approved / rejected.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status": {"type": "string", "enum": ["pending", "approved", "rejected"]},
                        "limit":  {"type": "integer", "default": 100, "maximum": 1000}
                    }
                }
            },
            {
                "name": "memory_pending_approve",
                "description": "Approve a pending action by id (Task 1.9). Caller identity is stamped as decided_by.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Pending action id"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_pending_reject",
                "description": "Reject a pending action by id (Task 1.9). Caller identity is stamped as decided_by.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Pending action id"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_agent_register",
                "description": "Register an agent in the reserved _agents namespace. Stores agent_type and capabilities, refreshes last_seen_at on re-registration while preserving registered_at. agent_id is claimed, not attested.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Agent identifier (same validation as metadata.agent_id)"},
                        "agent_type": {"type": "string", "enum": ["ai:claude-opus-4.6", "ai:claude-opus-4.7", "ai:codex-5.4", "ai:grok-4.2", "human", "system"]},
                        "capabilities": {"type": "array", "items": {"type": "string"}, "default": [], "description": "Optional capability tags"}
                    },
                    "required": ["agent_id", "agent_type"]
                }
            },
            {
                "name": "memory_agent_list",
                "description": "List every registered agent.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_notify",
                "description": "v0.6.0.0 — send a message from the caller to another agent. Stored as a memory in the reserved `_messages/<target>` namespace with sender metadata. The sender is the caller's resolved agent_id. Target agent reads via `memory_inbox`. Payload is a free-form string.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "target_agent_id": {"type": "string", "description": "Recipient agent_id (same validation as metadata.agent_id)"},
                        "title": {"type": "string", "description": "Short subject (≤ 200 chars, required)"},
                        "payload": {"type": "string", "description": "Message body (required)"},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid", "description": "short TTL default = 6h, mid = 7d, long = no expiry"}
                    },
                    "required": ["target_agent_id", "title", "payload"]
                }
            },
            {
                "name": "memory_inbox",
                "description": "v0.6.0.0 — list messages sent to an agent via memory_notify. Reads the reserved `_messages/<agent_id>` namespace. `access_count == 0` is the conventional unread marker; recalling/reading a memory increments access_count via the normal touch path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Recipient agent_id. Defaults to the caller's resolved agent_id."},
                        "unread_only": {"type": "boolean", "default": false, "description": "When true, return only messages with access_count == 0."},
                        "limit": {"type": "integer", "default": 50, "maximum": 500}
                    }
                }
            },
            {
                "name": "memory_subscribe",
                "description": "v0.6.0.0 — register a webhook subscription. Events fire on memory_store today and additional events in v0.6.1+. Payload is a JSON body signed with HMAC-SHA256 when a secret is supplied (header: X-Ai-Memory-Signature: sha256=<hex>). URL must be https unless the host is a loopback address. The shared secret is stored hashed only; the plaintext the operator supplies is what they verify signatures with.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "https:// endpoint (or http:// for loopback). SSRF guard rejects private-range IPs."},
                        "events": {"type": "string", "default": "*", "description": "Comma-separated event whitelist or `*` for all. Known events: memory_store, memory_delete, memory_promote."},
                        "secret": {"type": "string", "description": "Optional shared secret for HMAC signing. If omitted, payload is unsigned."},
                        "namespace_filter": {"type": "string", "description": "Optional exact namespace match."},
                        "agent_filter": {"type": "string", "description": "Optional agent_id filter — only events whose stored agent_id matches this value will fire."}
                    },
                    "required": ["url"]
                }
            },
            {
                "name": "memory_unsubscribe",
                "description": "v0.6.0.0 — delete a subscription by id.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_list_subscriptions",
                "description": "v0.6.0.0 — list active webhook subscriptions. Secrets are not exposed; only `secret_hash` is stored and even that is not returned.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
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

/// Minimum content length (bytes) before the post-store autonomy hook
/// will invoke LLM `auto_tag` / `detect_contradiction`. Below this the
/// LLM round-trip cost exceeds the informational payoff.
const AUTONOMY_MIN_CONTENT_LEN: usize = 50;

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn handle_store(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    embedder: Option<&Embedder>,
    llm: Option<&OllamaClient>,
    vector_index: Option<&VectorIndex>,
    resolved_ttl: &crate::config::ResolvedTtl,
    autonomous_hooks: bool,
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
    // #151 scope: top-level `scope` param OR inline metadata.scope
    let explicit_scope = params["scope"]
        .as_str()
        .or_else(|| metadata.get("scope").and_then(serde_json::Value::as_str))
        .map(str::to_string);
    if let Some(ref s) = explicit_scope {
        validate::validate_scope(s).map_err(|e| e.to_string())?;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("scope".to_string(), serde_json::Value::String(s.clone()));
        }
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

    // Task 1.9: governance enforcement (store-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let payload = serde_json::to_value(&mem).unwrap_or_default();
        match db::enforce_governance(
            conn,
            GovernedAction::Store,
            &mem.namespace,
            &agent_id,
            None,
            None,
            &payload,
        )
        .map_err(|e| e.to_string())?
        {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                return Err(format!("store denied by governance: {reason}"));
            }
            GovernanceDecision::Pending(pending_id) => {
                return Ok(json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": "store",
                    "namespace": mem.namespace,
                }));
            }
        }
    }

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
        // #196: echo the preserved agent_id (original on dedup, not the caller's)
        let echoed_agent_id = preserved_metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return Ok(json!({
            "id": dup.id,
            "tier": mem.tier,
            "title": mem.title,
            "namespace": mem.namespace,
            "agent_id": echoed_agent_id,
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

    // v0.6.0.0 post-store autonomy hooks. When enabled via
    // `AI_MEMORY_AUTONOMOUS_HOOKS=1` or `autonomous_hooks = true` in
    // config.toml AND an LLM is wired AND the content is long enough
    // to be meaningfully taggable, fire `auto_tag` + `detect_contradiction`
    // synchronously and persist the results into the memory's metadata.
    // Best-effort: any LLM error is logged and does not fail the store.
    // Skipped for internal/system namespaces to avoid feedback loops.
    let mut auto_tags: Vec<String> = Vec::new();
    let mut confirmed_contradictions: Vec<String> = Vec::new();
    let hooks_skipped_reason: Option<&'static str> = if !autonomous_hooks {
        Some("disabled")
    } else if llm.is_none() {
        Some("no_llm")
    } else if mem.content.len() < AUTONOMY_MIN_CONTENT_LEN {
        Some("content_too_short")
    } else if mem.namespace.starts_with('_') {
        Some("internal_namespace")
    } else {
        None
    };
    if hooks_skipped_reason.is_none()
        && let Some(llm_client) = llm
    {
        match llm_client.auto_tag(&mem.title, &mem.content) {
            Ok(tags) => {
                auto_tags = tags.into_iter().take(8).collect();
            }
            Err(e) => {
                tracing::warn!("auto_tag hook failed for {}: {}", &actual_id, e);
            }
        }
        for cand in &existing {
            if cand.id == actual_id || cand.id == mem.id {
                continue;
            }
            match llm_client.detect_contradiction(&mem.content, &cand.content) {
                Ok(true) => confirmed_contradictions.push(cand.id.clone()),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        "detect_contradiction hook failed ({actual_id} vs {}): {e}",
                        cand.id
                    );
                }
            }
        }
        // Persist hook results into metadata. Best-effort — a failed update
        // here does not fail the store (the memory is already committed).
        if !auto_tags.is_empty() || !confirmed_contradictions.is_empty() {
            let mut updated_metadata = mem.metadata.clone();
            if let Some(obj) = updated_metadata.as_object_mut() {
                if !auto_tags.is_empty() {
                    obj.insert("auto_tags".to_string(), json!(auto_tags));
                }
                if !confirmed_contradictions.is_empty() {
                    obj.insert(
                        "confirmed_contradictions".to_string(),
                        json!(confirmed_contradictions),
                    );
                }
            }
            if let Err(e) = db::update(
                conn,
                &actual_id,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(&updated_metadata),
            ) {
                tracing::warn!(
                    "autonomy-hook metadata update failed for {}: {}",
                    &actual_id,
                    e
                );
            }
        }
    }

    // v0.6.0.0: fire webhook subscribers on successful store. Best-effort
    // fire-and-forget — each subscriber gets its own OS thread; the
    // response here does not wait on any webhook dispatch.
    crate::subscriptions::dispatch_event(
        conn,
        "memory_store",
        &actual_id,
        &mem.namespace,
        Some(&agent_id),
        db_path,
    );

    // #196: echo the resolved agent_id
    let mut response = json!({
        "id": actual_id,
        "tier": mem.tier,
        "title": mem.title,
        "namespace": mem.namespace,
        "agent_id": agent_id,
    });
    if !contradiction_ids.is_empty() {
        response["potential_contradictions"] = json!(contradiction_ids);
    }
    if !auto_tags.is_empty() {
        response["auto_tags"] = json!(auto_tags);
    }
    if !confirmed_contradictions.is_empty() {
        response["confirmed_contradictions"] = json!(confirmed_contradictions);
    }
    if let Some(reason) = hooks_skipped_reason
        && autonomous_hooks
    {
        response["autonomy_hook_skipped"] = json!(reason);
    }
    Ok(response)
}

/// Build the standards-inheritance chain for a namespace, most-general
/// first. Task 1.6 extends this from the historical 3-level scheme
/// (global → parent → namespace) to N levels by walking the `/`-derived
/// ancestors from [`crate::models::namespace_ancestors`] plus any
/// `namespace_meta` explicit-parent chain rooted at the top of the
/// hierarchical path (which keeps legacy flat-namespace setups working).
///
/// Returned vector is top-down: `[*, org, unit, team, agent]` for a
/// 4-level hierarchical namespace. Cycle-safe and bounded.
fn build_namespace_chain(conn: &rusqlite::Connection, namespace: &str) -> Vec<String> {
    const MAX_EXPLICIT_DEPTH: usize = 8;
    let mut chain: Vec<String> = Vec::new();

    if namespace == "*" {
        chain.push("*".to_string());
        return chain;
    }

    // Always start with the global standard — most general.
    chain.push("*".to_string());

    // 1. /-derived ancestors. `namespace_ancestors` returns most-specific-first;
    //    reverse for top-down (root ancestor first, then namespace itself last).
    let mut hierarchy_chain: Vec<String> = crate::models::namespace_ancestors(namespace)
        .into_iter()
        .rev()
        .collect();

    // 2. If the ROOTmost of the /-chain has an explicit `namespace_meta` parent,
    //    prepend that chain (bounded by MAX_EXPLICIT_DEPTH + cycle-safe).
    //    Supports legacy flat namespaces (e.g. `ai-memory` → `ai-memory-mcp`).
    if let Some(root) = hierarchy_chain.first().cloned() {
        let mut explicit_above: Vec<String> = Vec::new();
        let mut current = root;
        for _ in 0..MAX_EXPLICIT_DEPTH {
            match db::get_namespace_parent(conn, &current) {
                Some(p)
                    if p != "*"
                        && !explicit_above.contains(&p)
                        && !hierarchy_chain.contains(&p) =>
                {
                    explicit_above.push(p.clone());
                    current = p;
                }
                _ => break,
            }
        }
        // `explicit_above` is [immediate-explicit-parent, grandparent, ...];
        // reverse to prepend in top-down order.
        for p in explicit_above.into_iter().rev() {
            chain.push(p);
        }
    }

    // 3. Append the /-derived chain (top-down).
    for entry in hierarchy_chain.drain(..) {
        if !chain.contains(&entry) {
            chain.push(entry);
        }
    }

    chain
}

/// Inject namespace standards into a `recall/session_start` response.
/// N-level rule layering: global ("*") → root → ... → namespace-specific.
/// Uses [`build_namespace_chain`] to resolve the full ancestor path.
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

    let chain = if let Some(ns) = namespace {
        build_namespace_chain(conn, ns)
    } else {
        // No namespace context — only the global standard applies.
        vec!["*".to_string()]
    };

    for link in chain {
        if let Some(std) = lookup_namespace_standard(conn, &link) {
            add_standard(std, &mut standard_ids, &mut standards);
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

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn handle_recall(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&Embedder>,
    vector_index: Option<&VectorIndex>,
    reranker: Option<&CrossEncoder>,
    archive_on_gc: bool,
    resolved_ttl: &crate::config::ResolvedTtl,
    resolved_scoring: &crate::config::ResolvedScoring,
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
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(10)).unwrap_or(usize::MAX);
    let tags = params["tags"].as_str();
    let since = params["since"].as_str();
    let until = params["until"].as_str();
    // #151 visibility
    let as_agent = params["as_agent"].as_str();
    if let Some(a) = as_agent {
        validate::validate_namespace(a).map_err(|e| e.to_string())?;
    }
    // Task 1.11: optional token budget.
    // Ultrareview #348: reject budget_tokens=0 explicitly. An off-by-one
    // or uninitialized counter passed as 0 would previously return an
    // empty result with no error — hides the caller's bug.
    let budget_tokens = match params["budget_tokens"].as_u64() {
        Some(0) => {
            return Err("budget_tokens must be >= 1".to_string());
        }
        Some(n) => usize::try_from(n).ok(),
        None => None,
    };

    // v0.6.0.0 contextual recall — caller-supplied recent conversation tokens.
    let context_tokens: Vec<String> = params["context_tokens"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Helper: tack tokens_used / budget_tokens onto the response metadata.
    let decorate_budget = |resp: &mut Value, tokens_used: usize| {
        resp["tokens_used"] = json!(tokens_used);
        if let Some(b) = budget_tokens {
            resp["budget_tokens"] = json!(b);
        }
    };

    // Use hybrid recall if embedder is available
    if let Some(emb) = embedder {
        match emb.embed(context) {
            Ok(primary_emb) => {
                // v0.6.0.0: fuse primary query with context-token embedding
                // at 70/30 when caller supplied conversation tokens.
                let query_emb = if context_tokens.is_empty() {
                    primary_emb
                } else {
                    let joined = context_tokens.join(" ");
                    match emb.embed(&joined) {
                        Ok(ctx_emb) => {
                            crate::embeddings::Embedder::fuse(&primary_emb, &ctx_emb, 0.7)
                        }
                        Err(e) => {
                            tracing::warn!("context_tokens embed failed, using primary only: {e}");
                            primary_emb
                        }
                    }
                };
                let (results, tokens_used) = db::recall_hybrid(
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
                    as_agent,
                    budget_tokens,
                    resolved_scoring,
                )
                .map_err(|e| e.to_string())?;

                // Apply cross-encoder reranking if available
                if let Some(ce) = reranker {
                    let ce_reranked = ce.rerank(context, results);
                    let memories = scored_memories(ce_reranked);
                    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "hybrid+rerank"});
                    decorate_budget(&mut resp, tokens_used);
                    inject_namespace_standard(conn, namespace, &mut resp);
                    return Ok(resp);
                }

                let memories = scored_memories(results);
                let mut resp =
                    json!({"memories": memories, "count": memories.len(), "mode": "hybrid"});
                decorate_budget(&mut resp, tokens_used);
                inject_namespace_standard(conn, namespace, &mut resp);
                return Ok(resp);
            }
            Err(e) => {
                tracing::warn!("embedding failed, falling back to FTS: {}", e);
            }
        }
    }

    // Fallback to keyword-only recall
    let (results, tokens_used) = db::recall(
        conn,
        context,
        namespace,
        limit.min(50),
        tags,
        since,
        until,
        resolved_ttl.short_extend_secs,
        resolved_ttl.mid_extend_secs,
        as_agent,
        budget_tokens,
    )
    .map_err(|e| e.to_string())?;
    let memories = scored_memories(results);
    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "keyword"});
    decorate_budget(&mut resp, tokens_used);
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
    // Ultrareview #339: saturate instead of panic on 32-bit targets
    // where u64 may exceed usize::MAX. A malicious client passing
    // limit=2^63 would otherwise take down the daemon.
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(20)).unwrap_or(usize::MAX);

    let agent_id = params["agent_id"].as_str();
    if let Some(aid) = agent_id {
        validate::validate_agent_id(aid).map_err(|e| e.to_string())?;
    }
    let as_agent = params["as_agent"].as_str();
    if let Some(a) = as_agent {
        validate::validate_namespace(a).map_err(|e| e.to_string())?;
    }
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
        as_agent,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"results": results, "count": results.len()}))
}

fn handle_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    // Ultrareview #339: saturate instead of panic (see handle_search).
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(20)).unwrap_or(usize::MAX);
    let agent_id = params["agent_id"].as_str();
    if let Some(aid) = agent_id {
        validate::validate_agent_id(aid).map_err(|e| e.to_string())?;
    }

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
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;

    // Resolve the memory first so governance has owner context.
    let target = if let Some(m) = db::get(conn, id).map_err(|e| e.to_string())? {
        Some(m)
    } else {
        db::get_by_prefix(conn, id).map_err(|e| e.to_string())?
    };
    let Some(target) = target else {
        return Err("memory not found".into());
    };

    // Task 1.9: governance enforcement (delete-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
            .map_err(|e| e.to_string())?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({"id": target.id, "title": target.title});
        match db::enforce_governance(
            conn,
            GovernedAction::Delete,
            &target.namespace,
            &agent_id,
            Some(&target.id),
            mem_owner.as_deref(),
            &payload,
        )
        .map_err(|e| e.to_string())?
        {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                return Err(format!("delete denied by governance: {reason}"));
            }
            GovernanceDecision::Pending(pending_id) => {
                return Ok(json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": "delete",
                    "memory_id": target.id,
                }));
            }
        }
    }

    let deleted = db::delete(conn, &target.id).map_err(|e| e.to_string())?;
    if deleted {
        if let Some(idx) = vector_index {
            idx.remove(&target.id);
        }
        Ok(json!({"deleted": true}))
    } else {
        Err("memory not found".into())
    }
}

fn handle_promote(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    // Resolve prefix if exact ID not found; capture the memory so governance
    // has owner context (Task 1.9).
    let target = if let Some(m) = db::get(conn, id).map_err(|e| e.to_string())? {
        m
    } else if let Some(m) = db::get_by_prefix(conn, id).map_err(|e| e.to_string())? {
        m
    } else {
        return Err("memory not found".into());
    };
    let resolved_id = target.id.clone();

    // Task 1.9: governance enforcement (promote-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
            .map_err(|e| e.to_string())?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({
            "id": resolved_id,
            "to_namespace": params["to_namespace"].as_str(),
        });
        match db::enforce_governance(
            conn,
            GovernedAction::Promote,
            &target.namespace,
            &agent_id,
            Some(&resolved_id),
            mem_owner.as_deref(),
            &payload,
        )
        .map_err(|e| e.to_string())?
        {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                return Err(format!("promote denied by governance: {reason}"));
            }
            GovernanceDecision::Pending(pending_id) => {
                return Ok(json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": "promote",
                    "memory_id": resolved_id,
                }));
            }
        }
    }

    // Task 1.7: optional vertical promotion to an ancestor namespace.
    // When `to_namespace` is supplied, clone (don't move) the memory to the
    // target and link clone → source with `derived_from`. Original is
    // untouched; tier is NOT changed by this path.
    if let Some(to_ns) = params["to_namespace"].as_str() {
        validate::validate_namespace(to_ns).map_err(|e| e.to_string())?;
        let clone_id =
            db::promote_to_namespace(conn, &resolved_id, to_ns).map_err(|e| e.to_string())?;
        return Ok(json!({
            "promoted": true,
            "mode": "vertical",
            "source_id": resolved_id,
            "clone_id": clone_id,
            "to_namespace": to_ns,
        }));
    }

    // Default: tier promotion to long (historical behavior).
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
    Ok(json!({"promoted": true, "mode": "tier", "id": resolved_id, "tier": "long"}))
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

    // Task 1.8: optional governance policy merged into the standard memory's
    // metadata.governance. Policy is deserialized + validated before write.
    let governance_val = params.get("governance").filter(|v| !v.is_null());
    if let Some(g) = governance_val {
        let policy: crate::models::GovernancePolicy =
            serde_json::from_value(g.clone()).map_err(|e| format!("invalid governance: {e}"))?;
        validate::validate_governance_policy(&policy).map_err(|e| e.to_string())?;

        // Load the standard memory, merge metadata.governance, write back.
        let mut mem = db::get(conn, id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("memory not found: {id}"))?;
        let mut metadata = if mem.metadata.is_object() {
            mem.metadata.clone()
        } else {
            serde_json::json!({})
        };
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "governance".to_string(),
                serde_json::to_value(&policy).map_err(|e| e.to_string())?,
            );
        }
        let (found, _) = db::update(
            conn,
            &mem.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&metadata),
        )
        .map_err(|e| e.to_string())?;
        if !found {
            return Err(format!("memory not found during governance merge: {id}"));
        }
        mem.metadata = metadata;
    }

    db::set_namespace_standard(conn, namespace, id, parent).map_err(|e| e.to_string())?;
    let mut resp = json!({"set": true, "namespace": namespace, "standard_id": id});
    if let Some(p) = parent {
        resp["parent"] = json!(p);
    }
    if let Some(g) = governance_val {
        resp["governance"] = g.clone();
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

    // Task 1.6: --inherit returns the full resolved chain, most-general-first.
    let inherit = params["inherit"].as_bool().unwrap_or(false);
    if inherit {
        let chain = build_namespace_chain(conn, namespace);
        let mut standards: Vec<Value> = Vec::new();
        for link in &chain {
            if let Some(std) = lookup_namespace_standard(conn, link) {
                let gov = extract_governance(&std);
                let entry = json!({
                    "namespace": link,
                    "standard_id": std["id"].clone(),
                    "title": std["title"].clone(),
                    "content": std["content"].clone(),
                    "priority": std["priority"].clone(),
                    "governance": gov,
                });
                standards.push(entry);
            }
        }
        return Ok(json!({
            "namespace": namespace,
            "chain": chain,
            "standards": standards,
            "count": standards.len(),
        }));
    }

    let standard_id = db::get_namespace_standard(conn, namespace).map_err(|e| e.to_string())?;
    match standard_id {
        Some(id) => {
            let mem = db::get(conn, &id).map_err(|e| e.to_string())?;
            match mem {
                Some(m) => {
                    // Task 1.8: surface metadata.governance (or default policy).
                    let gov = GovernancePolicy::from_metadata(&m.metadata)
                        .map(Result::unwrap_or_default)
                        .unwrap_or_default();
                    Ok(json!({
                        "namespace": namespace,
                        "standard_id": id,
                        "title": m.title,
                        "content": m.content,
                        "priority": m.priority,
                        "governance": gov,
                    }))
                }
                None => Ok(
                    json!({"namespace": namespace, "standard_id": id, "warning": "standard memory not found — may have been deleted"}),
                ),
            }
        }
        None => Ok(json!({"namespace": namespace, "standard_id": null})),
    }
}

/// Task 1.8 — extract metadata.governance from a serialized memory value,
/// resolving to the default policy when missing or invalid. Used by the
/// `--inherit` get-standard path and tool responses.
fn extract_governance(mem_val: &Value) -> Value {
    let default = serde_json::to_value(GovernancePolicy::default()).unwrap_or(Value::Null);
    let Some(meta) = mem_val.get("metadata") else {
        return default;
    };
    match GovernancePolicy::from_metadata(meta) {
        Some(Ok(p)) => serde_json::to_value(&p).unwrap_or(default),
        _ => default,
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

fn handle_agent_register(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let agent_id = params["agent_id"].as_str().ok_or("agent_id is required")?;
    let agent_type = params["agent_type"]
        .as_str()
        .ok_or("agent_type is required")?;
    let capabilities: Vec<String> = params["capabilities"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    validate::validate_agent_id(agent_id).map_err(|e| e.to_string())?;
    validate::validate_agent_type(agent_type).map_err(|e| e.to_string())?;
    validate::validate_capabilities(&capabilities).map_err(|e| e.to_string())?;

    let id =
        db::register_agent(conn, agent_id, agent_type, &capabilities).map_err(|e| e.to_string())?;

    Ok(json!({
        "registered": true,
        "id": id,
        "agent_id": agent_id,
        "agent_type": agent_type,
        "capabilities": capabilities,
    }))
}

fn handle_agent_list(conn: &rusqlite::Connection) -> Result<Value, String> {
    let agents = db::list_agents(conn).map_err(|e| e.to_string())?;
    Ok(json!({
        "count": agents.len(),
        "agents": agents,
    }))
}

// --- v0.6.0.0 agent notify / inbox -----------------------------------------

/// Compose the canonical inbox namespace for a given `agent_id`.
///
/// Reuses the same sanitization regex that `validate_namespace` enforces
/// on writes, so any `agent_id` that passes `validate::validate_agent_id`
/// produces an acceptable namespace here.
fn messages_namespace_for(agent_id: &str) -> String {
    format!("_messages/{agent_id}")
}

fn handle_notify(
    conn: &rusqlite::Connection,
    params: &Value,
    resolved_ttl: &crate::config::ResolvedTtl,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let target = params["target_agent_id"]
        .as_str()
        .ok_or("target_agent_id is required")?;
    let title = params["title"].as_str().ok_or("title is required")?;
    let payload = params["payload"].as_str().ok_or("payload is required")?;
    let priority = i32::try_from(params["priority"].as_i64().unwrap_or(5))
        .expect("i64 as i32")
        .clamp(1, 10);
    let tier_str = params["tier"].as_str().unwrap_or("mid");
    let tier = Tier::from_str(tier_str).ok_or(format!("invalid tier: {tier_str}"))?;

    validate::validate_agent_id(target).map_err(|e| e.to_string())?;
    validate::validate_title(title).map_err(|e| e.to_string())?;
    validate::validate_content(payload).map_err(|e| e.to_string())?;

    let sender = crate::identity::resolve_agent_id(None, mcp_client).map_err(|e| e.to_string())?;
    let namespace = messages_namespace_for(target);

    let now = chrono::Utc::now();
    let expires_at = resolved_ttl
        .ttl_for_tier(&tier)
        .map(|s| (now + chrono::Duration::seconds(s)).to_rfc3339());

    let metadata = json!({
        "agent_id": sender.clone(),
        "recipient_agent_id": target,
        "message_kind": "notify",
    });

    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace: namespace.clone(),
        title: title.to_string(),
        content: payload.to_string(),
        tags: vec!["_message".to_string()],
        priority,
        confidence: 1.0,
        source: "notify".to_string(),
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
        metadata,
    };
    let actual_id = db::insert(conn, &mem).map_err(|e| e.to_string())?;

    Ok(json!({
        "id": actual_id,
        "from": sender,
        "to": target,
        "namespace": namespace,
        "tier": mem.tier,
        "delivered_at": mem.created_at,
    }))
}

fn handle_inbox(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    // Caller identity is the default inbox owner — agents read their own
    // inbox unless an explicit agent_id is supplied.
    let explicit = params["agent_id"].as_str();
    let owner =
        crate::identity::resolve_agent_id(explicit, mcp_client).map_err(|e| e.to_string())?;
    let unread_only = params["unread_only"].as_bool().unwrap_or(false);
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(50))
        .unwrap_or(usize::MAX)
        .min(500);
    let namespace = messages_namespace_for(&owner);
    let items = db::list(
        conn,
        Some(&namespace),
        None,
        limit,
        0,
        None,
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    let filtered: Vec<&Memory> = items
        .iter()
        .filter(|m| !unread_only || m.access_count == 0)
        .collect();
    let messages: Vec<Value> = filtered
        .iter()
        .map(|m| {
            let sender = m
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            json!({
                "id": m.id,
                "from": sender,
                "title": m.title,
                "payload": m.content,
                "priority": m.priority,
                "tier": m.tier,
                "created_at": m.created_at,
                "read": m.access_count > 0,
                "access_count": m.access_count,
            })
        })
        .collect();
    Ok(json!({
        "agent_id": owner,
        "namespace": namespace,
        "count": messages.len(),
        "unread_only": unread_only,
        "messages": messages,
    }))
}

// --- v0.6.0.0 webhook subscriptions ---------------------------------------

fn handle_subscribe(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let url = params["url"].as_str().ok_or("url is required")?;
    let events = params["events"].as_str().unwrap_or("*");
    let secret = params["secret"].as_str();
    let namespace_filter = params["namespace_filter"].as_str();
    let agent_filter = params["agent_filter"].as_str();
    let created_by =
        crate::identity::resolve_agent_id(None, mcp_client).map_err(|e| e.to_string())?;

    // Require the caller to be a registered agent (#301 item 4).
    // MCP stdio is single-tenant per process, but the same tool set is
    // exposed on the HTTP daemon where a caller might not be attested.
    // Registration in `_agents` is cheap (single memory_agent_register
    // call) and provides an audit trail; refusing unregistered
    // subscribers closes the "any MCP client owns the webhook fleet"
    // hole flagged by the v0.6.0 security review.
    let registered = crate::db::list_agents(conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|a| a.agent_id == created_by);
    if !registered {
        return Err(format!(
            "agent {created_by:?} is not registered; call memory_agent_register before memory_subscribe"
        ));
    }

    crate::subscriptions::validate_url(url).map_err(|e| e.to_string())?;

    let id = crate::subscriptions::insert(
        conn,
        &crate::subscriptions::NewSubscription {
            url,
            events,
            secret,
            namespace_filter,
            agent_filter,
            created_by: Some(&created_by),
        },
    )
    .map_err(|e| e.to_string())?;

    Ok(json!({
        "id": id,
        "url": url,
        "events": events,
        "namespace_filter": namespace_filter,
        "agent_filter": agent_filter,
        "created_by": created_by,
    }))
}

fn handle_unsubscribe(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    let removed = crate::subscriptions::delete(conn, id).map_err(|e| e.to_string())?;
    Ok(json!({"id": id, "removed": removed}))
}

fn handle_list_subscriptions(conn: &rusqlite::Connection) -> Result<Value, String> {
    let subs = crate::subscriptions::list(conn).map_err(|e| e.to_string())?;
    Ok(json!({"count": subs.len(), "subscriptions": subs}))
}

fn handle_pending_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let status = params["status"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(100))
        .unwrap_or(usize::MAX)
        .min(1000);
    let items = db::list_pending_actions(conn, status, limit).map_err(|e| e.to_string())?;
    Ok(json!({"count": items.len(), "pending": items}))
}

fn handle_pending_approve(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    use crate::db::ApproveOutcome;
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
        .map_err(|e| e.to_string())?;
    match db::approve_with_approver_type(conn, id, &agent_id).map_err(|e| e.to_string())? {
        ApproveOutcome::Approved => {
            // Task 1.10: auto-execute the queued action on final approval.
            let executed = db::execute_pending_action(conn, id).map_err(|e| e.to_string())?;
            Ok(json!({
                "approved": true,
                "id": id,
                "decided_by": agent_id,
                "executed": true,
                "memory_id": executed,
            }))
        }
        ApproveOutcome::Pending { votes, quorum } => Ok(json!({
            "approved": false,
            "status": "pending",
            "id": id,
            "votes": votes,
            "quorum": quorum,
            "reason": "consensus threshold not yet reached",
        })),
        ApproveOutcome::Rejected(reason) => Err(format!("approve rejected: {reason}")),
    }
}

fn handle_pending_reject(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
        .map_err(|e| e.to_string())?;
    let transitioned =
        db::decide_pending_action(conn, id, false, &agent_id).map_err(|e| e.to_string())?;
    if !transitioned {
        return Err(format!("pending action not found or already decided: {id}"));
    }
    Ok(json!({"rejected": true, "id": id, "decided_by": agent_id}))
}

fn handle_archive_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(50)).unwrap_or(usize::MAX);
    let offset = usize::try_from(params["offset"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX);
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
    resolved_scoring: &crate::config::ResolvedScoring,
    archive_on_gc: bool,
    autonomous_hooks: bool,
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
                    db_path,
                    arguments,
                    embedder,
                    llm,
                    vector_index,
                    resolved_ttl,
                    autonomous_hooks,
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
                    resolved_scoring,
                ),
                "memory_search" => handle_search(conn, arguments),
                "memory_list" => handle_list(conn, arguments),
                "memory_delete" => handle_delete(conn, arguments, vector_index, mcp_client),
                "memory_promote" => handle_promote(conn, arguments, mcp_client),
                "memory_pending_list" => handle_pending_list(conn, arguments),
                "memory_pending_approve" => handle_pending_approve(conn, arguments, mcp_client),
                "memory_pending_reject" => handle_pending_reject(conn, arguments, mcp_client),
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
                "memory_agent_register" => handle_agent_register(conn, arguments),
                "memory_agent_list" => handle_agent_list(conn),
                "memory_notify" => handle_notify(conn, arguments, resolved_ttl, mcp_client),
                "memory_inbox" => handle_inbox(conn, arguments, mcp_client),
                "memory_subscribe" => handle_subscribe(conn, arguments, mcp_client),
                "memory_unsubscribe" => handle_unsubscribe(conn, arguments),
                "memory_list_subscriptions" => handle_list_subscriptions(conn),
                // Ultrareview #349: unknown tool is a JSON-RPC 2.0
                // "method not found" condition — return -32601, not
                // an ok_response with `isError: true`. Clients that
                // switch on error code can then misroute / retry
                // correctly. We surface the tool name in `data` so
                // clients can log it without parsing the message.
                unknown => {
                    return err_response(id, -32601, format!("unknown tool: {unknown}"));
                }
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
        let resolved_scoring = app_config.effective_scoring();
        let archive_on_gc = app_config.effective_archive_on_gc();
        let autonomous_hooks = app_config.effective_autonomous_hooks();
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
            &resolved_scoring,
            archive_on_gc,
            autonomous_hooks,
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
    fn tool_definitions_returns_36_tools() {
        // v0.6.0.0 adds memory_notify + memory_inbox + memory_subscribe
        // + memory_unsubscribe + memory_list_subscriptions on top of the
        // 31 baseline.
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 36);
    }

    #[test]
    fn tool_definitions_include_agent_register_and_list() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"memory_agent_register"));
        assert!(names.contains(&"memory_agent_list"));
    }

    #[test]
    fn tool_definitions_include_notify_and_inbox() {
        // v0.6.0.0 agent-to-agent messaging primitive.
        let defs = tool_definitions();
        let names: Vec<&str> = defs["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"memory_notify"));
        assert!(names.contains(&"memory_inbox"));
    }

    #[test]
    fn messages_namespace_is_prefixed() {
        assert_eq!(super::messages_namespace_for("alice"), "_messages/alice");
        assert_eq!(
            super::messages_namespace_for("ai:claude-opus-4.7"),
            "_messages/ai:claude-opus-4.7"
        );
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
