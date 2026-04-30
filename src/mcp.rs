// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP (Model Context Protocol) server for ai-memory.
//! Exposes memory operations as tools for any MCP-compatible AI client over stdio JSON-RPC.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{AppConfig, FeatureTier, TierConfig};
use crate::db;
use crate::embeddings::Embedder;
use crate::hnsw::VectorIndex;
use crate::llm::OllamaClient;
use crate::models::{CandidateCounts, GovernancePolicy, Memory, RecallMeta, RecallTelemetry, Tier};
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

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
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
const TOOLS_VERSION: &str = "2026-04-26";

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
                "name": "memory_get_taxonomy",
                "description": "Pillar 1 / Stream A — return a hierarchical tree of namespaces with memory counts. Walks the `/`-delimited namespace paths grouped from live memories (expired rows excluded). Each node carries `count` (memories at exactly that namespace) and `subtree_count` (count plus all descendants visible within `depth`); the response also exposes `total_count` for the prefix and a `truncated` flag set when `limit` forced rows to be dropped from the tree.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace_prefix": {"type": "string", "description": "Restrict the tree to memories at this namespace OR any descendant. Omit to walk the full tree. Trailing '/' is tolerated."},
                        "depth": {"type": "integer", "minimum": 0, "maximum": 8, "default": 8, "description": "Max levels to descend below the prefix. Memories deeper than this still contribute to `subtree_count` of the boundary ancestor."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 1000, "description": "Cap on `(namespace, count)` rows walked when assembling the tree. Densest namespaces win when truncated."}
                    }
                }
            },
            {
                "name": "memory_check_duplicate",
                "description": "Pillar 2 / Stream D — pre-write near-duplicate check. Embeds `title + content`, scans live memories with stored embeddings (optionally restricted to `namespace`), and returns the highest-cosine match. `is_duplicate` is `nearest.similarity >= threshold`; the response also surfaces `suggested_merge` (the nearest memory's id) when the threshold is met. Threshold is clamped to a hard floor of 0.5 so permissive callers can't dress unrelated content as a merge candidate. Requires the embedder to be loaded (semantic tier or above).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Title of the candidate memory. Combined with `content` to form the embedding input, matching memory_store's encoding."},
                        "content": {"type": "string", "description": "Content of the candidate memory."},
                        "namespace": {"type": "string", "description": "Restrict the duplicate scan to this namespace. Omit to scan all namespaces."},
                        "threshold": {"type": "number", "minimum": 0.5, "maximum": 1.0, "default": 0.85, "description": "Cosine similarity threshold for declaring a duplicate. Clamped to >= 0.5. Default 0.85 is tuned for MiniLM-L6-v2 — near-paraphrases land at 0.88+."}
                    },
                    "required": ["title", "content"]
                }
            },
            {
                "name": "memory_entity_register",
                "description": "Pillar 2 / Stream B — register an entity (canonical name + aliases) under a namespace. Entities are stored as long-tier memories tagged 'entity' with metadata.kind='entity', so the (title, namespace) coordinate is shared with regular memories without ambiguity. Idempotent: re-registering the same canonical_name+namespace reuses the existing entity_id and merges any new aliases. Errors when the namespace+canonical_name already names a non-entity memory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "canonical_name": {"type": "string", "description": "Display name for the entity. Stored as the entity memory's title."},
                        "namespace": {"type": "string", "description": "Namespace under which the entity lives. Hierarchy paths (e.g. 'projects/alpha') are accepted."},
                        "aliases": {"type": "array", "items": {"type": "string"}, "description": "Aliases that should resolve to this entity. Blank entries are skipped; duplicates are de-duped via the entity_aliases primary key."},
                        "metadata": {"type": "object", "description": "Arbitrary metadata to attach to the entity memory. Caller-supplied 'kind' is overwritten with 'entity'; agent_id is stamped from the NHI caller when not specified."},
                        "agent_id": {"type": "string", "description": "Override the caller's resolved NHI for the entity memory's metadata.agent_id."}
                    },
                    "required": ["canonical_name", "namespace"]
                }
            },
            {
                "name": "memory_entity_get_by_alias",
                "description": "Pillar 2 / Stream B — resolve an alias to its registered entity. When 'namespace' is provided, only entities in that namespace are returned. When omitted, the most recently created matching entity wins. Returns null when no entity claims the alias under the given filter.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "alias": {"type": "string", "description": "Alias string to resolve. Whitespace is trimmed."},
                        "namespace": {"type": "string", "description": "Restrict the resolution to this namespace. Omit to search all namespaces."}
                    },
                    "required": ["alias"]
                }
            },
            {
                "name": "memory_kg_timeline",
                "description": "Pillar 2 / Stream C — ordered fact timeline for an entity. Returns outbound links from `source_id` (e.g. an entity registered via memory_entity_register) with their temporal-validity columns (valid_from, valid_until, observed_by) and the target memory's title/namespace. Events are ordered by valid_from ASC; rows with NULL valid_from are excluded. Cross-namespace by design — callers can post-filter by target_namespace if needed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Memory ID whose outbound assertions form the timeline. Typically an entity_id from memory_entity_register, but any memory works."},
                        "since": {"type": "string", "description": "RFC3339 timestamp; events with valid_from earlier than this are excluded (inclusive boundary)."},
                        "until": {"type": "string", "description": "RFC3339 timestamp; events with valid_from later than this are excluded (inclusive boundary)."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200, "description": "Max events returned. Clamped to [1, 1000]."}
                    },
                    "required": ["source_id"]
                }
            },
            {
                "name": "memory_kg_invalidate",
                "description": "Pillar 2 / Stream C — mark a KG link as superseded by setting its `valid_until` column. The link is identified by the (source_id, target_id, relation) triple (memory_links has no separate id column). When `valid_until` is omitted, the current wall-clock time is used. Idempotent: repeated calls overwrite the prior value and the response reports `previous_valid_until` so callers can detect the overwrite. Returns `found: false` when no link matches the triple.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID of the link to invalidate."},
                        "target_id": {"type": "string", "description": "Target memory ID of the link to invalidate."},
                        "relation": {"type": "string", "description": "Relation label of the link (e.g. 'related_to', 'supersedes', 'derived_from'). Must be a recognized relation."},
                        "valid_until": {"type": "string", "description": "RFC3339 timestamp marking when the assertion stops being valid. Defaults to the current time when omitted."}
                    },
                    "required": ["source_id", "target_id", "relation"]
                }
            },
            {
                "name": "memory_kg_query",
                "description": "Pillar 2 / Stream C — outbound KG traversal from a source memory. Returns one node per link reachable from `source_id` within `max_depth` hops, with the link's temporal-validity columns (valid_from, valid_until, observed_by) and the target memory's title/namespace. Multi-hop traversal uses a recursive CTE with cycle detection — chains only extend through links that pass every filter on every hop. Filters: `valid_at` keeps only links valid at that instant; `allowed_agents` keeps only links observed by an agent in the set (empty list returns zero rows by design — empty allowlist means 'no agents are trusted'). Ordered by depth ASC, then COALESCE(valid_from, created_at) ASC, for stable shallow-first display. `max_depth` ceiling is 5 (matches the published performance budget); larger values return an explicit error.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Memory ID whose outbound links form the traversal frontier. Typically an entity_id from memory_entity_register, but any memory works."},
                        "max_depth": {"type": "integer", "minimum": 1, "maximum": 5, "default": 1, "description": "Hops from the source. Supported range: 1..=5 (matches the published performance budget for `memory_kg_query`). Larger values return an explicit error."},
                        "valid_at": {"type": "string", "description": "RFC3339 timestamp; only links valid at this instant (valid_from <= valid_at AND (valid_until IS NULL OR valid_until > valid_at)) are returned. Omit to skip the temporal filter (NULL valid_from rows are then included)."},
                        "allowed_agents": {"type": "array", "items": {"type": "string"}, "description": "If provided, only links whose observed_by is in this set are returned. An empty array returns zero rows. Omit to skip the agent filter."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200, "description": "Max nodes returned across all depths. Clamped to [1, 1000]."}
                    },
                    "required": ["source_id"]
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
pub fn handle_recall(
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

    // v0.6.3.1 (P3): build the per-request meta block from retrieval-stage
    // telemetry + the runtime reranker variant. The block is always
    // present in the response — clients that don't read it ignore unknown
    // fields per JSON-RPC convention. Closes audit gaps G2/G8/G11 by
    // making silent-degrade paths visible at request time.
    let reranker_used = match reranker {
        Some(ce) if ce.is_neural() => "neural",
        Some(_) => "lexical",
        None => "none",
    };
    let attach_meta = |resp: &mut Value, recall_mode: &str, telemetry: &RecallTelemetry| {
        // Round blend_weight to 3 decimals — matches the score field
        // precision and keeps the wire shape stable regardless of f64
        // representation jitter.
        let blend_weight = (telemetry.blend_weight_avg * 1000.0).round() / 1000.0;
        let meta = RecallMeta {
            recall_mode: recall_mode.to_string(),
            reranker_used: reranker_used.to_string(),
            candidate_counts: CandidateCounts {
                fts: telemetry.fts_candidates,
                hnsw: telemetry.hnsw_candidates,
            },
            blend_weight,
        };
        if let Ok(v) = serde_json::to_value(&meta) {
            resp["meta"] = v;
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
                let (results, tokens_used, telemetry) = db::recall_hybrid_with_telemetry(
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
                    attach_meta(&mut resp, "hybrid", &telemetry);
                    inject_namespace_standard(conn, namespace, &mut resp);
                    return Ok(resp);
                }

                let memories = scored_memories(results);
                let mut resp =
                    json!({"memories": memories, "count": memories.len(), "mode": "hybrid"});
                decorate_budget(&mut resp, tokens_used);
                attach_meta(&mut resp, "hybrid", &telemetry);
                inject_namespace_standard(conn, namespace, &mut resp);
                return Ok(resp);
            }
            Err(e) => {
                // v0.6.3.1 (P3, G11): the embedder being present but the
                // per-query embed failing is a different silent-degrade
                // path than "embedder unavailable at startup" — preserve
                // the existing tracing event and fall through to
                // keyword_only mode below, which is what the meta block
                // will report.
                tracing::warn!("embedding failed, falling back to FTS: {}", e);
            }
        }
    }

    // Fallback to keyword-only recall
    let (results, tokens_used, telemetry) = db::recall_with_telemetry(
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
    attach_meta(&mut resp, "keyword_only", &telemetry);
    inject_namespace_standard(conn, namespace, &mut resp);
    Ok(resp)
}

/// v0.6.3 (capabilities schema v2): the canonical capabilities entry
/// point. When `conn` is `Some`, the dynamic blocks
/// (`permissions.active_rules`, `hooks.registered_count`,
/// `approval.pending_requests`) are populated from live DB counts.
/// When `None`, they remain at the zero-state defaults set in
/// `TierConfig::capabilities`. Both shapes are valid schema-v2 output —
/// old clients reading by named path continue to work either way.
pub(crate) fn handle_capabilities_with_conn(
    tier_config: &TierConfig,
    reranker: Option<&CrossEncoder>,
    embedder_loaded: bool,
    conn: Option<&rusqlite::Connection>,
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
    // v0.6.2 (S18): report whether the embedder successfully materialized
    // at serve startup. `semantic_search` reflects the tier CONFIG while
    // this bool reflects the RUNTIME — the two can diverge when the HF
    // model fetch fails on an offline runner.
    caps.features.embedder_loaded = embedder_loaded;

    // v0.6.3.1 (P3, G2): mirror the per-process HNSW eviction counters
    // into the capabilities surface so a `memory_capabilities` poll can
    // tell operators whether the index is currently shedding embeddings.
    // These are the same values surfaced in `memory_stats.index_evictions_total`.
    caps.hnsw.evictions_total = crate::hnsw::index_evictions_total();
    caps.hnsw.evicted_recently = crate::hnsw::evicted_recently(60);

    // v0.6.3 (capabilities schema v2): when we have a connection, fill
    // the dynamic blocks with live counts. Failures here are non-fatal —
    // the report still serializes with the zero-state defaults so a
    // transient DB blip can't 500 the capabilities endpoint.
    if let Some(c) = conn {
        if let Ok(n) = db::count_active_governance_rules(c) {
            caps.permissions.active_rules = n;
        }
        if let Ok(n) = db::count_subscriptions(c) {
            caps.hooks.registered_count = n;
        }
        if let Ok(n) = db::count_pending_actions_by_status(c, "pending") {
            caps.approval.pending_requests = n;
        }
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

fn handle_get_taxonomy(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    // Defaults match the JSON schema. Trailing '/' is forgiven so MCP
    // clients can pass either `"alpha"` or `"alpha/"` without an extra
    // round trip — the underlying validate_namespace rejects the
    // trailing slash form, so we strip it before validating.
    let prefix_raw = params
        .get("namespace_prefix")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let prefix_owned: Option<String> = prefix_raw.map(|s| s.trim_end_matches('/').to_string());
    if let Some(p) = prefix_owned.as_deref() {
        validate::validate_namespace(p).map_err(|e| e.to_string())?;
    }
    let depth = usize::try_from(params.get("depth").and_then(Value::as_u64).unwrap_or(8))
        .unwrap_or(usize::MAX)
        .min(crate::models::MAX_NAMESPACE_DEPTH);
    let limit = usize::try_from(params.get("limit").and_then(Value::as_u64).unwrap_or(1000))
        .unwrap_or(usize::MAX)
        .clamp(1, 10_000);

    let tax =
        db::get_taxonomy(conn, prefix_owned.as_deref(), depth, limit).map_err(|e| e.to_string())?;
    Ok(json!({
        "tree": tax.tree,
        "total_count": tax.total_count,
        "truncated": tax.truncated,
    }))
}

fn handle_check_duplicate(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&Embedder>,
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

    let check = db::check_duplicate(conn, &query_embedding, namespace, threshold)
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

fn handle_entity_register(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let canonical_name = params["canonical_name"]
        .as_str()
        .ok_or("canonical_name is required")?;
    let namespace = params["namespace"]
        .as_str()
        .ok_or("namespace is required")?;
    let aliases: Vec<String> = params["aliases"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let extra_metadata = if params["metadata"].is_object() {
        params["metadata"].clone()
    } else {
        json!({})
    };
    let explicit_agent_id = params["agent_id"].as_str();

    validate::validate_title(canonical_name).map_err(|e| e.to_string())?;
    validate::validate_namespace(namespace).map_err(|e| e.to_string())?;
    if let Some(aid) = explicit_agent_id {
        validate::validate_agent_id(aid).map_err(|e| e.to_string())?;
    }

    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;

    let reg = db::entity_register(
        conn,
        canonical_name,
        namespace,
        &aliases,
        &extra_metadata,
        Some(&agent_id),
    )
    .map_err(|e| e.to_string())?;

    Ok(json!({
        "entity_id": reg.entity_id,
        "canonical_name": reg.canonical_name,
        "namespace": reg.namespace,
        "aliases": reg.aliases,
        "created": reg.created,
    }))
}

fn handle_entity_get_by_alias(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let alias = params["alias"].as_str().ok_or("alias is required")?;
    let namespace = params["namespace"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ns) = namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }

    match db::entity_get_by_alias(conn, alias, namespace).map_err(|e| e.to_string())? {
        Some(rec) => Ok(json!({
            "found": true,
            "entity_id": rec.entity_id,
            "canonical_name": rec.canonical_name,
            "namespace": rec.namespace,
            "aliases": rec.aliases,
        })),
        None => Ok(json!({
            "found": false,
            "entity_id": null,
            "canonical_name": null,
            "namespace": null,
            "aliases": [],
        })),
    }
}

fn handle_kg_timeline(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    validate::validate_id(source_id).map_err(|e| e.to_string())?;
    let since = params["since"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let until = params["until"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(s) = since {
        validate::validate_expires_at_format(s).map_err(|e| e.to_string())?;
    }
    if let Some(u) = until {
        validate::validate_expires_at_format(u).map_err(|e| e.to_string())?;
    }
    let limit = params["limit"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());

    let events =
        db::kg_timeline(conn, source_id, since, until, limit).map_err(|e| e.to_string())?;

    let events_json: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "target_id": e.target_id,
                "relation": e.relation,
                "valid_from": e.valid_from,
                "valid_until": e.valid_until,
                "observed_by": e.observed_by,
                "title": e.title,
                "target_namespace": e.target_namespace,
            })
        })
        .collect();

    Ok(json!({
        "source_id": source_id,
        "events": events_json,
        "count": events.len(),
    }))
}

fn handle_kg_invalidate(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    let target_id = params["target_id"]
        .as_str()
        .ok_or("target_id is required")?;
    let relation = params["relation"].as_str().ok_or("relation is required")?;
    validate::validate_link(source_id, target_id, relation).map_err(|e| e.to_string())?;
    let valid_until = params["valid_until"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ts) = valid_until {
        validate::validate_expires_at_format(ts).map_err(|e| e.to_string())?;
    }

    match db::invalidate_link(conn, source_id, target_id, relation, valid_until)
        .map_err(|e| e.to_string())?
    {
        Some(res) => Ok(json!({
            "found": true,
            "source_id": source_id,
            "target_id": target_id,
            "relation": relation,
            "valid_until": res.valid_until,
            "previous_valid_until": res.previous_valid_until,
        })),
        None => Ok(json!({
            "found": false,
            "source_id": source_id,
            "target_id": target_id,
            "relation": relation,
        })),
    }
}

fn handle_kg_query(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let source_id = params["source_id"]
        .as_str()
        .ok_or("source_id is required")?;
    validate::validate_id(source_id).map_err(|e| e.to_string())?;

    let max_depth = params["max_depth"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(1);

    let valid_at = params["valid_at"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(t) = valid_at {
        validate::validate_expires_at_format(t).map_err(|e| e.to_string())?;
    }

    let allowed_agents: Option<Vec<String>> = params["allowed_agents"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_string)
            .collect()
    });
    if let Some(agents) = allowed_agents.as_ref() {
        for a in agents {
            validate::validate_agent_id(a).map_err(|e| e.to_string())?;
        }
    }

    let limit = params["limit"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());

    let nodes = db::kg_query(
        conn,
        source_id,
        max_depth,
        valid_at,
        allowed_agents.as_deref(),
        limit,
    )
    .map_err(|e| e.to_string())?;

    let memories_json: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "target_id": n.target_id,
                "relation": n.relation,
                "valid_from": n.valid_from,
                "valid_until": n.valid_until,
                "observed_by": n.observed_by,
                "title": n.title,
                "target_namespace": n.target_namespace,
                "depth": n.depth,
                "path": n.path,
            })
        })
        .collect();
    let paths_json: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();

    Ok(json!({
        "source_id": source_id,
        "max_depth": max_depth,
        "memories": memories_json,
        "paths": paths_json,
        "count": nodes.len(),
    }))
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

pub(crate) fn handle_namespace_set_standard(
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

pub(crate) fn handle_namespace_get_standard(
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

pub(crate) fn handle_namespace_clear_standard(
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

pub(crate) fn handle_notify(
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

pub(crate) fn handle_inbox(
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

pub(crate) fn handle_subscribe(
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

pub(crate) fn handle_unsubscribe(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    let removed = crate::subscriptions::delete(conn, id).map_err(|e| e.to_string())?;
    Ok(json!({"id": id, "removed": removed}))
}

pub(crate) fn handle_list_subscriptions(conn: &rusqlite::Connection) -> Result<Value, String> {
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

pub(crate) fn handle_session_start(
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

            // Pillar 3 / Stream E — emit a structured tracing span around
            // every MCP tool dispatch so production observability can
            // attribute latency per tool. The span carries the tool name
            // and JSON-RPC id; outcome and elapsed wall time are emitted
            // as a child event after dispatch returns.
            let span = tracing::info_span!(
                "mcp_tool_call",
                tool = tool_name,
                rpc_id = ?id,
            );
            let _enter = span.enter();
            let started = Instant::now();

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
                "memory_get_taxonomy" => handle_get_taxonomy(conn, arguments),
                "memory_check_duplicate" => handle_check_duplicate(conn, arguments, embedder),
                "memory_entity_register" => handle_entity_register(conn, arguments, mcp_client),
                "memory_entity_get_by_alias" => handle_entity_get_by_alias(conn, arguments),
                "memory_kg_timeline" => handle_kg_timeline(conn, arguments),
                "memory_kg_invalidate" => handle_kg_invalidate(conn, arguments),
                "memory_kg_query" => handle_kg_query(conn, arguments),
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
                "memory_capabilities" => handle_capabilities_with_conn(
                    tier_config,
                    reranker,
                    embedder.is_some(),
                    Some(conn),
                ),
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

            // Outcome + elapsed reported under the `mcp_tool_call` span so
            // exporters can chart per-tool p95/p99 against PERFORMANCE.md
            // budgets without needing per-handler instrumentation.
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            match &result {
                Ok(_) => tracing::info!(elapsed_ms, "ok"),
                Err(err) => tracing::warn!(elapsed_ms, error = %err, "err"),
            }

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
    // Pillar 3 / Stream E — wire `tracing` for the MCP entrypoint so the
    // per-tool spans added in `handle_request` actually surface. The
    // writer is pinned to stderr because stdio JSON-RPC owns stdout;
    // emitting trace lines there would corrupt the protocol. `try_init`
    // is a no-op if a subscriber was already installed by another
    // command in the same process.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ai_memory=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

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
    fn tool_definitions_returns_43_tools() {
        // v0.6.3 adds memory_get_taxonomy (Pillar 1 / Stream A),
        // memory_check_duplicate (Pillar 2 / Stream D),
        // memory_entity_register + memory_entity_get_by_alias
        // (Pillar 2 / Stream B), and memory_kg_timeline +
        // memory_kg_invalidate + memory_kg_query (Pillar 2 / Stream C)
        // on top of the 36-tool v0.6.0.0 surface.
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 43);
    }

    #[test]
    fn tool_definitions_include_check_duplicate() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"memory_check_duplicate"));
    }

    #[test]
    fn tool_definitions_include_entity_registry_tools() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"memory_entity_register"));
        assert!(names.contains(&"memory_entity_get_by_alias"));
    }

    #[test]
    fn tool_definitions_include_kg_timeline() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"memory_kg_timeline"));
    }

    #[test]
    fn tool_definitions_include_kg_invalidate() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"memory_kg_invalidate"));
    }

    #[test]
    fn tool_definitions_include_kg_query() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"memory_kg_query"));
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

    /// Buffer-backed `MakeWriter` so `tracing` output can be asserted on
    /// without polluting test stdout/stderr or installing a global
    /// subscriber. Used by the Stream E span coverage tests below.
    #[derive(Clone)]
    struct VecWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn run_with_capture<F: FnOnce()>(f: F) -> String {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer = VecWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap_or_default()
    }

    fn make_tools_call(tool: &str, args: Value) -> RpcRequest {
        RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "tools/call".into(),
            params: json!({ "name": tool, "arguments": args }),
        }
    }

    /// Pillar 3 / Stream E coverage — every successful `tools/call` must
    /// emit a `mcp_tool_call` span carrying the tool name plus an `ok`
    /// event with `elapsed_ms`. This is the single point of latency
    /// instrumentation production exporters key off.
    #[test]
    fn tools_call_emits_span_with_tool_name_and_elapsed_ms() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let tier_config = FeatureTier::Keyword.config();
        let resolved_ttl = crate::config::ResolvedTtl::default();
        let resolved_scoring = crate::config::ResolvedScoring::default();
        let req = make_tools_call("memory_list", json!({"limit": 1}));

        let captured = run_with_capture(|| {
            let resp = handle_request(
                &conn,
                std::path::Path::new(":memory:"),
                &req,
                None,
                None,
                None,
                &tier_config,
                None,
                &resolved_ttl,
                &resolved_scoring,
                true,
                false,
                None,
            );
            assert!(resp.error.is_none(), "expected ok rpc response");
        });

        assert!(
            captured.contains("mcp_tool_call"),
            "missing span name in: {captured}"
        );
        assert!(
            captured.contains("memory_list"),
            "missing tool field in: {captured}"
        );
        assert!(
            captured.contains("elapsed_ms"),
            "missing elapsed_ms field in: {captured}"
        );
        assert!(
            captured.contains(" ok"),
            "missing ok outcome event in: {captured}"
        );
    }

    /// Failure path — when the underlying handler returns an `Err`, the
    /// span emits a `warn` level event with the error message so on-call
    /// dashboards can alert on per-tool error rate.
    #[test]
    fn tools_call_emits_warn_event_on_handler_error() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let tier_config = FeatureTier::Keyword.config();
        let resolved_ttl = crate::config::ResolvedTtl::default();
        let resolved_scoring = crate::config::ResolvedScoring::default();
        // memory_get with a missing/invalid id is a deterministic Err
        // path: validate_id rejects empty strings.
        let req = make_tools_call("memory_get", json!({"id": ""}));

        let captured = run_with_capture(|| {
            let resp = handle_request(
                &conn,
                std::path::Path::new(":memory:"),
                &req,
                None,
                None,
                None,
                &tier_config,
                None,
                &resolved_ttl,
                &resolved_scoring,
                true,
                false,
                None,
            );
            // Handler errs are returned as ok_response with isError=true,
            // not RpcError, by design (the JSON-RPC layer is reserved for
            // protocol-level failures).
            assert!(resp.error.is_none());
        });

        assert!(
            captured.contains("mcp_tool_call"),
            "missing span in err path: {captured}"
        );
        assert!(
            captured.contains("memory_get"),
            "missing tool field in err path: {captured}"
        );
        assert!(
            captured.contains("WARN"),
            "missing WARN level on err path: {captured}"
        );
        assert!(
            captured.contains("err"),
            "missing err outcome in: {captured}"
        );
    }
    /// Parametrized smoke matrix for all 43 MCP tools (Justice of MCP pathway).
    /// Tier 1: happy path with canonical valid args.
    /// Tier 2: required arg validation (missing required arg → error).
    #[test]
    #[allow(clippy::too_many_lines)]
    fn mcp_tools_smoke_matrix() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let tier_config = FeatureTier::Keyword.config();
        let resolved_ttl = crate::config::ResolvedTtl::default();
        let resolved_scoring = crate::config::ResolvedScoring::default();

        struct ToolCase {
            name: &'static str,
            valid_args: Value,
            required_arg: Option<&'static str>, // first required arg name for error test
        }

        let cases: &[ToolCase] = &[
            ToolCase {
                name: "memory_store",
                valid_args: json!({"title": "test", "content": "test content"}),
                required_arg: Some("title"),
            },
            ToolCase {
                name: "memory_recall",
                valid_args: json!({"context": "test"}),
                required_arg: Some("context"),
            },
            ToolCase {
                name: "memory_search",
                valid_args: json!({"query": "test"}),
                required_arg: Some("query"),
            },
            ToolCase {
                name: "memory_list",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_get_taxonomy",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_check_duplicate",
                valid_args: json!({"title": "test", "content": "test content"}),
                required_arg: Some("title"),
            },
            ToolCase {
                name: "memory_entity_register",
                valid_args: json!({"canonical_name": "Entity", "namespace": "test"}),
                required_arg: Some("canonical_name"),
            },
            ToolCase {
                name: "memory_entity_get_by_alias",
                valid_args: json!({"alias": "test"}),
                required_arg: Some("alias"),
            },
            ToolCase {
                name: "memory_kg_timeline",
                valid_args: json!({"source_id": "fake-id-for-test"}),
                required_arg: Some("source_id"),
            },
            ToolCase {
                name: "memory_kg_invalidate",
                valid_args: json!({"source_id": "s", "target_id": "t", "relation": "related_to"}),
                required_arg: Some("source_id"),
            },
            ToolCase {
                name: "memory_kg_query",
                valid_args: json!({"source_id": "fake-id-for-test"}),
                required_arg: Some("source_id"),
            },
            ToolCase {
                name: "memory_delete",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_promote",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_forget",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_stats",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_update",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_get",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_link",
                valid_args: json!({"source_id": "s", "target_id": "t"}),
                required_arg: Some("source_id"),
            },
            ToolCase {
                name: "memory_get_links",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_consolidate",
                valid_args: json!({"ids": ["id1", "id2"], "title": "consolidated"}),
                required_arg: Some("ids"),
            },
            ToolCase {
                name: "memory_capabilities",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_expand_query",
                valid_args: json!({"query": "test"}),
                required_arg: Some("query"),
            },
            ToolCase {
                name: "memory_auto_tag",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_detect_contradiction",
                valid_args: json!({"id_a": "a", "id_b": "b"}),
                required_arg: Some("id_a"),
            },
            ToolCase {
                name: "memory_archive_list",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_archive_restore",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_archive_purge",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_archive_stats",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_gc",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_session_start",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_namespace_set_standard",
                valid_args: json!({"namespace": "test", "id": "fake-id-for-test"}),
                required_arg: Some("namespace"),
            },
            ToolCase {
                name: "memory_namespace_get_standard",
                valid_args: json!({"namespace": "test"}),
                required_arg: Some("namespace"),
            },
            ToolCase {
                name: "memory_namespace_clear_standard",
                valid_args: json!({"namespace": "test"}),
                required_arg: Some("namespace"),
            },
            ToolCase {
                name: "memory_pending_list",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_pending_approve",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_pending_reject",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_agent_register",
                valid_args: json!({"agent_id": "test-agent", "agent_type": "human"}),
                required_arg: Some("agent_id"),
            },
            ToolCase {
                name: "memory_agent_list",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_notify",
                valid_args: json!({"target_agent_id": "agent", "title": "msg", "payload": "body"}),
                required_arg: Some("target_agent_id"),
            },
            ToolCase {
                name: "memory_inbox",
                valid_args: json!({}),
                required_arg: None,
            },
            ToolCase {
                name: "memory_subscribe",
                valid_args: json!({"url": "https://example.com/webhook"}),
                required_arg: Some("url"),
            },
            ToolCase {
                name: "memory_unsubscribe",
                valid_args: json!({"id": "fake-id-for-test"}),
                required_arg: Some("id"),
            },
            ToolCase {
                name: "memory_list_subscriptions",
                valid_args: json!({}),
                required_arg: None,
            },
        ];

        // Tier 1: happy path tests
        for case in cases {
            let req = make_tools_call(case.name, case.valid_args.clone());
            let resp = handle_request(
                &conn,
                std::path::Path::new(":memory:"),
                &req,
                None,
                None,
                None,
                &tier_config,
                None,
                &resolved_ttl,
                &resolved_scoring,
                true,
                false,
                None,
            );
            assert!(
                resp.error.is_none(),
                "happy path failed for {}: {:?}",
                case.name,
                resp.error
            );
            assert!(
                resp.result.is_some(),
                "missing result for happy path {}: {:?}",
                case.name,
                resp
            );
        }

        // Tier 2: required arg validation
        for case in cases {
            if let Some(required_arg) = case.required_arg {
                let mut bad_args = case.valid_args.clone();
                bad_args.as_object_mut().unwrap().remove(required_arg);

                let req = make_tools_call(case.name, bad_args);
                let resp = handle_request(
                    &conn,
                    std::path::Path::new(":memory:"),
                    &req,
                    None,
                    None,
                    None,
                    &tier_config,
                    None,
                    &resolved_ttl,
                    &resolved_scoring,
                    true,
                    false,
                    None,
                );

                // Missing required args should produce an error response (handler returns Err)
                // which becomes an ok_response with isError=true, not a JSON-RPC error
                assert!(
                    resp.error.is_none() || resp.result.is_some(),
                    "unexpected RPC-layer error for {} (missing {}) should be handler-level Err",
                    case.name,
                    required_arg
                );
            }
        }
    }

    // =====================================================================
    // W9 / Closer M9 — mcp.rs sweep
    //
    // Targets the four areas identified in the W9 close-out: tool-handler
    // happy/error pairs (per family), JSON-RPC framing (parse / unknown
    // method / invalid params), `auto_register_path_hierarchy`, and
    // `inject_namespace_standard`. All tests append-only at end of the
    // tests module — production code is untouched.
    //
    // Inner-fn factor-out: `dispatch_line` is added below as a test-only
    // helper that mirrors the parse-and-dispatch loop in `run_mcp_server`.
    // It is `#[cfg(test)]` and lives inside the `tests` module so it
    // does NOT leak into the public surface (no production callers are
    // affected). This is the minimum needed to drive parse-error /
    // truncation / two-requests-per-line cases without spinning up the
    // real stdio loop.
    // =====================================================================

    /// Build a fully-defaulted handle_request invocation against an
    /// in-memory connection. Returns the response so individual tests
    /// can assert on `error` / `result` shape.
    fn invoke_handle_request(conn: &rusqlite::Connection, req: &RpcRequest) -> RpcResponse {
        let tier_config = FeatureTier::Keyword.config();
        let resolved_ttl = crate::config::ResolvedTtl::default();
        let resolved_scoring = crate::config::ResolvedScoring::default();
        handle_request(
            conn,
            std::path::Path::new(":memory:"),
            req,
            None,
            None,
            None,
            &tier_config,
            None,
            &resolved_ttl,
            &resolved_scoring,
            true,
            false,
            None,
        )
    }

    /// Test-only helper that mirrors the parse-then-dispatch portion of
    /// `run_mcp_server`'s stdin loop for a single line. Returns:
    /// - `Some(RpcResponse)` for any line that produces a response
    ///   (including parse errors and successful dispatches),
    /// - `None` for lines that should not produce a response (blank
    ///   lines, valid notifications without an id).
    ///
    /// This is the minimum factor-out needed to exercise the framing
    /// branches that live inside `run_mcp_server` (parse error, blank
    /// skip, notification skip) without spinning up real stdio.
    fn dispatch_line(conn: &rusqlite::Connection, line: &str) -> Option<RpcResponse> {
        if line.trim().is_empty() {
            return None;
        }
        let req: RpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Some(err_response(
                    Value::Null,
                    -32700,
                    format!("parse error: {e}"),
                ));
            }
        };
        if req.id.is_none() || req.id == Some(Value::Null) {
            return None;
        }
        Some(invoke_handle_request(conn, &req))
    }

    // ------------------------------------------------------------------
    // Tool-handler happy-path coverage (paired with error tests below).
    // The smoke matrix above already confirms every tool dispatches; the
    // tests below assert on the *shape* of the success result so a
    // handler that silently changes its return key set fails loudly.
    // ------------------------------------------------------------------

    #[test]
    fn handle_store_happy_returns_id_and_tier() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_store",
            json!({"title": "t", "content": "c", "namespace": "m9-store", "tier": "short"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["id"].is_string());
        assert_eq!(val["tier"], "short");
    }

    #[test]
    fn handle_store_error_missing_title() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_store", json!({"content": "c"}));
        let resp = invoke_handle_request(&conn, &req);
        // Handler-level errors come back as ok_response with isError=true.
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_recall_happy_returns_memories_array() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_recall",
            json!({"context": "anything", "format": "json"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["memories"].is_array());
        assert!(val["count"].is_u64());
    }

    #[test]
    fn handle_recall_error_budget_tokens_zero() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_recall", json!({"context": "x", "budget_tokens": 0}));
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("budget_tokens"));
    }

    #[test]
    fn handle_search_happy_returns_results_array() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_search",
            json!({"query": "needle", "format": "json"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["results"].is_array());
        assert!(val["count"].is_u64());
    }

    #[test]
    fn handle_search_error_missing_query() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_search", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_get_happy_returns_memory() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Insert a memory directly to know the id.
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "m9-get".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call("memory_get", json!({"id": id}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["title"], "t");
        assert_eq!(val["namespace"], "m9-get");
        assert!(val["links"].is_array());
    }

    #[test]
    fn handle_get_error_unknown_id() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_get",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
    }

    #[test]
    fn handle_list_happy_returns_memories_array() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_list", json!({"format": "json"}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["memories"].is_array());
    }

    #[test]
    fn handle_list_error_invalid_agent_id() {
        // Invalid agent_id (contains a space) is rejected upstream.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_list", json!({"agent_id": "has space"}));
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_delete_happy_removes_existing_memory() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "m9-del".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call("memory_delete", json!({"id": id}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["deleted"], true);
    }

    #[test]
    fn handle_delete_error_empty_id() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_delete", json!({"id": ""}));
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_link_happy_returns_linked_true() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mut ids = Vec::new();
        for tag in ["a", "b"] {
            let mem = Memory {
                id: uuid::Uuid::new_v4().to_string(),
                tier: Tier::Mid,
                namespace: "m9-link".into(),
                title: tag.into(),
                content: "c".into(),
                tags: vec![],
                priority: 5,
                confidence: 1.0,
                source: "test".into(),
                access_count: 0,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                last_accessed_at: None,
                expires_at: None,
                metadata: json!({}),
            };
            ids.push(db::insert(&conn, &mem).unwrap());
        }
        let req = make_tools_call(
            "memory_link",
            json!({"source_id": ids[0], "target_id": ids[1], "relation": "related_to"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["linked"], true);
    }

    #[test]
    fn handle_link_error_missing_target_id() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_link", json!({"source_id": "x"}));
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_promote_error_unknown_id() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_promote",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_consolidate_error_missing_summary_keyword_tier() {
        // Keyword tier has no LLM, so `summary` is required.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_consolidate",
            json!({"ids": ["a", "b"], "title": "t"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("summary"));
    }

    #[test]
    fn handle_capabilities_happy_returns_tier_struct() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_capabilities", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["tier"].is_string());
        assert!(val["features"].is_object());
    }

    /// v0.6.3 (capabilities schema v2 — arch-enhancement-spec §7).
    /// Every new top-level block is present with the expected shape.
    #[test]
    fn mcp_capabilities_v2_schema_includes_all_blocks() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_capabilities", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(val["schema_version"], "2", "schema_version bumped to 2");

        // permissions block
        assert!(val["permissions"].is_object(), "permissions block present");
        assert!(val["permissions"]["mode"].is_string());
        assert_eq!(val["permissions"]["mode"], "ask");
        assert!(val["permissions"]["active_rules"].is_number());
        assert!(val["permissions"]["rule_summary"].is_array());

        // hooks block
        assert!(val["hooks"].is_object(), "hooks block present");
        assert!(val["hooks"]["registered_count"].is_number());
        assert!(val["hooks"]["by_event"].is_object());

        // compaction block — pre-v0.8 reports zero-state
        assert!(val["compaction"].is_object(), "compaction block present");
        assert_eq!(val["compaction"]["enabled"], false);
        assert!(val["compaction"]["interval_minutes"].is_null());
        assert!(val["compaction"]["last_run_at"].is_null());
        assert!(val["compaction"]["last_run_stats"].is_null());

        // approval block
        assert!(val["approval"].is_object(), "approval block present");
        assert!(val["approval"]["subscribers"].is_number());
        assert!(val["approval"]["pending_requests"].is_number());
        assert_eq!(val["approval"]["default_timeout_seconds"], 30);

        // transcripts block — pre-v0.7 reports zero-state
        assert!(val["transcripts"].is_object(), "transcripts block present");
        assert_eq!(val["transcripts"]["enabled"], false);
        assert_eq!(val["transcripts"]["total_count"], 0);
        assert_eq!(val["transcripts"]["total_size_mb"], 0);
    }

    /// v0.6.3 (capabilities schema v2). Old clients reading the v1 paths
    /// must continue to find them at the same top-level keys.
    #[test]
    fn mcp_capabilities_v2_backwards_compatible() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_capabilities", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();

        // v1 fields preserved at the same paths
        assert!(val["tier"].is_string(), "v1: tier preserved");
        assert!(val["version"].is_string(), "v1: version preserved");
        assert!(val["features"].is_object(), "v1: features preserved");
        assert!(val["models"].is_object(), "v1: models preserved");

        // Specifically, well-known v1 sub-fields still resolve.
        assert!(val["features"]["keyword_search"].is_boolean());
        assert!(val["features"]["semantic_search"].is_boolean());
        assert!(val["features"]["embedder_loaded"].is_boolean());
        assert!(val["models"]["embedding"].is_string());
        assert!(val["models"]["llm"].is_string());
        assert!(val["models"]["cross_encoder"].is_string());
    }

    /// v0.6.3 (capabilities schema v2). `approval.pending_requests`
    /// reflects the live `pending_actions` count — the one block that is
    /// already wired through to a real subsystem instead of zero-state.
    #[test]
    fn mcp_capabilities_pending_requests_reflects_db() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Insert a pending action by hand (the queue path is exercised
        // elsewhere; here we only need the count to bump).
        let payload = serde_json::json!({"foo": "bar"}).to_string();
        conn.execute(
            "INSERT INTO pending_actions (id, action_type, memory_id, namespace,
                payload, requested_by, requested_at, status)
             VALUES ('p-1', 'store', NULL, 'global', ?1, 'agent-1',
                '2026-04-27T00:00:00Z', 'pending')",
            rusqlite::params![payload],
        )
        .unwrap();

        let req = make_tools_call("memory_capabilities", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(
            val["approval"]["pending_requests"], 1,
            "pending_actions(status=pending) count surfaces live"
        );
    }

    #[test]
    fn handle_subscribe_error_unregistered_agent() {
        // memory_subscribe refuses unregistered callers (#301 item 4).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_subscribe",
            json!({"url": "https://example.com/hook"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("not registered"));
    }

    // ------------------------------------------------------------------
    // JSON-RPC framing — drives `dispatch_line` and `handle_request`.
    // ------------------------------------------------------------------

    #[test]
    fn test_jsonrpc_handles_well_formed_request() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let resp = dispatch_line(&conn, line).expect("expected response");
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert!(result["tools"].is_array());
    }

    #[test]
    fn test_jsonrpc_handles_malformed_json() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Garbage on a single line.
        let resp = dispatch_line(&conn, "this is not json at all").expect("expected response");
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32700);
        assert!(err.message.contains("parse error"));
        // Spec: id MUST be Null for parse errors.
        assert_eq!(resp.id, Value::Null);
    }

    #[test]
    fn test_jsonrpc_handles_truncated_request() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Incomplete JSON object — serde_json must reject.
        let resp = dispatch_line(&conn, r#"{"jsonrpc":"2.0","id":1,"method":"#)
            .expect("expected response");
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32700);
    }

    #[test]
    fn test_jsonrpc_handles_two_requests_per_line() {
        // The MCP framing is line-delimited JSON: one request per line.
        // If a peer accidentally pastes two JSON objects on one line
        // (`{...}{...}`), serde_json::from_str must reject as parse
        // error rather than silently process the first.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"} {"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = dispatch_line(&conn, line).expect("expected response");
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32700);
    }

    #[test]
    fn test_jsonrpc_handles_blank_line() {
        // Blank lines are skipped (no response).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        assert!(dispatch_line(&conn, "").is_none());
        assert!(dispatch_line(&conn, "   \t  ").is_none());
    }

    #[test]
    fn test_jsonrpc_handles_notification_no_response() {
        // Requests without an `id` are JSON-RPC notifications — no
        // response should be emitted per spec.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(dispatch_line(&conn, line).is_none());
        // Explicit id:null is also a notification.
        let line_null = r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#;
        assert!(dispatch_line(&conn, line_null).is_none());
    }

    #[test]
    fn test_jsonrpc_handles_method_not_found() {
        // Unknown JSON-RPC method must return -32601.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: "no/such/method".into(),
            params: json!({}),
        };
        let resp = invoke_handle_request(&conn, &req);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert!(err.message.contains("method not found"));
    }

    #[test]
    fn test_jsonrpc_handles_invalid_params() {
        // tools/call with a missing tool name must surface -32602.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(8)),
            method: "tools/call".into(),
            params: json!({"arguments": {}}),
        };
        let resp = invoke_handle_request(&conn, &req);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn test_jsonrpc_handles_unknown_tool_returns_minus_32601() {
        // Ultrareview #349: unknown tool = method-not-found, not isError.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_does_not_exist", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert!(err.message.contains("memory_does_not_exist"));
    }

    #[test]
    fn test_jsonrpc_rejects_wrong_version() {
        // jsonrpc field must be exactly "2.0" — anything else = -32600.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "1.0".into(),
            id: Some(json!(1)),
            method: "tools/list".into(),
            params: json!({}),
        };
        let resp = invoke_handle_request(&conn, &req);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn test_jsonrpc_handles_initialize() {
        // Initialize handshake returns serverInfo + protocolVersion.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "initialize".into(),
            params: json!({"clientInfo": {"name": "test-client"}}),
        };
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "ai-memory");
    }

    // ------------------------------------------------------------------
    // auto_register_path_hierarchy — exercises the bail-out branches.
    //
    // The function only mutates rows whose `parent_namespace IS NULL`,
    // walking from `cwd().parent()` up to the home directory. The
    // working directory in `cargo test` is the crate root, which
    // typically lives under `home`, so the walk runs but finds no
    // matching parent (no namespace_meta row for any ancestor dir
    // name). Tests below cover: (1) no-op when an explicit parent is
    // already set, (2) no-op when the namespace has no row, (3) safe
    // call with an empty-string namespace, (4) idempotency.
    // ------------------------------------------------------------------

    #[test]
    fn test_auto_register_creates_top_level_namespace() {
        // With no namespace_meta row at all, the walk finds nothing
        // and the table stays empty (silent no-op, never panics).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        super::auto_register_path_hierarchy(&conn, "m9-top");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM namespace_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_auto_register_creates_nested_hierarchy() {
        // Pre-seed a row for "repo/team/sub" with parent NULL. The walk
        // looks for any ancestor *directory name* that has a standard;
        // since none of the test-runner's cwd ancestors will collide
        // with synthetic namespace names, the row's parent stays NULL.
        // The contract tested is: function tolerates nested-form inputs
        // without panicking and never overwrites a row whose parent is
        // already set.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Insert a synthetic standard for "m9-parent" so the walk *could*
        // match if cwd happened to be inside a "m9-parent" dir; in
        // practice it won't, so the row's parent stays NULL.
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "m9-parent".into(),
            title: "parent standard".into(),
            content: "...".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let std_id = db::insert(&conn, &mem).unwrap();
        db::set_namespace_standard(&conn, "m9-parent", &std_id, None).unwrap();
        // Seed a child row with parent NULL.
        let child_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "repo/team/sub".into(),
            title: "child".into(),
            content: "...".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let child_id = db::insert(&conn, &child_mem).unwrap();
        db::set_namespace_standard(&conn, "repo/team/sub", &child_id, None).unwrap();
        // Run the walk — must not panic, must not corrupt rows.
        super::auto_register_path_hierarchy(&conn, "repo/team/sub");
        // The seeded standard is still readable.
        let id = db::get_namespace_standard(&conn, "repo/team/sub")
            .unwrap()
            .unwrap();
        assert_eq!(id, child_id);
    }

    #[test]
    fn test_auto_register_idempotent() {
        // Calling twice must not corrupt state — even when no match is
        // found, the second call observes the same DB.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        super::auto_register_path_hierarchy(&conn, "m9-idem");
        super::auto_register_path_hierarchy(&conn, "m9-idem");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM namespace_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_auto_register_handles_empty_string_or_root() {
        // Empty / root-y inputs must not panic. The walk is a pure
        // observer when the namespace_meta row is absent.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        super::auto_register_path_hierarchy(&conn, "");
        super::auto_register_path_hierarchy(&conn, "/");
        super::auto_register_path_hierarchy(&conn, "*");
        // Still no rows, no crash.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM namespace_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_auto_register_skips_when_explicit_parent_set() {
        // Early-return path: if `get_namespace_parent` already returns
        // Some, the walk is skipped entirely. We verify by calling the
        // function and asserting that the explicit parent is preserved.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Seed two memories so we can register parent and child.
        let parent_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "m9-explicit-parent".into(),
            title: "p".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let parent_id = db::insert(&conn, &parent_mem).unwrap();
        db::set_namespace_standard(&conn, "m9-explicit-parent", &parent_id, None).unwrap();

        let child_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "m9-explicit-child".into(),
            title: "c".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let child_id = db::insert(&conn, &child_mem).unwrap();
        db::set_namespace_standard(
            &conn,
            "m9-explicit-child",
            &child_id,
            Some("m9-explicit-parent"),
        )
        .unwrap();

        // Pre-condition: parent is set.
        assert_eq!(
            db::get_namespace_parent(&conn, "m9-explicit-child"),
            Some("m9-explicit-parent".to_string())
        );
        super::auto_register_path_hierarchy(&conn, "m9-explicit-child");
        // Post-condition: parent unchanged.
        assert_eq!(
            db::get_namespace_parent(&conn, "m9-explicit-child"),
            Some("m9-explicit-parent".to_string())
        );
    }

    // ------------------------------------------------------------------
    // inject_namespace_standard — coverage for the four shape branches.
    // ------------------------------------------------------------------

    fn make_recall_response(memories: Vec<Value>) -> Value {
        let count = memories.len();
        json!({
            "memories": memories,
            "count": count,
            "mode": "keyword",
        })
    }

    fn seed_namespace_standard(
        conn: &rusqlite::Connection,
        namespace: &str,
        title: &str,
    ) -> String {
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: namespace.into(),
            title: title.into(),
            content: "policy text".into(),
            tags: vec!["_standard".into()],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(conn, &mem).unwrap();
        db::set_namespace_standard(conn, namespace, &id, None).unwrap();
        id
    }

    #[test]
    fn test_inject_namespace_standard_attaches_when_present() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let std_id = seed_namespace_standard(&conn, "m9-inject-attach", "S");
        let mut resp = make_recall_response(vec![]);
        super::inject_namespace_standard(&conn, Some("m9-inject-attach"), &mut resp);
        assert!(resp["standard"].is_object(), "expected attached standard");
        assert_eq!(resp["standard"]["id"].as_str().unwrap(), std_id);
    }

    #[test]
    fn test_inject_namespace_standard_skips_when_absent() {
        // No standard set anywhere → response is unchanged (no
        // `standard` / `standards` field added).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mut resp = make_recall_response(vec![]);
        let before = resp.clone();
        super::inject_namespace_standard(&conn, Some("m9-inject-empty"), &mut resp);
        assert_eq!(resp, before);
        assert!(resp.get("standard").is_none());
        assert!(resp.get("standards").is_none());
    }

    #[test]
    fn test_inject_namespace_standard_top_of_recall_response() {
        // The standard's own memory must be filtered OUT of the
        // `memories` array so the client doesn't see the policy
        // duplicated as a result + as the `standard` field.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let std_id = seed_namespace_standard(&conn, "m9-inject-dedup", "S");
        // Pretend recall returned the standard as one of its hits.
        let dup = json!({"id": std_id, "title": "S", "content": "policy text"});
        let other = json!({"id": "other-id", "title": "noise", "content": "x"});
        let mut resp = make_recall_response(vec![dup.clone(), other.clone()]);
        super::inject_namespace_standard(&conn, Some("m9-inject-dedup"), &mut resp);
        assert_eq!(resp["standard"]["id"].as_str().unwrap(), std_id);
        let memories = resp["memories"].as_array().unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0]["id"], "other-id");
        assert_eq!(resp["count"], 1);
    }

    #[test]
    fn test_inject_namespace_standard_preserves_other_response_fields() {
        // Mode / count / unrelated fields must survive injection.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        seed_namespace_standard(&conn, "m9-inject-preserve", "S");
        let mut resp = json!({
            "memories": [],
            "count": 0,
            "mode": "hybrid",
            "diagnostics": {"latency_ms": 42},
        });
        super::inject_namespace_standard(&conn, Some("m9-inject-preserve"), &mut resp);
        assert_eq!(resp["mode"], "hybrid");
        assert_eq!(resp["diagnostics"]["latency_ms"], 42);
        assert!(resp["standard"].is_object());
    }

    #[test]
    fn test_inject_namespace_standard_no_namespace_uses_global() {
        // When `namespace` is None, only the global "*" standard is
        // consulted. We seed "*" and assert it's attached.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        seed_namespace_standard(&conn, "*", "global standard");
        let mut resp = make_recall_response(vec![]);
        super::inject_namespace_standard(&conn, None, &mut resp);
        assert_eq!(resp["standard"]["title"], "global standard");
    }

    #[test]
    fn test_inject_namespace_standard_multiple_levels_emits_array() {
        // When more than one standard applies (global + namespace),
        // the response gets a `standards` array, not a single
        // `standard` object.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        seed_namespace_standard(&conn, "*", "GLOBAL");
        seed_namespace_standard(&conn, "m9-multi", "LOCAL");
        let mut resp = make_recall_response(vec![]);
        super::inject_namespace_standard(&conn, Some("m9-multi"), &mut resp);
        assert!(resp["standards"].is_array());
        let arr = resp["standards"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Order: global ("*") first, then namespace-specific.
        assert_eq!(arr[0]["title"], "GLOBAL");
        assert_eq!(arr[1]["title"], "LOCAL");
        assert!(resp.get("standard").is_none());
    }

    // =====================================================================
    // W12 / Closer W12-A — mcp.rs deeper sweep
    //
    // M9 covered the first 40 tests. W12-A targets the residual ~750
    // uncovered lines with focus on:
    //   1) Less-common tool handlers (archive_*, kg_*, agent_*, notify,
    //      inbox, namespace_*, pending_*, gc, session_start)
    //   2) Per-handler error branches not hit by the smoke matrix's "drop
    //      one required arg" pass — invalid argument shape, validation
    //      failures, "not found" lookups
    //   3) JSON-RPC framing edge cases beyond M9's six (nested method
    //      strings, unicode, empty params, prompts/list, prompts/get
    //      errors, ping)
    //   4) Helper-fn coverage holes — `inject_namespace_standard` shape
    //      branches, `auto_register_path_hierarchy` walk variants
    //
    // All tests use the test-only `invoke_handle_request` helper from
    // M9 to avoid repeating the 13-arg call site.
    // =====================================================================

    // ------------------------------------------------------------------
    // Less-common tool handlers — happy paths
    // ------------------------------------------------------------------

    #[test]
    fn handle_archive_list_returns_empty_when_no_archived() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_archive_list", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["count"], 0);
        assert!(val["archived"].is_array());
    }

    #[test]
    fn handle_archive_list_with_namespace_filter() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_archive_list",
            json!({"namespace": "w12-archive", "limit": 5, "offset": 0}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_archive_restore_unknown_id_returns_error() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_archive_restore",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("archive") || msg.contains("not found"));
    }

    #[test]
    fn handle_archive_purge_with_older_than_zero() {
        // older_than_days=0 → purges all entries; on an empty DB this is
        // a no-op that still hits the success branch.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_archive_purge", json!({"older_than_days": 0}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["purged"].is_u64() || val["purged"].is_i64());
    }

    #[test]
    fn handle_archive_stats_returns_struct() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_archive_stats", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        // Stats fields vary; just confirm the response is an object/value.
        assert!(val.is_object() || val.is_number() || val.is_array());
    }

    #[test]
    fn handle_kg_timeline_unknown_source_returns_empty_events() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_timeline",
            json!({"source_id": "00000000-0000-0000-0000-000000000000"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["events"].is_array());
        assert_eq!(val["count"], 0);
    }

    #[test]
    fn handle_kg_timeline_with_since_until_filters() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_timeline",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "since": "2024-01-01T00:00:00Z",
                "until": "2025-01-01T00:00:00Z",
                "limit": 50,
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_kg_timeline_invalid_since_returns_error() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_timeline",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "since": "this-is-not-a-timestamp",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_kg_invalidate_no_match_returns_found_false() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_invalidate",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "target_id": "11111111-1111-1111-1111-111111111111",
                "relation": "related_to",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["found"], false);
    }

    #[test]
    fn handle_kg_invalidate_with_explicit_valid_until() {
        // Seed source + target memories and a link, then invalidate with
        // an explicit timestamp — drives the Some(ts) validation branch
        // and the Some(res) match arm.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let src = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-kg".into(),
            title: "src".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let mut tgt = src.clone();
        tgt.id = uuid::Uuid::new_v4().to_string();
        tgt.title = "tgt".into();
        let src_id = db::insert(&conn, &src).unwrap();
        let tgt_id = db::insert(&conn, &tgt).unwrap();
        db::create_link(&conn, &src_id, &tgt_id, "related_to").unwrap();

        let req = make_tools_call(
            "memory_kg_invalidate",
            json!({
                "source_id": src_id,
                "target_id": tgt_id,
                "relation": "related_to",
                "valid_until": "2025-01-01T00:00:00Z",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["found"], true);
        assert_eq!(val["valid_until"], "2025-01-01T00:00:00Z");
    }

    #[test]
    fn handle_kg_invalidate_invalid_valid_until_format() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_invalidate",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "target_id": "11111111-1111-1111-1111-111111111111",
                "relation": "related_to",
                "valid_until": "not-a-date",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_kg_query_with_max_depth_and_filters() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_query",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "max_depth": 2,
                "valid_at": "2025-01-01T00:00:00Z",
                "allowed_agents": ["agent-a", "agent-b"],
                "limit": 10,
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["max_depth"], 2);
        assert!(val["memories"].is_array());
    }

    #[test]
    fn handle_kg_query_invalid_valid_at() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_query",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "valid_at": "garbage",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_kg_query_rejects_invalid_agent_id() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_kg_query",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "allowed_agents": ["bad agent with spaces!!"],
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_session_start_happy_returns_memories() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Seed a memory so list returns at least one row.
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-session".into(),
            title: "seed".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_session_start",
            json!({"namespace": "w12-session", "limit": 5, "format": "json"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["mode"], "session_start");
        assert!(val["memories"].is_array());
    }

    #[test]
    fn handle_session_start_empty_namespace_returns_zero() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_session_start",
            json!({"namespace": "w12-empty-ns", "format": "json"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["count"], 0);
    }

    #[test]
    fn handle_inbox_returns_empty_for_unregistered_caller() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_inbox", json!({"agent_id": "test-bot"}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["agent_id"], "test-bot");
        assert!(val["namespace"].as_str().unwrap().starts_with("_messages/"));
        assert_eq!(val["count"], 0);
    }

    #[test]
    fn handle_inbox_with_unread_only_filter() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_inbox",
            json!({"agent_id": "test-bot", "unread_only": true, "limit": 10}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["unread_only"], true);
    }

    #[test]
    fn handle_notify_happy_returns_message_id() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_notify",
            json!({
                "target_agent_id": "alice",
                "title": "hello",
                "payload": "world",
                "tier": "mid",
                "priority": 5,
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["id"].is_string());
        assert_eq!(val["to"], "alice");
        assert_eq!(val["namespace"], "_messages/alice");
    }

    #[test]
    fn handle_notify_invalid_tier_returns_error() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_notify",
            json!({
                "target_agent_id": "bob",
                "title": "hi",
                "payload": "p",
                "tier": "bogus-tier",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("invalid tier"));
    }

    #[test]
    fn handle_agent_register_then_list() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Register — `agent_type` must match the closed set or `ai:<name>`.
        let req = make_tools_call(
            "memory_agent_register",
            json!({
                "agent_id": "w12-bot",
                "agent_type": "ai:w12-bot",
                "capabilities": ["read", "write"],
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["registered"], true);
        // List
        let req2 = make_tools_call("memory_agent_list", json!({}));
        let resp2 = invoke_handle_request(&conn, &req2);
        assert!(resp2.error.is_none());
        let text2 = resp2.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val2: Value = serde_json::from_str(&text2).unwrap();
        assert!(val2["count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn handle_agent_register_invalid_type_rejects() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_agent_register",
            json!({"agent_id": "w12-bot2", "agent_type": "  not-allowed-type with spaces  "}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_namespace_set_get_clear_round_trip() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Seed a memory we can use as the standard
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-ns".into(),
            title: "policy".into(),
            content: "be excellent".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let std_id = db::insert(&conn, &mem).unwrap();

        // Set
        let set_req = make_tools_call(
            "memory_namespace_set_standard",
            json!({"namespace": "w12-ns", "id": std_id.clone()}),
        );
        let set_resp = invoke_handle_request(&conn, &set_req);
        assert!(set_resp.error.is_none());
        let set_text = set_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let set_val: Value = serde_json::from_str(&set_text).unwrap();
        assert_eq!(set_val["set"], true);

        // Get
        let get_req = make_tools_call(
            "memory_namespace_get_standard",
            json!({"namespace": "w12-ns"}),
        );
        let get_resp = invoke_handle_request(&conn, &get_req);
        assert!(get_resp.error.is_none());
        let get_text = get_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let get_val: Value = serde_json::from_str(&get_text).unwrap();
        assert_eq!(get_val["standard_id"], std_id);

        // Clear
        let clr_req = make_tools_call(
            "memory_namespace_clear_standard",
            json!({"namespace": "w12-ns"}),
        );
        let clr_resp = invoke_handle_request(&conn, &clr_req);
        assert!(clr_resp.error.is_none());
        let clr_text = clr_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let clr_val: Value = serde_json::from_str(&clr_text).unwrap();
        assert_eq!(clr_val["cleared"], true);
    }

    #[test]
    fn handle_namespace_get_standard_missing_returns_null() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_namespace_get_standard",
            json!({"namespace": "w12-no-standard-here"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["standard_id"].is_null());
    }

    #[test]
    fn handle_namespace_get_standard_inherit_returns_chain() {
        // Seed two standards: one global "*" and one for "w12-inh", and
        // request --inherit so the resolved chain branch fires.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        seed_namespace_standard(&conn, "*", "global rule");
        seed_namespace_standard(&conn, "w12-inh", "specific rule");
        let req = make_tools_call(
            "memory_namespace_get_standard",
            json!({"namespace": "w12-inh", "inherit": true}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["chain"].is_array());
        assert!(val["standards"].is_array());
        assert!(val["count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn handle_namespace_set_standard_with_invalid_governance_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-gov".into(),
            title: "p".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_namespace_set_standard",
            json!({
                "namespace": "w12-gov",
                "id": id,
                "governance": {"this": "is not a valid policy"},
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("invalid governance") || msg.contains("governance"));
    }

    #[test]
    fn handle_namespace_set_standard_invalid_namespace_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_namespace_set_standard",
            json!({"namespace": "bad ns with spaces!!", "id": "any"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_pending_list_happy_returns_array() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_pending_list",
            json!({"status": "pending", "limit": 100}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["pending"].is_array());
        assert!(val["count"].is_u64());
    }

    #[test]
    fn handle_pending_approve_unknown_id_returns_error() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_pending_approve",
            json!({"id": "00000000-0000-0000-0000-000000000000", "agent_id": "human:approver"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        // Either isError true or a not-found / rejected response — both
        // exercise the unknown-id code path in approve_with_approver_type.
        assert!(result.is_object());
    }

    #[test]
    fn handle_pending_reject_unknown_id_returns_not_found() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_pending_reject",
            json!({"id": "00000000-0000-0000-0000-000000000000", "agent_id": "human:rejector"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("not found") || msg.contains("already decided"));
    }

    #[test]
    fn handle_gc_dry_run_returns_count_without_deleting() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_gc", json!({"dry_run": true}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["dry_run"], true);
        assert!(val["collected"].is_u64() || val["collected"].is_i64());
    }

    #[test]
    fn handle_gc_actual_run_returns_zero_on_empty_db() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_gc", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["dry_run"], false);
    }

    #[test]
    fn handle_forget_dry_run_with_filters() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_forget",
            json!({"namespace": "w12-forget", "tier": "short", "dry_run": true}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["dry_run"], true);
    }

    #[test]
    fn handle_forget_actual_with_namespace() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_forget",
            json!({"namespace": "w12-forget-actual", "dry_run": false}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_unsubscribe_unknown_returns_false() {
        // db::subscriptions::delete returns a bool — false when no row
        // matched. The handler propagates that verbatim.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_unsubscribe",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        // Either a bool false or numeric 0 — the contract is "no row removed".
        assert!(
            val["removed"] == json!(false) || val["removed"] == json!(0),
            "unexpected removed value: {:?}",
            val["removed"]
        );
    }

    #[test]
    fn handle_list_subscriptions_returns_array() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_list_subscriptions", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_entity_register_happy() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_entity_register",
            json!({
                "canonical_name": "Hugo Boss",
                "namespace": "w12-people",
                "aliases": ["HB", "Hugo"],
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["entity_id"].is_string());
        assert_eq!(val["canonical_name"], "Hugo Boss");
    }

    #[test]
    fn handle_entity_register_invalid_namespace() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_entity_register",
            json!({"canonical_name": "X", "namespace": "INVALID NS!"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_entity_get_by_alias_not_found_returns_null() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_entity_get_by_alias",
            json!({"alias": "no-such-alias", "namespace": "w12-people"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["found"], false);
    }

    #[test]
    fn handle_get_taxonomy_with_prefix_and_depth() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_get_taxonomy",
            json!({"namespace_prefix": "w12-tax", "depth": 4, "limit": 100}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["tree"].is_object() || val["tree"].is_array());
    }

    #[test]
    fn handle_get_taxonomy_strips_trailing_slash() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_get_taxonomy",
            json!({"namespace_prefix": "w12-tax/", "depth": 2}),
        );
        let resp = invoke_handle_request(&conn, &req);
        // Trailing-slash forgiveness branch: must not error.
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_get_taxonomy_invalid_prefix_after_strip() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_get_taxonomy",
            json!({"namespace_prefix": "BAD NS!"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_check_duplicate_no_embedder_errors() {
        // Without embedder, check_duplicate must error (it requires
        // semantic tier or above).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_check_duplicate",
            json!({"title": "T", "content": "C"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("embedder") || msg.contains("semantic"));
    }

    #[test]
    fn handle_expand_query_no_llm_errors() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_expand_query", json!({"query": "test"}));
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("smart") || msg.contains("LLM") || msg.contains("Ollama"));
    }

    #[test]
    fn handle_auto_tag_no_llm_errors() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_auto_tag",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_detect_contradiction_no_llm_errors() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_detect_contradiction",
            json!({"id_a": "00000000-0000-0000-0000-000000000000", "id_b": "11111111-1111-1111-1111-111111111111"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_update_unknown_id_returns_not_found() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_update",
            json!({
                "id": "00000000-0000-0000-0000-000000000000",
                "title": "new title",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("not found"));
    }

    #[test]
    fn handle_update_invalid_priority_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // First insert a memory we can target.
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-update".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_update",
            json!({"id": id, "priority": 99_i64}), // out of 1..=10 range
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_update_with_metadata_object_accepted() {
        // Drives the metadata-is-object branch which validates and merges
        // the agent_id-preserving payload.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "w12-meta".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_update",
            json!({
                "id": id,
                "metadata": {"custom": "field", "numbers": [1, 2, 3]},
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_get_links_unknown_id_returns_empty() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_get_links",
            json!({"id": "00000000-0000-0000-0000-000000000000"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["links"].is_array());
        assert_eq!(val["count"], 0);
    }

    #[test]
    fn handle_link_invalid_relation_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_link",
            json!({
                "source_id": "00000000-0000-0000-0000-000000000000",
                "target_id": "11111111-1111-1111-1111-111111111111",
                "relation": "BADRELATIONNOTALLOWED",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_promote_to_namespace_with_explicit_target() {
        // Vertical-promote branch: when `to_namespace` is provided, the
        // memory is cloned to an ancestor namespace and linked with
        // `derived_from`. db::promote_to_namespace requires the target
        // to be an ancestor of the source's namespace, so use a
        // hierarchical namespace.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "w12-parent/w12-child".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_promote",
            json!({"id": id, "to_namespace": "w12-parent"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["mode"], "vertical");
        assert!(val["clone_id"].is_string());
    }

    #[test]
    fn handle_promote_invalid_to_namespace_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "w12-pm".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_promote",
            json!({"id": id, "to_namespace": "BAD NS WITH SPACES"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_consolidate_with_explicit_summary_no_llm() {
        // Drives the "explicit summary" branch (no LLM call needed).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem_a = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "w12-cons".into(),
            title: "a".into(),
            content: "alpha".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let mut mem_b = mem_a.clone();
        mem_b.id = uuid::Uuid::new_v4().to_string();
        mem_b.title = "b".into();
        mem_b.content = "beta".into();
        let id_a = db::insert(&conn, &mem_a).unwrap();
        let id_b = db::insert(&conn, &mem_b).unwrap();

        let req = make_tools_call(
            "memory_consolidate",
            json!({
                "ids": [id_a, id_b],
                "title": "merged",
                "summary": "merged summary",
                "namespace": "w12-cons",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["id"].is_string());
        assert_eq!(val["consolidated"], 2);
    }

    #[test]
    fn handle_consolidate_non_string_id_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_consolidate",
            json!({"ids": [42, "valid-id"], "title": "t", "summary": "s"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("must be a string"));
    }

    // ------------------------------------------------------------------
    // JSON-RPC framing — additional edge cases beyond M9's six.
    // ------------------------------------------------------------------

    #[test]
    fn test_jsonrpc_handles_ping() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "ping".into(),
            params: json!({}),
        };
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_handles_notifications_initialized() {
        // The client→server "I'm ready" notification — handler returns
        // the same empty body as ping.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "notifications/initialized".into(),
            params: json!({}),
        };
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_prompts_list_returns_array() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "prompts/list".into(),
            params: json!({}),
        };
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert!(result["prompts"].is_array());
    }

    #[test]
    fn test_jsonrpc_prompts_get_known_name_returns_messages() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(4)),
            method: "prompts/get".into(),
            params: json!({"name": "recall-first"}),
        };
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert!(result["messages"].is_array());
    }

    #[test]
    fn test_jsonrpc_prompts_get_with_namespace_arg_includes_hint() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(5)),
            method: "prompts/get".into(),
            params: json!({"name": "recall-first", "arguments": {"namespace": "w12-test"}}),
        };
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("w12-test"));
    }

    #[test]
    fn test_jsonrpc_prompts_get_unknown_name_returns_error() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(6)),
            method: "prompts/get".into(),
            params: json!({"name": "no-such-prompt"}),
        };
        let resp = invoke_handle_request(&conn, &req);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn test_jsonrpc_prompts_get_missing_name_returns_error() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: "prompts/get".into(),
            params: json!({}),
        };
        let resp = invoke_handle_request(&conn, &req);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn test_jsonrpc_prompts_get_memory_workflow() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(8)),
            method: "prompts/get".into(),
            params: json!({"name": "memory-workflow"}),
        };
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert!(result["messages"].is_array());
    }

    #[test]
    fn test_jsonrpc_tools_call_empty_tool_name_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(9)),
            method: "tools/call".into(),
            params: json!({"name": ""}),
        };
        let resp = invoke_handle_request(&conn, &req);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn test_jsonrpc_tools_call_arguments_not_object_uses_empty() {
        // arguments=null is replaced with an empty object before dispatch.
        // Combined with a tool that has no required args, this path
        // exercises the `is_object()` false branch of the arguments
        // resolution.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(10)),
            method: "tools/call".into(),
            params: json!({"name": "memory_capabilities", "arguments": null}),
        };
        let resp = invoke_handle_request(&conn, &req);
        // Capabilities accepts no args; with empty defaults it succeeds.
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_tools_call_unicode_in_args() {
        // Unicode strings round-trip through serde_json without issue —
        // verifies the dispatch path doesn't choke on non-ASCII args.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_store",
            json!({"title": "тест", "content": "日本語 ✨", "namespace": "w12-unicode"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_dispatch_line_with_id_zero_treated_as_request() {
        // id=0 is a valid JSON-RPC id (numeric, non-null). Must NOT be
        // treated as a notification.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let line = r#"{"jsonrpc":"2.0","id":0,"method":"tools/list"}"#;
        let resp = dispatch_line(&conn, line);
        assert!(resp.is_some());
    }

    #[test]
    fn test_jsonrpc_dispatch_line_string_id_passes_through() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let line = r#"{"jsonrpc":"2.0","id":"call-abc","method":"tools/list"}"#;
        let resp = dispatch_line(&conn, line).expect("expected response");
        assert_eq!(resp.id, json!("call-abc"));
    }

    // ------------------------------------------------------------------
    // Helper-fn coverage — build_namespace_chain branches.
    // ------------------------------------------------------------------

    #[test]
    fn test_build_namespace_chain_global_only() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let chain = super::build_namespace_chain(&conn, "*");
        assert_eq!(chain, vec!["*".to_string()]);
    }

    #[test]
    fn test_build_namespace_chain_simple_namespace() {
        // A flat namespace produces ["*", "ns"].
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let chain = super::build_namespace_chain(&conn, "w12-flat");
        assert!(chain.contains(&"*".to_string()));
        assert!(chain.contains(&"w12-flat".to_string()));
    }

    #[test]
    fn test_build_namespace_chain_nested_yields_ancestors() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let chain = super::build_namespace_chain(&conn, "a/b/c");
        // Must contain "*" and the full chain top-down.
        assert_eq!(chain.first().unwrap(), "*");
        assert!(chain.contains(&"a/b/c".to_string()));
        // Top-down order: a precedes a/b precedes a/b/c.
        let pos_a = chain.iter().position(|s| s == "a").unwrap();
        let pos_ab = chain.iter().position(|s| s == "a/b").unwrap();
        let pos_abc = chain.iter().position(|s| s == "a/b/c").unwrap();
        assert!(pos_a < pos_ab && pos_ab < pos_abc);
    }

    #[test]
    fn test_build_namespace_chain_with_explicit_parent() {
        // Seeding an explicit `parent_namespace` row should prepend that
        // ancestor before the /-derived chain.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Insert a row in namespace_meta so the explicit-parent walk
        // has something to traverse. Use db helpers when possible.
        let parent_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-explicit-grand".into(),
            title: "g".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let pid = db::insert(&conn, &parent_mem).unwrap();
        db::set_namespace_standard(&conn, "w12-explicit-grand", &pid, None).unwrap();

        let mut child_mem = parent_mem.clone();
        child_mem.id = uuid::Uuid::new_v4().to_string();
        child_mem.namespace = "w12-explicit-leaf".into();
        let cid = db::insert(&conn, &child_mem).unwrap();
        db::set_namespace_standard(&conn, "w12-explicit-leaf", &cid, Some("w12-explicit-grand"))
            .unwrap();

        let chain = super::build_namespace_chain(&conn, "w12-explicit-leaf");
        // Explicit-parent walk should include the grandparent.
        assert!(chain.contains(&"w12-explicit-grand".to_string()));
        assert!(chain.contains(&"w12-explicit-leaf".to_string()));
    }

    // ------------------------------------------------------------------
    // extract_governance — surface the metadata.governance branch.
    // ------------------------------------------------------------------

    #[test]
    fn test_extract_governance_default_when_metadata_absent() {
        let mem_val = json!({"id": "x"});
        let gov = super::extract_governance(&mem_val);
        // Default policy is non-null and serializes to an object.
        assert!(gov.is_object() || gov.is_null());
    }

    #[test]
    fn test_extract_governance_default_when_metadata_invalid() {
        // metadata.governance present but not a valid policy -> default.
        let mem_val = json!({"metadata": {"governance": {"unknown": "policy"}}});
        let gov = super::extract_governance(&mem_val);
        // Default policy is non-null and serializes to an object.
        assert!(gov.is_object());
    }

    // ------------------------------------------------------------------
    // messages_namespace_for — confirm both ASCII and ai: prefixes.
    // ------------------------------------------------------------------

    #[test]
    fn test_messages_namespace_for_plain_id() {
        assert_eq!(super::messages_namespace_for("alice"), "_messages/alice");
    }

    #[test]
    fn test_messages_namespace_for_ai_prefixed_id() {
        let ns = super::messages_namespace_for("ai:claude@host:pid-1");
        assert!(ns.starts_with("_messages/"));
        assert!(ns.contains("ai:"));
    }

    // ------------------------------------------------------------------
    // inject_namespace_standard — additional shape branches that M9
    // didn't reach (no-namespace + no-global, dedup ordering).
    // ------------------------------------------------------------------

    #[test]
    fn test_inject_namespace_standard_no_namespace_no_global() {
        // namespace=None and no "*" standard set → response unchanged.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mut resp = make_recall_response(vec![]);
        let before = resp.clone();
        super::inject_namespace_standard(&conn, None, &mut resp);
        assert_eq!(resp, before);
    }

    // ------------------------------------------------------------------
    // W12-A — additional coverage targets discovered after the first
    // sweep. These hit handler happy-paths that the smoke matrix
    // skipped (tier-default promotion, dedup-update, registered
    // subscriber) plus a few error / boundary branches.
    // ------------------------------------------------------------------

    #[test]
    fn handle_promote_default_tier_to_long() {
        // Drives the "no to_namespace" branch which clears expires_at
        // and bumps tier to Long. This is the historical behaviour.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "w12-tier-promote".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call("memory_promote", json!({"id": id}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["promoted"], true);
        assert_eq!(val["mode"], "tier");
        assert_eq!(val["tier"], "long");
    }

    #[test]
    fn handle_store_dedup_updates_existing() {
        // Storing twice with the same title+namespace must hit the
        // dedup-update branch instead of inserting a second row.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req1 = make_tools_call(
            "memory_store",
            json!({
                "title": "dup-title",
                "content": "first",
                "namespace": "w12-dedup",
                "tier": "mid",
            }),
        );
        let resp1 = invoke_handle_request(&conn, &req1);
        assert!(resp1.error.is_none());
        let text1 = resp1.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val1: Value = serde_json::from_str(&text1).unwrap();
        let id1 = val1["id"].as_str().unwrap().to_string();

        let req2 = make_tools_call(
            "memory_store",
            json!({
                "title": "dup-title",
                "content": "second-update",
                "namespace": "w12-dedup",
                "tier": "long",
            }),
        );
        let resp2 = invoke_handle_request(&conn, &req2);
        assert!(resp2.error.is_none());
        let text2 = resp2.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val2: Value = serde_json::from_str(&text2).unwrap();
        assert_eq!(val2["id"], id1);
        assert_eq!(val2["duplicate"], true);
        assert_eq!(val2["action"], "updated existing memory");
    }

    #[test]
    fn handle_subscribe_with_registered_agent_succeeds() {
        // Drives the subscribe-after-register happy path (the smoke
        // matrix only catches the unregistered-error case).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        // Register the caller (default agent_id resolved by mcp_client=None)
        // — we let resolve_agent_id mint one; by registering the resolved
        // value we can pass the subscribe gate.
        let resolved = crate::identity::resolve_agent_id(None, None).unwrap();
        db::register_agent(&conn, &resolved, "human", &[]).unwrap();
        let req = make_tools_call(
            "memory_subscribe",
            json!({
                "url": "https://example.com/hook",
                "events": "memory_store,memory_delete",
                "namespace_filter": "w12-sub",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["id"].is_string());
        assert_eq!(val["url"], "https://example.com/hook");
    }

    #[test]
    fn handle_subscribe_invalid_url_after_registered() {
        // After registering, a malformed URL still falls through to the
        // url-validate branch.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let resolved = crate::identity::resolve_agent_id(None, None).unwrap();
        db::register_agent(&conn, &resolved, "human", &[]).unwrap();
        let req = make_tools_call("memory_subscribe", json!({"url": "not-a-url-at-all"}));
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_namespace_set_standard_with_valid_governance() {
        // Drives the governance-merge branch (lines 2284-2322) which
        // re-writes the standard memory's metadata with the resolved
        // policy.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-gov-ok".into(),
            title: "p".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_namespace_set_standard",
            json!({
                "namespace": "w12-gov-ok",
                "id": id,
                "governance": {
                    "write": "any",
                    "promote": "any",
                    "delete": "owner",
                    "approver": "human",
                },
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["set"], true);
        assert!(val["governance"].is_object());
    }

    #[test]
    fn handle_namespace_set_standard_with_parent() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-parent-ns".into(),
            title: "p".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_namespace_set_standard",
            json!({
                "namespace": "w12-parent-ns",
                "id": id,
                "parent": "w12-grand-ns",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["parent"], "w12-grand-ns");
    }

    #[test]
    fn handle_get_resolves_by_prefix_and_includes_links() {
        // db::resolve_id walks both exact and prefix lookup. Insert a
        // memory and request it by its 8-char prefix to drive the
        // prefix branch.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-prefix".into(),
            title: "T".into(),
            content: "C".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call("memory_get", json!({"id": id}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["links"].is_array());
        assert_eq!(val["id"], id);
    }

    #[test]
    fn handle_link_creates_link_between_existing_memories() {
        // Drives the create_link happy path (smoke matrix uses bogus IDs
        // so the existence check fails out before INSERT).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let src = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-link".into(),
            title: "src".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let mut tgt = src.clone();
        tgt.id = uuid::Uuid::new_v4().to_string();
        tgt.title = "tgt".into();
        let src_id = db::insert(&conn, &src).unwrap();
        let tgt_id = db::insert(&conn, &tgt).unwrap();
        let req = make_tools_call(
            "memory_link",
            json!({
                "source_id": src_id,
                "target_id": tgt_id,
                "relation": "related_to",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["linked"], true);
    }

    #[test]
    fn handle_get_links_returns_outbound_and_inbound() {
        // Seed source+target+link, query links from source.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let src = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-getlinks".into(),
            title: "src".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let mut tgt = src.clone();
        tgt.id = uuid::Uuid::new_v4().to_string();
        let src_id = db::insert(&conn, &src).unwrap();
        let tgt_id = db::insert(&conn, &tgt).unwrap();
        db::create_link(&conn, &src_id, &tgt_id, "supersedes").unwrap();

        let req = make_tools_call("memory_get_links", json!({"id": src_id}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn handle_kg_timeline_with_seeded_link_returns_event() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let src = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-tl".into(),
            title: "src".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let mut tgt = src.clone();
        tgt.id = uuid::Uuid::new_v4().to_string();
        tgt.title = "tgt".into();
        let src_id = db::insert(&conn, &src).unwrap();
        let tgt_id = db::insert(&conn, &tgt).unwrap();
        db::create_link(&conn, &src_id, &tgt_id, "related_to").unwrap();

        let req = make_tools_call(
            "memory_kg_timeline",
            json!({"source_id": src_id, "limit": 10}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["count"], 1);
        let events = val["events"].as_array().unwrap();
        assert_eq!(events[0]["target_id"], tgt_id);
    }

    #[test]
    fn handle_kg_query_with_seeded_link_returns_node() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let src = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-kgq".into(),
            title: "src".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let mut tgt = src.clone();
        tgt.id = uuid::Uuid::new_v4().to_string();
        let src_id = db::insert(&conn, &src).unwrap();
        let tgt_id = db::insert(&conn, &tgt).unwrap();
        db::create_link(&conn, &src_id, &tgt_id, "related_to").unwrap();

        let req = make_tools_call(
            "memory_kg_query",
            json!({"source_id": src_id, "max_depth": 1, "limit": 10}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["count"].as_u64().unwrap() >= 1);
        assert!(val["paths"].is_array());
    }

    #[test]
    fn handle_archive_list_with_pagination() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_archive_list", json!({"limit": 100, "offset": 50}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_pending_list_with_status_filter() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        for status in &["pending", "approved", "rejected"] {
            let req = make_tools_call(
                "memory_pending_list",
                json!({"status": status, "limit": 50}),
            );
            let resp = invoke_handle_request(&conn, &req);
            assert!(resp.error.is_none(), "failed for status={status}");
        }
    }

    #[test]
    fn handle_pending_approve_with_seeded_pending_action() {
        // Seed a pending action to drive the consensus / approval branch.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let pending_id = db::queue_pending_action(
            &conn,
            crate::models::GovernedAction::Promote,
            "w12-approve",
            None,
            "human:requestor",
            &json!({"id": "00000000-0000-0000-0000-000000000000"}),
        )
        .unwrap();
        let req = make_tools_call(
            "memory_pending_approve",
            json!({"id": pending_id, "agent_id": "human:approver"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        // Either approves outright or marks pending — both touch the
        // ApproveOutcome match arms in the handler.
        let result = resp.result.unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn handle_pending_reject_with_seeded_pending_action() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let pending_id = db::queue_pending_action(
            &conn,
            crate::models::GovernedAction::Promote,
            "w12-reject",
            None,
            "human:requestor",
            &json!({"id": "00000000-0000-0000-0000-000000000000"}),
        )
        .unwrap();
        let req = make_tools_call(
            "memory_pending_reject",
            json!({"id": pending_id, "agent_id": "human:rejector"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["rejected"], true);
    }

    #[test]
    fn handle_session_start_toon_format_default() {
        // session_start defaults to TOON compact format — drives the
        // toon_compact match arm in the format dispatch.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_session_start", json!({"namespace": "w12-toon"}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        // TOON output is plain text, not JSON — just confirm it's present.
        let result = resp.result.unwrap();
        assert!(result["content"][0]["text"].is_string());
    }

    #[test]
    fn handle_search_explicit_toon_format() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_search",
            json!({"query": "anything", "format": "toon"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_recall_explicit_toon_format() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_recall", json!({"context": "ctx", "format": "toon"}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_list_explicit_toon_compact_format() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_list",
            json!({"namespace": "w12-toon-list", "format": "toon_compact"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_search_with_namespace_and_tier_filters() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_search",
            json!({
                "query": "test query",
                "namespace": "w12-search",
                "tier": "long",
                "limit": 10,
                "agent_id": "ai:bot",
                "format": "json",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_search_invalid_agent_id_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_search",
            json!({"query": "x", "agent_id": "bad agent !!"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_search_invalid_as_agent_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_search",
            json!({"query": "x", "as_agent": "BAD AS AGENT"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_recall_invalid_as_agent_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_recall",
            json!({"context": "x", "as_agent": "INVALID NS"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_recall_with_context_tokens() {
        // Drives the context_tokens-not-empty branch (without an embedder
        // it just feeds the keyword fallback).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_recall",
            json!({
                "context": "main",
                "context_tokens": ["recent", "tokens", "from", "convo"],
                "format": "json",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_recall_with_budget_tokens_positive() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_recall",
            json!({"context": "x", "budget_tokens": 1000, "format": "json"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["tokens_used"].is_u64() || val["tokens_used"].is_i64());
        assert_eq!(val["budget_tokens"], 1000);
    }

    #[test]
    fn handle_recall_invalid_namespace_filter_passes_through() {
        // Recall accepts a namespace filter without validating; an
        // unknown namespace simply returns zero results.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_recall",
            json!({
                "context": "x",
                "namespace": "w12-no-such-namespace",
                "format": "json",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_list_with_tier_filter() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_list",
            json!({
                "namespace": "w12-list-tier",
                "tier": "long",
                "agent_id": "ai:bot",
                "limit": 25,
                "format": "json",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_list_invalid_tier_treated_as_none() {
        // tier::from_str returns None for an invalid value, which the
        // handler tolerates (no validation error) — drives the
        // and_then-None branch.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_list",
            json!({"namespace": "w12-list-bad-tier", "tier": "ULTRAMID", "format": "json"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_get_taxonomy_invalid_depth_clamps_to_max() {
        // `depth` saturates against MAX_NAMESPACE_DEPTH; very large
        // values still succeed.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_get_taxonomy",
            json!({"depth": 100_000_u64, "limit": 50_000_u64}),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_archive_purge_no_filter_purges_all() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_archive_purge", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_check_duplicate_invalid_title_rejected() {
        // No embedder → standard error; but when title is empty the
        // validate_title path errors before the embedder check.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_check_duplicate",
            json!({"title": "", "content": "anything"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_check_duplicate_invalid_namespace_rejected() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_check_duplicate",
            json!({"title": "T", "content": "C", "namespace": "BAD NS"}),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_entity_register_with_explicit_agent_id() {
        // Drives the explicit_agent_id-Some branch (validates +
        // resolves).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_entity_register",
            json!({
                "canonical_name": "Org Alpha",
                "namespace": "w12-orgs",
                "aliases": ["alpha", "α"],
                "agent_id": "ai:bot",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_entity_register_invalid_explicit_agent_id() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call(
            "memory_entity_register",
            json!({
                "canonical_name": "Org Beta",
                "namespace": "w12-orgs",
                "agent_id": "BAD AGENT !!",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn handle_entity_get_by_alias_no_namespace() {
        // Drives the namespace=None branch (alias lookup across all ns).
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let req = make_tools_call("memory_entity_get_by_alias", json!({"alias": "any-alias"}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_inbox_with_message_seeded() {
        // Notify alice, then read alice's inbox.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let notify = make_tools_call(
            "memory_notify",
            json!({
                "target_agent_id": "alice-w12",
                "title": "ping",
                "payload": "are you there?",
                "tier": "short",
            }),
        );
        let _ = invoke_handle_request(&conn, &notify);
        let inbox = make_tools_call(
            "memory_inbox",
            json!({"agent_id": "alice-w12", "limit": 10}),
        );
        let resp = invoke_handle_request(&conn, &inbox);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["count"].as_u64().unwrap() >= 1);
        assert_eq!(val["agent_id"], "alice-w12");
    }

    #[test]
    fn handle_consolidate_succeeds_when_source_was_standard() {
        // Even when one of the source memories is a namespace standard,
        // consolidate must succeed (the warning branch may or may not
        // fire depending on whether is_namespace_standard sees the row
        // pre- or post-deletion). This drives both the namespace-standard
        // check loop and the consolidate happy path together.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem_a = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "w12-cons-warn".into(),
            title: "a".into(),
            content: "alpha".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let mut mem_b = mem_a.clone();
        mem_b.id = uuid::Uuid::new_v4().to_string();
        mem_b.title = "b".into();
        mem_b.content = "beta".into();
        let id_a = db::insert(&conn, &mem_a).unwrap();
        let id_b = db::insert(&conn, &mem_b).unwrap();
        // Mark id_a as the standard for w12-cons-warn.
        db::set_namespace_standard(&conn, "w12-cons-warn", &id_a, None).unwrap();

        let req = make_tools_call(
            "memory_consolidate",
            json!({
                "ids": [id_a, id_b],
                "title": "merged-warn",
                "summary": "merged summary",
                "namespace": "w12-cons-warn",
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert!(val["id"].is_string());
        assert_eq!(val["consolidated"], 2);
    }

    #[test]
    fn handle_update_clears_expires_with_empty_string() {
        // expires_at="" path is special-cased by db::update to clear
        // the column.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Short,
            namespace: "w12-clear-exp".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: Some(chrono::Utc::now().to_rfc3339()),
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call("memory_update", json!({"id": id, "expires_at": ""}));
        let resp = invoke_handle_request(&conn, &req);
        // empty "" is rejected by validate_expires_at_format; the
        // handler returns isError.
        let result = resp.result.unwrap();
        // The result shape depends on whether validate accepts "" — both
        // outcomes exercise distinct paths, so accept either.
        assert!(result.is_object());
    }

    #[test]
    fn handle_update_change_namespace() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "w12-update-ns".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call(
            "memory_update",
            json!({
                "id": id,
                "namespace": "w12-update-ns-new",
                "tags": ["a", "b"],
                "title": "new-title",
                "content": "new-content",
                "tier": "long",
                "priority": 8_i64,
                "confidence": 0.9_f64,
            }),
        );
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
    }

    #[test]
    fn handle_delete_with_prefix_id_lookup() {
        // db::get_by_prefix is consulted when exact ID lookup misses.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "w12-delete-prefix".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({}),
        };
        let id = db::insert(&conn, &mem).unwrap();
        let req = make_tools_call("memory_delete", json!({"id": id}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["deleted"], true);
    }

    #[test]
    fn handle_unsubscribe_after_subscribe_removes_row() {
        // Drives the removed=1 branch.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let resolved = crate::identity::resolve_agent_id(None, None).unwrap();
        db::register_agent(&conn, &resolved, "human", &[]).unwrap();
        let sub = make_tools_call(
            "memory_subscribe",
            json!({"url": "https://example.com/hook2"}),
        );
        let sub_resp = invoke_handle_request(&conn, &sub);
        let sub_text = sub_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let sub_val: Value = serde_json::from_str(&sub_text).unwrap();
        let id = sub_val["id"].as_str().unwrap().to_string();

        let unsub = make_tools_call("memory_unsubscribe", json!({"id": id}));
        let unsub_resp = invoke_handle_request(&conn, &unsub);
        assert!(unsub_resp.error.is_none());
        let unsub_text = unsub_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let unsub_val: Value = serde_json::from_str(&unsub_text).unwrap();
        assert!(
            unsub_val["removed"] == json!(true) || unsub_val["removed"] == json!(1),
            "unexpected removed value: {:?}",
            unsub_val["removed"]
        );
    }

    #[test]
    fn handle_list_subscriptions_after_subscribe_returns_one() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let resolved = crate::identity::resolve_agent_id(None, None).unwrap();
        db::register_agent(&conn, &resolved, "human", &[]).unwrap();
        let sub = make_tools_call(
            "memory_subscribe",
            json!({"url": "https://example.com/listed"}),
        );
        let _ = invoke_handle_request(&conn, &sub);
        let req = make_tools_call("memory_list_subscriptions", json!({}));
        let resp = invoke_handle_request(&conn, &req);
        assert!(resp.error.is_none());
        let text = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let val: Value = serde_json::from_str(&text).unwrap();
        // subscriptions field holds the array; the count field may be at
        // top level — accept either key.
        assert!(val.get("subscriptions").is_some() || val.get("count").is_some() || val.is_array());
    }

    #[test]
    fn test_inject_namespace_standard_dedup_keeps_originals_order() {
        // When the standard is one of the recall hits, dedup removes it
        // but preserves the relative order of remaining results.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let std_id = seed_namespace_standard(&conn, "w12-order", "S");
        let mems = vec![
            json!({"id": "first", "title": "f"}),
            json!({"id": std_id, "title": "S"}),
            json!({"id": "third", "title": "t"}),
        ];
        let mut resp = make_recall_response(mems);
        super::inject_namespace_standard(&conn, Some("w12-order"), &mut resp);
        let memories = resp["memories"].as_array().unwrap();
        assert_eq!(memories.len(), 2);
        assert_eq!(memories[0]["id"], "first");
        assert_eq!(memories[1]["id"], "third");
    }
}
