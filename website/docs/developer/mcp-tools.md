---
sidebar_position: 4
title: MCP tool reference
description: All 23 MCP tools exposed by ai-memory.
---

# MCP tool reference

ai-memory exposes **23 MCP tools** over stdio JSON-RPC 2.0. Tool definitions live in `src/mcp.rs::tool_definitions()`.

## Core CRUD

| Tool | Purpose |
|---|---|
| `memory_store` | Store a new memory or upsert |
| `memory_get` | Retrieve by ID (full + short prefix) |
| `memory_list` | List with filter + pagination |
| `memory_update` | Update fields (preserves `agent_id`) |
| `memory_delete` | Delete by ID |
| `memory_promote` | Promote to long-tier (or to ancestor namespace) |
| `memory_forget` | Bulk delete by namespace/tier/pattern |

## Recall + search

| Tool | Purpose |
|---|---|
| `memory_recall` | Hybrid recall, 6-factor scoring, supports `budget_tokens`, `as_agent` |
| `memory_search` | Exact-keyword AND search |
| `memory_session_start` | Inject namespace standards + recent memories at session boot |

## Links + relationships

| Tool | Purpose |
|---|---|
| `memory_link` | Link two memories (`related_to`/`supersedes`/`contradicts`/`derived_from`) |
| `memory_get_links` | Get all links for a memory |
| `memory_consolidate` | Merge memories, preserves provenance via `derived_from` |

## Smart tier (Ollama-backed)

| Tool | Purpose | Tier |
|---|---|---|
| `memory_expand_query` | Expand a recall query with synonyms | smart+ |
| `memory_auto_tag` | Auto-suggest tags via LLM | smart+ |
| `memory_detect_contradiction` | Check if two memories disagree | smart+ |

## Archive

| Tool | Purpose |
|---|---|
| `memory_archive_list` | List archived memories |
| `memory_archive_restore` | Restore from archive (back to long-tier) |
| `memory_archive_purge` | Permanently delete archived |
| `memory_archive_stats` | Counts + storage |

## Namespaces + agents (v0.6.0+)

| Tool | Purpose |
|---|---|
| `memory_namespace_set_standard` | Set the standard memory for a namespace |
| `memory_namespace_get_standard` | Get the active standard |
| `memory_namespace_clear_standard` | Clear the standard |
| `memory_agent_register` | Register an agent in `_agents` namespace |
| `memory_capabilities` | Report current tier + loaded models |

## Adding a new tool

```rust
// 1. Add JSON definition in tool_definitions()
// 2. Add match arm in dispatch block
// 3. Implement handler taking &Connection + params
// 4. Return Result<Value>
```

See `src/mcp.rs` for the patterns.
