# User Guide

> **BLUF (Bottom Line Up Front):** `ai-memory` gives any AI assistant persistent memory across sessions. It works with **any MCP-compatible AI client** -- including Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others. Configure the MCP server once, and your AI automatically stores and recalls knowledge -- your project architecture, preferences, past decisions, and hard-won lessons.

## What Is This and Why Do I Need It?

`ai-memory` gives any AI assistant persistent memory across sessions. Without it, every conversation starts from zero. With it, your AI can:

- Remember your project architecture, preferences, and past decisions
- Recall debugging context from yesterday
- Build up institutional knowledge over time
- Never repeat the same mistakes twice

Think of it as a brain for your AI assistant -- short-term for what you're doing right now, mid-term for this week's work, and long-term for things that should never be forgotten.

## MCP Integration (Recommended)

The easiest way to use ai-memory is as an **MCP tool server**. MCP (Model Context Protocol) is an open standard supported by multiple AI platforms. ai-memory works with **Claude Code, Codex, Gemini, Cursor, Windsurf, Continue.dev, Grok, Llama**, and any other MCP-compatible client. Once configured, your AI client can store and recall memories natively without any manual CLI usage.

### Setup

Each AI platform has its own MCP configuration path and format. See the [Installation Guide](INSTALL.md) for platform-specific setup instructions.

Below is an example for **Claude Code** (user scope: merge `mcpServers` into `~/.claude.json`; or project scope: `.mcp.json` in project root) — one of many supported platforms:

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

> **Claude Code note:** MCP server configuration does **not** go in `settings.json` or `settings.local.json` -- those files do not support `mcpServers`.

> **Tier flag:** The `--tier` flag must be passed in the args: `keyword`, `semantic` (default), `smart`, or `autonomous`. The `config.toml` tier setting is not used when launched by an AI client. Smart/autonomous tiers require [Ollama](https://ollama.com).

> **Other platforms** (Codex, Gemini, Cursor, Windsurf, Continue.dev, etc.): config paths vary by platform. The command and args are the same -- only the config file location differs. Refer to the [Installation Guide](INSTALL.md) for exact paths.

> **Grok note:** Grok connects via remote MCP over HTTPS only (no stdio). Run `ai-memory serve` and expose it behind an HTTPS reverse proxy. `server_label` is required. See the [Installation Guide](INSTALL.md) for details.

> **Llama note:** Llama Stack connects over HTTP rather than stdio MCP. Run `ai-memory serve` to start the HTTP daemon, then point your client at `http://localhost:9077`. See the [Installation Guide](INSTALL.md) for details.

### How It Works

With MCP configured, your AI client gains 21 memory tools:

- **memory_store** -- Store new knowledge (auto-deduplicates by title+namespace, reports contradictions)
- **memory_recall** -- Recall relevant memories for the current context (supports `until` date filter)
- **memory_search** -- Search for specific memories by keyword (max 200 results)
- **memory_list** -- Browse memories with filters (max 200 results)
- **memory_delete** -- Remove a specific memory
- **memory_promote** -- Make a memory permanent (long-term)
- **memory_forget** -- Bulk delete memories by pattern
- **memory_stats** -- View memory statistics
- **memory_update** -- Update an existing memory by ID (partial update, supports `expires_at`)
- **memory_get** -- Get a specific memory by ID with its links
- **memory_link** -- Create a link between two memories
- **memory_get_links** -- Get all links for a memory
- **memory_consolidate** -- Consolidate multiple memories into one long-term summary (2-100 memories)
- **memory_capabilities** -- Report which features are available at the current tier
- **memory_expand_query** -- Expand a recall query with synonyms and related terms (smart+ tier)
- **memory_auto_tag** -- Automatically suggest tags for a memory based on its content (smart+ tier)
- **memory_detect_contradiction** -- Detect contradictions between a new memory and existing ones (smart+ tier)
- **memory_archive_list** -- List archived memories (memories preserved by GC when archiving is enabled)
- **memory_archive_restore** -- Restore an archived memory back to active status
- **memory_archive_purge** -- Permanently delete all archived memories
- **memory_archive_stats** -- View archive statistics (count, size, oldest/newest)

Your AI assistant uses these tools automatically during conversations. You can also ask directly: "Remember that we use PostgreSQL 15" or "What do you remember about our auth system?"

## MCP Tool Reference

This section documents all 21 MCP tools with their exact parameter schemas, example requests, and response formats. All tools are invoked via JSON-RPC 2.0 using method `tools/call` with the tool name in `params.name` and tool parameters in `params.arguments`.

All responses are wrapped in the MCP content envelope:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "content": [{ "type": "text", "text": "<tool output>" }]
  }
}
```

On error, the envelope includes `"isError": true` and the text contains the error message.

---

### memory_store

Store a new memory. Deduplicates by title+namespace -- if a memory with the same title and namespace already exists, it updates the existing memory instead of creating a duplicate.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `title` | string | Yes | -- | Short descriptive title |
| `content` | string | Yes | -- | Full memory content (max 64 KB) |
| `tier` | string | No | `"mid"` | Memory tier: `"short"`, `"mid"`, or `"long"` |
| `namespace` | string | No | `"global"` | Project/topic namespace |
| `tags` | array of strings | No | `[]` | Tags for filtering |
| `priority` | integer (1-10) | No | `5` | Priority ranking |
| `confidence` | number (0.0-1.0) | No | `1.0` | Certainty level |
| `source` | string | No | `"claude"` | Origin: `"user"`, `"claude"`, `"hook"`, `"api"`, `"cli"`, `"import"`, `"consolidation"`, `"system"` |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "memory_store",
    "arguments": {
      "title": "Project uses PostgreSQL 15",
      "content": "The main database is PostgreSQL 15 with pgvector for embeddings.",
      "tier": "long",
      "namespace": "my-app",
      "tags": ["database", "infrastructure"],
      "priority": 8,
      "source": "user"
    }
  }
}
```

**Example response (new memory):**

```json
{
  "id": "a1b2c3d4-...",
  "tier": "long",
  "title": "Project uses PostgreSQL 15",
  "namespace": "my-app",
  "potential_contradictions": ["e5f6g7h8-..."]
}
```

**Example response (duplicate -- updated existing):**

```json
{
  "id": "existing-id-...",
  "tier": "long",
  "title": "Project uses PostgreSQL 15",
  "namespace": "my-app",
  "duplicate": true,
  "action": "updated existing memory"
}
```

---

### memory_recall

Recall memories relevant to a context. Uses fuzzy OR matching, ranked by a composite score of relevance + priority + access frequency + confidence + tier boost + recency decay. At semantic tier and above, uses hybrid scoring (semantic + keyword blending).

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `context` | string | Yes | -- | What you are trying to remember |
| `namespace` | string | No | -- | Filter by namespace |
| `limit` | integer (max 50) | No | `10` | Maximum results to return |
| `tags` | string | No | -- | Filter by tag |
| `since` | string | No | -- | Only memories created after this RFC 3339 timestamp |
| `until` | string | No | -- | Only memories created before this RFC 3339 timestamp |
| `format` | string | No | `"toon_compact"` | Response format: `"json"`, `"toon"`, or `"toon_compact"` (default saves 79% tokens vs JSON) |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/call",
  "params": {
    "name": "memory_recall",
    "arguments": {
      "context": "database setup and configuration",
      "namespace": "my-app",
      "limit": 5
    }
  }
}
```

**Example response (JSON format):**

```json
{
  "memories": [
    {
      "id": "a1b2c3d4-...",
      "title": "Project uses PostgreSQL 15",
      "content": "The main database is PostgreSQL 15 with pgvector.",
      "tier": "long",
      "namespace": "my-app",
      "priority": 8,
      "tags": ["database"],
      "score": 0.763,
      "confidence": 1.0,
      "access_count": 3,
      "created_at": "2026-04-01T12:00:00Z",
      "updated_at": "2026-04-10T08:00:00Z"
    }
  ],
  "count": 1,
  "mode": "hybrid"
}
```

**Example response (TOON compact, the default):**

```
count:1|mode:hybrid
memories[id|title|tier|namespace|priority|score|tags]:
a1b2c3d4-...|Project uses PostgreSQL 15|long|my-app|8|0.763|database
```

---

### memory_search

Search memories by exact keyword match with AND semantics (all terms must match).

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `query` | string | Yes | -- | Search keywords |
| `namespace` | string | No | -- | Filter by namespace |
| `tier` | string | No | -- | Filter by tier: `"short"`, `"mid"`, or `"long"` |
| `limit` | integer (max 200) | No | `20` | Maximum results |
| `format` | string | No | `"toon_compact"` | Response format: `"json"`, `"toon"`, or `"toon_compact"` |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "memory_search",
    "arguments": {
      "query": "PostgreSQL",
      "namespace": "my-app"
    }
  }
}
```

**Example response (JSON format):**

```json
{
  "results": [ { "id": "a1b2c3d4-...", "title": "Project uses PostgreSQL 15", "..." : "..." } ],
  "count": 1
}
```

---

### memory_list

List memories, optionally filtered by namespace or tier.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `namespace` | string | No | -- | Filter by namespace |
| `tier` | string | No | -- | Filter by tier: `"short"`, `"mid"`, or `"long"` |
| `limit` | integer (max 200) | No | `20` | Maximum results |
| `format` | string | No | `"toon_compact"` | Response format: `"json"`, `"toon"`, or `"toon_compact"` |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "tools/call",
  "params": {
    "name": "memory_list",
    "arguments": {
      "namespace": "my-app",
      "tier": "long",
      "limit": 10
    }
  }
}
```

**Example response (JSON format):**

```json
{
  "memories": [ { "id": "...", "title": "...", "tier": "long", "..." : "..." } ],
  "count": 1
}
```

---

### memory_delete

Delete a memory by ID.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id` | string | Yes | -- | Memory ID to delete |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "tools/call",
  "params": {
    "name": "memory_delete",
    "arguments": { "id": "a1b2c3d4-..." }
  }
}
```

**Example response:**

```json
{ "deleted": true }
```

---

### memory_promote

Promote a memory to long-term (permanent). Clears the expiry timestamp.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id` | string | Yes | -- | Memory ID to promote |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "tools/call",
  "params": {
    "name": "memory_promote",
    "arguments": { "id": "a1b2c3d4-..." }
  }
}
```

**Example response:**

```json
{ "promoted": true, "id": "a1b2c3d4-...", "tier": "long" }
```

---

### memory_forget

Bulk delete memories matching a pattern, namespace, or tier. At least one filter should be provided.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `namespace` | string | No | -- | Filter by namespace |
| `pattern` | string | No | -- | Delete memories matching this pattern |
| `tier` | string | No | -- | Filter by tier: `"short"`, `"mid"`, or `"long"` |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "tools/call",
  "params": {
    "name": "memory_forget",
    "arguments": {
      "namespace": "my-app",
      "tier": "short"
    }
  }
}
```

**Example response:**

```json
{ "deleted": 12 }
```

---

### memory_stats

Get memory store statistics (counts, tiers, namespaces, links, database size).

**Parameters:** None.

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "method": "tools/call",
  "params": {
    "name": "memory_stats",
    "arguments": {}
  }
}
```

**Example response:**

```json
{
  "total": 142,
  "by_tier": { "short": 5, "mid": 37, "long": 100 },
  "by_namespace": { "my-app": 80, "global": 62 },
  "links": 23,
  "db_size_bytes": 524288
}
```

---

### memory_update

Update an existing memory by ID. Only provided fields are changed -- omitted fields remain unchanged.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id` | string | Yes | -- | Memory ID to update |
| `title` | string | No | -- | New title |
| `content` | string | No | -- | New content |
| `tier` | string | No | -- | New tier: `"short"`, `"mid"`, or `"long"` |
| `namespace` | string | No | -- | New namespace |
| `tags` | array of strings | No | -- | New tags (replaces existing) |
| `priority` | integer (1-10) | No | -- | New priority |
| `confidence` | number (0.0-1.0) | No | -- | New confidence |
| `expires_at` | string | No | -- | Expiry timestamp (RFC 3339), or null to clear |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 9,
  "method": "tools/call",
  "params": {
    "name": "memory_update",
    "arguments": {
      "id": "a1b2c3d4-...",
      "priority": 9,
      "tags": ["database", "critical"]
    }
  }
}
```

**Example response:**

```json
{
  "updated": true,
  "memory": {
    "id": "a1b2c3d4-...",
    "title": "Project uses PostgreSQL 15",
    "tier": "long",
    "priority": 9,
    "tags": ["database", "critical"],
    "...": "..."
  }
}
```

---

### memory_get

Get a specific memory by ID, including its links.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id` | string | Yes | -- | Memory ID to retrieve |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "tools/call",
  "params": {
    "name": "memory_get",
    "arguments": { "id": "a1b2c3d4-..." }
  }
}
```

**Example response:**

```json
{
  "id": "a1b2c3d4-...",
  "title": "Project uses PostgreSQL 15",
  "content": "The main database is PostgreSQL 15 with pgvector.",
  "tier": "long",
  "namespace": "my-app",
  "priority": 8,
  "confidence": 1.0,
  "tags": ["database"],
  "source": "user",
  "access_count": 5,
  "created_at": "2026-04-01T12:00:00Z",
  "updated_at": "2026-04-10T08:00:00Z",
  "last_accessed_at": "2026-04-12T09:00:00Z",
  "expires_at": null,
  "links": [
    { "source_id": "a1b2c3d4-...", "target_id": "e5f6g7h8-...", "relation": "related_to" }
  ]
}
```

---

### memory_link

Create a link between two memories.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `source_id` | string | Yes | -- | Source memory ID |
| `target_id` | string | Yes | -- | Target memory ID |
| `relation` | string | No | `"related_to"` | Relation type: `"related_to"`, `"supersedes"`, `"contradicts"`, or `"derived_from"` |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "method": "tools/call",
  "params": {
    "name": "memory_link",
    "arguments": {
      "source_id": "a1b2c3d4-...",
      "target_id": "e5f6g7h8-...",
      "relation": "supersedes"
    }
  }
}
```

**Example response:**

```json
{
  "linked": true,
  "source_id": "a1b2c3d4-...",
  "target_id": "e5f6g7h8-...",
  "relation": "supersedes"
}
```

---

### memory_get_links

Get all links for a memory (both directions -- where the memory is source or target).

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id` | string | Yes | -- | Memory ID to get links for |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "method": "tools/call",
  "params": {
    "name": "memory_get_links",
    "arguments": { "id": "a1b2c3d4-..." }
  }
}
```

**Example response:**

```json
{
  "links": [
    { "source_id": "a1b2c3d4-...", "target_id": "e5f6g7h8-...", "relation": "related_to" }
  ],
  "count": 1
}
```

---

### memory_consolidate

Consolidate multiple memories into one long-term summary. Deletes source memories and creates `derived_from` links. If `summary` is omitted and LLM is available (smart/autonomous tier), auto-generates a summary.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `ids` | array of strings (2-100 items) | Yes | -- | Memory IDs to consolidate |
| `title` | string | Yes | -- | Title for the consolidated memory |
| `summary` | string | No | -- | Summary content. Auto-generated via LLM if omitted at smart/autonomous tier |
| `namespace` | string | No | `"global"` | Namespace for the consolidated memory |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 13,
  "method": "tools/call",
  "params": {
    "name": "memory_consolidate",
    "arguments": {
      "ids": ["id-1", "id-2", "id-3"],
      "title": "Auth system architecture",
      "summary": "JWT tokens with refresh rotation, RBAC via middleware, sessions in Redis."
    }
  }
}
```

**Example response:**

```json
{ "id": "new-consolidated-id-...", "consolidated": 3 }
```

**Example response (auto-generated summary):**

```json
{
  "id": "new-consolidated-id-...",
  "consolidated": 3,
  "auto_summary": true,
  "summary_preview": "JWT tokens with refresh rotation, RBAC via middleware..."
}
```

---

### memory_capabilities

Report the active feature tier, loaded models, and available capabilities of the memory system. Takes no parameters. Call once per session to discover what features are available.

**Parameters:** None.

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 14,
  "method": "tools/call",
  "params": {
    "name": "memory_capabilities",
    "arguments": {}
  }
}
```

**Example response:**

```json
{
  "tier": "semantic",
  "features": {
    "embedding_recall": true,
    "hybrid_scoring": true,
    "query_expansion": false,
    "auto_tagging": false,
    "contradiction_detection": false,
    "cross_encoder_reranking": false,
    "memory_reflection": false
  },
  "models": {
    "embedding": "all-MiniLM-L6-v2",
    "llm": "none",
    "cross_encoder": "none"
  }
}
```

---

### memory_expand_query

Use LLM to expand a search query into additional semantically related terms. Requires smart or autonomous tier.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `query` | string | Yes | -- | The search query to expand |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 15,
  "method": "tools/call",
  "params": {
    "name": "memory_expand_query",
    "arguments": { "query": "database migration" }
  }
}
```

**Example response:**

```json
{
  "original": "database migration",
  "expanded_terms": ["schema change", "data migration", "SQL migration", "alembic", "flyway", "db upgrade"]
}
```

---

### memory_auto_tag

Use LLM to auto-generate tags for a memory. Merges new tags with existing ones. Requires smart or autonomous tier.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id` | string | Yes | -- | Memory ID to auto-tag |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 16,
  "method": "tools/call",
  "params": {
    "name": "memory_auto_tag",
    "arguments": { "id": "a1b2c3d4-..." }
  }
}
```

**Example response:**

```json
{
  "id": "a1b2c3d4-...",
  "new_tags": ["postgresql", "infrastructure", "database"],
  "all_tags": ["existing-tag", "postgresql", "infrastructure", "database"]
}
```

---

### memory_detect_contradiction

Use LLM to check if two memories contradict each other. Requires smart or autonomous tier.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id_a` | string | Yes | -- | First memory ID |
| `id_b` | string | Yes | -- | Second memory ID |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 17,
  "method": "tools/call",
  "params": {
    "name": "memory_detect_contradiction",
    "arguments": {
      "id_a": "a1b2c3d4-...",
      "id_b": "e5f6g7h8-..."
    }
  }
}
```

**Example response:**

```json
{
  "contradicts": true,
  "memory_a": { "id": "a1b2c3d4-...", "title": "Use PostgreSQL 15" },
  "memory_b": { "id": "e5f6g7h8-...", "title": "Use MySQL 8" }
}
```

---

### memory_archive_list

List archived (expired) memories. Archived memories are preserved by garbage collection when `archive_on_gc = true` in config.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `namespace` | string | No | -- | Filter by namespace |
| `limit` | integer (max 1000) | No | `50` | Maximum results |
| `offset` | integer | No | `0` | Pagination offset |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 18,
  "method": "tools/call",
  "params": {
    "name": "memory_archive_list",
    "arguments": { "limit": 10 }
  }
}
```

**Example response:**

```json
{
  "archived": [ { "id": "...", "title": "...", "..." : "..." } ],
  "count": 3
}
```

---

### memory_archive_restore

Restore an archived memory back to the active memory store. The restored memory has its `expires_at` cleared (becomes permanent).

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `id` | string | Yes | -- | ID of the archived memory to restore |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 19,
  "method": "tools/call",
  "params": {
    "name": "memory_archive_restore",
    "arguments": { "id": "archived-id-..." }
  }
}
```

**Example response:**

```json
{ "restored": true, "id": "archived-id-..." }
```

---

### memory_archive_purge

Permanently delete archived memories. Optionally only those older than N days. This cannot be undone.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `older_than_days` | integer | No | -- | Only purge entries archived more than N days ago. Omit to purge all |

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "tools/call",
  "params": {
    "name": "memory_archive_purge",
    "arguments": { "older_than_days": 30 }
  }
}
```

**Example response:**

```json
{ "purged": 7 }
```

---

### memory_archive_stats

Show archive statistics: total count and breakdown by namespace.

**Parameters:** None.

**Example request:**

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "tools/call",
  "params": {
    "name": "memory_archive_stats",
    "arguments": {}
  }
}
```

**Example response:**

```json
{
  "total": 15,
  "by_namespace": { "my-app": 10, "global": 5 }
}
```

---

## Zero Token Cost

Unlike built-in memory systems (Claude Code auto-memory, ChatGPT memory) that load your entire memory into every conversation, ai-memory uses **zero context tokens until recalled**. Only relevant memories come back, ranked by a 6-factor scoring algorithm. For Claude Code users: disable auto-memory (`"autoMemoryEnabled": false` in settings.json) to stop paying for 200+ lines of idle context.

## TOON Format (Token-Oriented Object Notation)

All recall, search, and list responses default to **TOON compact** format -- 79% smaller than JSON. Field names are declared once as a header, then values as pipe-delimited rows. This saves tokens on every recall.

- `format: "toon_compact"` (default) -- 79% smaller, omits timestamps
- `format: "toon"` -- 61% smaller, includes all fields
- `format: "json"` -- full JSON (use only when you need structured parsing)

## MCP Prompts

The server provides 2 MCP prompts via `prompts/list` that teach AI clients to use memory proactively:

- **recall-first** -- 8 behavioral rules: recall at session start, store corrections as long-term, use TOON format, tier strategy, dedup awareness
- **memory-workflow** -- Quick reference card for all tool usage patterns

## Feature Tiers

ai-memory supports 4 feature tiers, controlled by the `--tier` flag when starting the MCP server (e.g., `ai-memory mcp --tier semantic`). Each tier builds on the previous one:

| Tier | Recall Method | Extra Features | Requirements |
|------|--------------|----------------|--------------|
| **keyword** | FTS5 only | None | None (lightest) |
| **semantic** (default) | Hybrid: semantic + keyword blending | Embedding-based recall | HuggingFace embedding model (~256 MB RAM) |
| **smart** | Hybrid | Query expansion, auto-tagging, contradiction detection | Ollama + LLM (~1 GB RAM) |
| **autonomous** | Hybrid | Full autonomous memory management | Ollama + LLM (~4 GB RAM) |

> **Semantic tier first-run note:** The first run at the semantic tier (or above) downloads a ~100 MB embedding model from HuggingFace, which takes 30-60 seconds. Subsequent starts load from cache (~2 seconds). If the download fails, retry or use `--tier keyword` temporarily.

### Hybrid Recall (semantic tier and above)

At the `semantic` tier and above, recall uses **hybrid scoring** that blends two signals:

1. **Semantic similarity** -- the query and each memory are converted to embeddings (dense vectors), and cosine similarity measures how close they are in meaning. This catches relevant results even when exact keywords differ.
2. **Keyword matching** -- the existing FTS5 full-text search with the 6-factor composite score.

The final ranking blends both signals, so you get the precision of keyword matching plus the flexibility of semantic understanding.

### Query Expansion, Auto-Tagging, and Contradiction Detection (smart+ tier)

At the `smart` and `autonomous` tiers, three additional capabilities are available via LLM inference (requires Ollama):

- **Query expansion** (`memory_expand_query`) -- expands a recall query with synonyms, related terms, and alternative phrasings to improve recall coverage.
- **Auto-tagging** (`memory_auto_tag`) -- analyzes memory content and suggests relevant tags automatically.
- **Contradiction detection** (`memory_detect_contradiction`) -- compares a new memory against existing ones to detect semantic contradictions, even when the wording is different.

These tools are available to your AI assistant automatically at the smart+ tier. At lower tiers, calling them returns a tier-requirement notice.

## Getting Started (CLI)

### Store Your First Memory

```bash
ai-memory store \
  -T "Project uses PostgreSQL 15" \
  -c "The main database is PostgreSQL 15 with pgvector for embeddings." \
  --tier long \
  --priority 7
```

That's it. The memory is now stored permanently (long tier) with priority 7/10.

#### Custom Expiration

You can set a custom expiration on any memory:

```bash
# Set an explicit expiration timestamp
ai-memory store \
  -T "Sprint 42 goals" \
  -c "Finish migration, deploy v2 API." \
  --tier mid \
  --expires-at "2026-04-15T00:00:00Z"

# Or set a TTL in seconds (e.g., 2 hours)
ai-memory store \
  -T "Current debugging session" \
  -c "Investigating auth timeout in login.rs" \
  --tier short \
  --ttl-secs 7200
```

### Recall Memories

```bash
ai-memory recall "database setup"
```

This performs a fuzzy OR search across all your memories and returns the most relevant ones, ranked by a 6-factor composite score:

1. **FTS relevance** -- how well the text matches (via SQLite FTS5)
2. **Priority** -- higher priority memories rank higher (weight: 0.5)
3. **Access frequency** -- frequently recalled memories rank higher (weight: 0.1)
4. **Confidence** -- higher certainty memories rank higher (weight: 2.0)
5. **Tier boost** -- long-term gets +3.0, mid gets +1.0, short gets +0.0
6. **Recency decay** -- `1/(1 + days_old * 0.1)` so recent memories rank higher

Recall also automatically:
- Bumps the access count
- Extends the TTL (1 hour for short, 1 day for mid)
- Auto-promotes mid-tier memories to long-term after 5 accesses

### Search for Exact Matches

```bash
ai-memory search "PostgreSQL"
```

Search uses AND semantics -- all terms must match. Use this when you know exactly what you're looking for. Search uses the same 6-factor ranking but without the tier boost.

### Search vs Recall

Use **recall** when you vaguely remember something -- it uses fuzzy OR matching and returns ranked results. Any term can match, so it casts a wide net.

Use **search** when you know exact keywords -- it requires all terms to match (AND semantics), so results are precise but narrower.

## CLI Command Reference

All commands accept the following **global flags** (before the subcommand):

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--db` | | path | `ai-memory.db` (env: `AI_MEMORY_DB`) | Path to the SQLite database file |
| `--json` | | bool | `false` | Output as JSON (machine-parseable) |

---

### store

Store a new memory. Deduplicates by title+namespace (upserts on conflict).

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--title` | `-T` | string | required | Short descriptive title |
| `--content` | `-c` | string | required | Memory content (use `-` to read from stdin) |
| `--tier` | `-t` | string | `mid` | Memory tier: `short`, `mid`, or `long` |
| `--namespace` | `-n` | string | auto-detected | Namespace (auto-detects from git remote or directory name) |
| `--tags` | | string | `""` | Comma-separated tags |
| `--priority` | `-p` | int | `5` | Priority 1-10 |
| `--confidence` | | float | `1.0` | Confidence 0.0-1.0 |
| `--source` | `-S` | string | `cli` | Source: `user`, `claude`, `hook`, `api`, `cli` |
| `--expires-at` | | string | | Explicit expiry timestamp (RFC 3339). Overrides tier default |
| `--ttl-secs` | | int | | TTL in seconds. Overrides tier default |

---

### update

Update an existing memory by ID. Only the fields you provide are changed.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Memory ID |
| `--title` | `-T` | string | | New title |
| `--content` | `-c` | string | | New content |
| `--tier` | `-t` | string | | New tier: `short`, `mid`, or `long` |
| `--namespace` | `-n` | string | | New namespace |
| `--tags` | | string | | New comma-separated tags |
| `--priority` | `-p` | int | | New priority 1-10 |
| `--confidence` | | float | | New confidence 0.0-1.0 |
| `--expires-at` | | string | | Expiry timestamp (RFC 3339), or empty string to clear |

---

### recall

Recall memories relevant to a context. Fuzzy OR search with ranked results; auto-touches recalled memories.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Context query string |
| `--namespace` | `-n` | string | | Filter by namespace |
| `--limit` | | int | `10` | Maximum number of results |
| `--tags` | | string | | Filter by tags (comma-separated) |
| `--since` | | string | | Only memories created after this timestamp (RFC 3339) |
| `--until` | | string | | Only memories created before this timestamp (RFC 3339) |
| `--tier` | `-T` | string | | Feature tier for recall: `keyword`, `semantic`, `smart`, `autonomous` |

> **Note:** Default limit is 10 for CLI, 10 for MCP, capped at 50.

---

### search

Search memories by text. AND semantics -- all terms must match.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Search query string |
| `--namespace` | `-n` | string | | Filter by namespace |
| `--tier` | `-t` | string | | Filter by tier: `short`, `mid`, or `long` |
| `--limit` | | int | `20` | Maximum number of results |
| `--since` | | string | | Only memories created after this timestamp (RFC 3339) |
| `--until` | | string | | Only memories created before this timestamp (RFC 3339) |
| `--tags` | | string | | Filter by tags (comma-separated) |

---

### get

Retrieve a single memory by ID, including its links.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Memory ID |

---

### list

Browse memories with filters and pagination.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--namespace` | `-n` | string | | Filter by namespace |
| `--tier` | `-t` | string | | Filter by tier: `short`, `mid`, or `long` |
| `--limit` | | int | `20` | Maximum number of results |
| `--offset` | | int | `0` | Skip this many results (for pagination) |
| `--since` | | string | | Only memories created after this timestamp (RFC 3339) |
| `--until` | | string | | Only memories created before this timestamp (RFC 3339) |
| `--tags` | | string | | Filter by tags (comma-separated) |

---

### delete

Delete a memory by ID.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Memory ID |

---

### promote

Promote a memory to long-term tier (clears expiry).

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Memory ID |

---

### forget

Bulk delete memories matching a pattern, namespace, and/or tier.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--namespace` | `-n` | string | | Delete only in this namespace |
| `--pattern` | `-p` | string | | Delete memories matching this text pattern |
| `--tier` | `-t` | string | | Delete only memories of this tier |

---

### link

Link two memories with a typed relation.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional 1) | | string | required | Source memory ID |
| (positional 2) | | string | required | Target memory ID |
| `--relation` | `-r` | string | `related_to` | Relation type: `related_to`, `supersedes`, `contradicts`, `derived_from` |

---

### consolidate

Consolidate multiple memories into one long-term summary. Deletes source memories.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Comma-separated memory IDs (2-100) |
| `--title` | `-T` | string | required | Title for the consolidated memory |
| `--summary` | `-s` | string | required | Summary content for the consolidated memory |
| `--namespace` | `-n` | string | | Namespace for the consolidated memory |

---

### resolve

Resolve a contradiction -- mark one memory as superseding another. Creates a "supersedes" link and demotes the loser (priority=1, confidence=0.1).

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional 1) | | string | required | Winner memory ID (supersedes) |
| (positional 2) | | string | required | Loser memory ID (superseded) |

---

### mcp

Run as an MCP (Model Context Protocol) tool server over stdio.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--tier` | | string | `semantic` | Feature tier: `keyword`, `semantic`, `smart`, or `autonomous` |

---

### serve

Start the HTTP memory daemon with automatic background garbage collection.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--host` | | string | `127.0.0.1` | Listen address |
| `--port` | | int | `9077` | Listen port |

---

### shell

Launch the interactive memory REPL.

*(No command-specific flags)*

---

### sync

Sync memories between two SQLite database files.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | path | required | Path to the remote database to sync with |
| `--direction` | `-d` | string | `merge` | Sync direction: `pull`, `push`, or `merge` |

---

### auto-consolidate

Automatically group memories by namespace and primary tag, then consolidate groups into long-term summaries.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--namespace` | `-n` | string | | Only consolidate this namespace |
| `--short-only` | | bool | `false` | Only consolidate short-term memories |
| `--min-count` | | int | `3` | Minimum memories in a group to trigger consolidation |
| `--dry-run` | | bool | `false` | Show what would be consolidated without doing it |

---

### gc

Run garbage collection on expired memories.

*(No command-specific flags)*

---

### stats

Show memory statistics (counts, tiers, namespaces, links, DB size).

*(No command-specific flags)*

---

### namespaces

List all namespaces with memory counts.

*(No command-specific flags)*

---

### export

Export all memories and links as JSON to stdout.

Export format: `{"memories": [...], "links": [...], "count": N, "exported_at": "RFC3339"}`. Import expects the same structure (`count` and `exported_at` are optional).

*(No command-specific flags)*

---

### import

Import memories and links from JSON (reads from stdin).

*(No command-specific flags)*

---

### completions

Generate shell completions for the specified shell.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Shell type: `bash`, `zsh`, `fish`, `elvish`, `powershell` |

---

### man

Generate a roff man page to stdout. Pipe to `man -l -` to view.

*(No command-specific flags)*

---

### mine

Import memories from historical conversations (Claude, ChatGPT, Slack exports).

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | path | required | Path to the export file or directory |
| `--format` | `-f` | string | required | Export format: `claude`, `chatgpt`, `slack` |
| `--namespace` | `-n` | string | auto-detected | Namespace for imported memories |
| `--tier` | `-t` | string | `mid` | Memory tier for imported memories |
| `--min-messages` | | int | `3` | Minimum message count to import a conversation |
| `--dry-run` | | bool | `false` | Show what would be imported without writing |

---

### archive

Manage the memory archive. Has four sub-subcommands:

#### archive list

List archived memories.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--namespace` | `-n` | string | | Filter by namespace |
| `--limit` | | int | `50` | Maximum number of results |
| `--offset` | | int | `0` | Skip this many results (for pagination) |

#### archive restore

Restore an archived memory back to active status (expiry cleared).

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| (positional) | | string | required | Archived memory ID |

#### archive purge

Permanently delete archived memories.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--older-than-days` | | int | | Only purge entries older than N days (all if omitted) |

#### archive stats

Show archive statistics (count, size, oldest/newest).

*(No command-specific flags)*

---

## Memory Tiers Explained

| Tier | TTL | Use Case | Example |
|------|-----|----------|---------|
| **short** | 6 hours | What you're doing right now | "Currently debugging auth flow in login.rs" |
| **mid** | 7 days | This week's working knowledge | "Sprint goal: migrate to new API v2" |
| **long** | Forever | Permanent knowledge | "User prefers tabs over spaces" |

### Automatic Behaviors

- **TTL extension**: Every time a memory is recalled, its expiry extends (1 hour for short, 1 day for mid)
- **Auto-promotion**: A mid-tier memory recalled 5+ times automatically becomes long-term (expiry cleared)
- **Priority reinforcement**: Every 10 accesses, a memory's priority increases by 1 (max 10)
- **Garbage collection**: Expired memories are cleaned up every 30 minutes (optionally archived instead of deleted when `archive_on_gc = true` in `config.toml`)
- **Deduplication**: Storing a memory with the same title+namespace updates the existing one (tier never downgrades, priority takes the higher value)

## Namespaces

Namespaces isolate memories per project. If you omit `--namespace`, it auto-detects from the git remote URL or the current directory name.

```bash
# These are equivalent when run inside a git repo named "my-app":
ai-memory store -T "API uses REST" -c "..." --namespace my-app
ai-memory store -T "API uses REST" -c "..."  # auto-detects "my-app"
```

List all namespaces:

```bash
ai-memory namespaces
```

Filter recall or search to a specific namespace:

```bash
ai-memory recall "auth flow" --namespace my-app
```

## Memory Linking

Connect related memories with typed relations:

```bash
ai-memory link <source-id> <target-id> --relation supersedes
```

Valid relation types: `related_to` (default), `supersedes`, `contradicts`, `derived_from`.

- `related_to` (default) -- general association
- `supersedes` -- this memory replaces the other
- `contradicts` -- these memories conflict
- `derived_from` -- this memory was created from the other

When you `get` a memory, its links are shown alongside it:

```bash
ai-memory get <id>
# Shows the memory plus all its links
```

## Contradiction Detection

When you store a memory, ai-memory automatically checks for existing memories in the same namespace with similar titles. If potential contradictions are found, you get a warning:

```
stored: abc123 [long] (ns=my-app)
warning: 2 similar memories found in same namespace (potential contradictions)
```

In JSON mode (`--json`), the response includes `potential_contradictions` with the IDs of conflicting memories, so you can review and resolve them.

## Consolidation

After accumulating scattered memories about a topic, merge them into a single long-term summary:

```bash
ai-memory consolidate "id1,id2,id3" \
  -T "Auth system architecture" \
  -s "JWT tokens with refresh rotation, RBAC via middleware, sessions in Redis."
```

Consolidation:
- Creates a new long-term memory with the combined tags and highest priority
- Deletes the source memories
- Requires at least 2 IDs (max 100)

## Updating Memories

Update an existing memory by ID. Only the fields you provide are changed:

```bash
ai-memory update <id> -T "New title" -c "New content" --priority 8

# Set a custom expiration on an existing memory
ai-memory update <id> --expires-at "2026-06-01T00:00:00Z"
```

## Listing with Pagination

Browse memories with filters and pagination using `--offset`:

```bash
# First page (default)
ai-memory list --namespace my-app --limit 20

# Second page
ai-memory list --namespace my-app --limit 20 --offset 20

# Third page
ai-memory list --namespace my-app --limit 20 --offset 40
```

## Common Workflows

### Start of Session

Recall context relevant to what you're about to work on:

```bash
ai-memory recall "auth module refactor" --namespace my-app --limit 5
```

### Learning Something New

When you discover something important during a session:

```bash
ai-memory store \
  -T "Rate limiter uses token bucket" \
  -c "The rate limiter in middleware.rs uses a token bucket algorithm with 100 req/min default." \
  --tier mid --priority 6
```

### User Correction

When the user corrects you, store it as high-priority long-term:

```bash
ai-memory store \
  -T "User correction: always use snake_case for API fields" \
  -c "The user prefers snake_case for all JSON API response fields, not camelCase." \
  --tier long --priority 9 --source user
```

### Promoting a Memory

If a mid-tier memory turns out to be permanently valuable:

```bash
ai-memory promote <memory-id>
```

### Bulk Cleanup

Delete all short-term memories in a namespace:

```bash
ai-memory forget --namespace my-app --tier short
```

Delete memories matching a pattern:

```bash
ai-memory forget --pattern "deprecated API"
```

### Time-Filtered Queries

List memories created in the last week:

```bash
ai-memory list --since 2026-03-23T00:00:00Z
```

Search within a date range:

```bash
ai-memory search "migration" --since 2026-01-01T00:00:00Z --until 2026-03-01T00:00:00Z
```

### Importing Historical Conversations

Use the `mine` command to import memories from past conversations across platforms:

```bash
ai-memory mine /path/to/export --format claude     # Claude JSONL export
ai-memory mine /path/to/export.json --format chatgpt  # ChatGPT JSON export
ai-memory mine /path/to/slack-export/ --format slack   # Slack export directory
```

Supported formats (`--format` / `-f` flag values):
- `claude` -- Claude JSONL conversation export
- `chatgpt` -- ChatGPT JSON conversation export
- `slack` -- Slack workspace export directory (contains channel subdirectories with JSON files)

Additional flags: `--namespace` (override auto-detection), `--tier` (default: `mid`), `--min-messages` (minimum message count, default: 3), `--dry-run` (preview without writing).

This extracts key knowledge from historical conversations and stores them as memories, giving your AI assistant a head start with context it would otherwise have lost.

### Export and Backup

```bash
ai-memory export > memories-backup.json
```

Restore (preserves links):

```bash
ai-memory import < memories-backup.json
```

## Priority Guide

| Priority | When to Use |
|----------|-------------|
| 1-3 | Low-value context, temporary notes |
| 4-6 | Standard working knowledge (default is 5) |
| 7-8 | Important architecture decisions, user preferences |
| 9-10 | Critical corrections, hard-won lessons, "never forget this" |

## Confidence

Confidence (0.0 to 1.0) indicates how certain a memory is. Default is 1.0. Lower confidence for things that might change:

```bash
ai-memory store \
  -T "API might switch to GraphQL" \
  -c "Team is evaluating GraphQL migration." \
  --confidence 0.5
```

Confidence is factored into recall scoring (weight: 2.0) -- higher confidence memories rank higher.

## Source Tracking

Every memory tracks its source. Valid sources:

| Source | Meaning |
|--------|---------|
| `user` | Created by the user directly |
| `claude` | Created by Claude during a session |
| `hook` | Created by an automated hook |
| `api` | Created via the HTTP API (default for API) |
| `cli` | Created via the CLI (default for CLI) |
| `import` | Imported from a backup |
| `consolidation` | Created by consolidating other memories |
| `system` | System-generated |

## Tags

Tag memories for filtered retrieval:

```bash
ai-memory store -T "Deploy process" -c "..." --tags "devops,ci,deploy"
ai-memory recall "deployment" --tags "devops"
```

> **Tags format:** Tags are comma-separated strings in quotes: `--tags "devops,ci,deploy"`. Max 50 tags, 128 bytes each. Tags may contain hyphens and underscores but not spaces or commas.

## Interactive Shell

The `shell` command opens a REPL (read-eval-print loop) for browsing and managing memories interactively. Output uses color-coded tier labels and priority bars.

```bash
ai-memory shell
```

Available REPL commands:

| Command | Description |
|---------|-------------|
| `recall <context>` (or `r`) | Fuzzy recall with colored output |
| `search <query>` (or `s`) | Keyword search |
| `list [namespace]` (or `ls`) | List memories, optionally filtered by namespace |
| `get <id>` | Show full memory details as JSON |
| `stats` | Show memory statistics with tier breakdown |
| `namespaces` (or `ns`) | List all namespaces with counts |
| `delete <id>` (or `del`, `rm`) | Delete a memory |
| `help` (or `h`) | Show command help |
| `quit` (or `exit`, `q`) | Exit the shell |

Type commands without the `ai-memory` prefix. Use up/down arrows for history. Exit with `quit`, `exit`, `Ctrl+C`, or `Ctrl+D`. The prompt shows `ai-memory>`.

In the shell, tier labels are color-coded: red for short, yellow for mid, green for long. Priority is shown as a visual bar (`████░░░░░░`).

## Color Output

CLI output uses ANSI colors when connected to a terminal (auto-detected). Colors are suppressed when piping to a file or another command. Use `--json` for machine-parseable output.

Color scheme:
- **Tier labels**: red = short, yellow = mid, green = long
- **Priority bars**: `█████░░░░░` (green for 8+, yellow for 5-7, red for 1-4)
- **Titles**: bold
- **Content previews**: dim
- **Namespaces**: cyan
- **Memory IDs**: colored by tier

## Multi-Node Sync

Sync memories between two SQLite database files. Useful for keeping laptop and server in sync, or merging memories from different machines.

```bash
# Pull all memories from remote database into local
ai-memory sync /path/to/remote.db --direction pull

# Push local memories to remote database
ai-memory sync /path/to/remote.db --direction push

# Bidirectional merge -- both databases end up with all memories
ai-memory sync /path/to/remote.db --direction merge
```

Sync uses the same dedup-safe upsert as regular stores:
- Title+namespace conflicts are resolved by keeping the higher priority
- Tier never downgrades (a long memory stays long)
- Links are synced alongside memories

**Typical workflow** (laptop and server):

```bash
# On laptop, mount remote DB (e.g., via sshfs or rsync'd copy)
scp server:/var/lib/ai-memory/ai-memory.db /tmp/remote-memory.db

# Merge both ways
ai-memory sync /tmp/remote-memory.db --direction merge

# Copy merged remote back
scp /tmp/remote-memory.db server:/var/lib/ai-memory/ai-memory.db
```

## Auto-Consolidation

Automatically group memories by namespace and primary tag, then consolidate groups with enough members into a single long-term summary:

```bash
# Dry run -- see what would be consolidated
ai-memory auto-consolidate --dry-run

# Consolidate all namespaces (groups of 3+ memories)
ai-memory auto-consolidate

# Only short-term memories, minimum 5 per group
ai-memory auto-consolidate --short-only --min-count 5

# Only a specific namespace
ai-memory auto-consolidate --namespace my-project
```

How it works:
1. Lists all namespaces (or the specified one)
2. For each namespace, groups memories by their primary tag (first tag)
3. Groups with >= `min_count` members are consolidated into one long-term memory
4. The consolidated memory gets the title "Consolidated: tag (N memories)" and combines the content from all source memories
5. Source memories are deleted; the new memory inherits the highest priority and all tags

Use `--dry-run` first to preview what would be consolidated.

## Configurable TTL

Memory TTLs (time-to-live) can be customized per tier via `config.toml`. This lets you tune how long short-term and mid-term memories survive before expiring:

```toml
# ~/.config/ai-memory/config.toml
[ttl]
short_ttl_secs = 43200       # 12 hours (default: 21600 = 6 hours)
mid_ttl_secs = 1209600       # 14 days (default: 604800 = 7 days)
long_ttl_secs = 0            # never expires (default: 0)
short_extend_secs = 3600     # +1 hour on access (default: 3600)
mid_extend_secs = 86400      # +1 day on access (default: 86400)
```

Set any TTL to `0` to disable expiry for that tier. Values are clamped to a 10-year maximum.

CLI flags `--ttl-secs` and `--expires-at` on individual memories override the tier defaults.

> **Note:** Configuration is loaded once at process startup. Changes to `config.toml` require restarting the ai-memory process (MCP server, HTTP daemon, or CLI) to take effect.

## Archive Management

When `archive_on_gc = true` is set in `config.toml`, garbage collection archives expired memories instead of permanently deleting them. This gives you a safety net to recover accidentally expired memories.

### List archived memories

```bash
ai-memory archive list
```

### Restore an archived memory

```bash
ai-memory archive restore <id>
```

This moves the memory back to active status with its original tier and content intact. Restored memories have their expiry cleared (`expires_at` set to NULL) and become permanent.

### Purge the archive

```bash
ai-memory archive purge
ai-memory archive purge --older-than-days 30   # only purge archives older than 30 days
```

Permanently deletes archived memories. This cannot be undone.

### Archive statistics

```bash
ai-memory archive stats
```

Shows the total count, size, and date range of archived memories.

## Contradiction Resolution

When two memories conflict, resolve the contradiction by declaring a winner:

```bash
ai-memory resolve <winner_id> <loser_id>
```

This command:
1. Creates a "supersedes" link from the winner to the loser
2. Demotes the loser (priority set to 1, confidence set to 0.1)
3. Touches the winner (bumps access count, extends TTL)

The loser memory is not deleted -- it remains searchable but ranks much lower due to its reduced priority and confidence.

## Man Page

Generate and view the built-in man page:

```bash
# View immediately
ai-memory man | man -l -

# Install system-wide
ai-memory man | sudo tee /usr/local/share/man/man1/ai-memory.1 > /dev/null
```

## Troubleshooting

Common issues, their causes, and how to fix them.

### MCP server not showing tools after restart

**Symptom:** Your AI client does not list any `memory_*` tools after restarting.

**Cause:** The MCP configuration is in the wrong file, has a syntax error, or the `ai-memory` binary is not in PATH.

**Solution:**
1. Verify the config is in the correct file for your platform (e.g., `~/.claude.json` for Claude Code -- not `settings.json`).
2. Validate the JSON/TOML/YAML syntax (a trailing comma or missing bracket will silently fail).
3. Run `ai-memory mcp --tier semantic` manually in a terminal. If it prints `{"jsonrpc":"2.0",...}` lines, the binary works. If it errors, fix the error first.
4. Restart your AI client completely (not just a new conversation).

### "database locked" error

**Symptom:** Operations fail with `database is locked` or `SQLITE_BUSY`.

**Cause:** Multiple processes are writing to the same SQLite database simultaneously (e.g., the MCP server and a CLI command, or two MCP servers).

**Solution:**
1. Ensure only one MCP server instance runs per database file.
2. If using the CLI while the MCP server is running, they can share the same database -- SQLite WAL mode handles concurrent reads. Write conflicts are rare but possible under heavy load.
3. If the error persists, stop all ai-memory processes, then run `ai-memory gc` to checkpoint the WAL and release any stale locks.

### Embedding model download failed / hangs on first run

**Symptom:** The first `recall` at the `semantic` tier (or above) hangs for a long time or fails with a network error.

**Cause:** The semantic tier downloads the all-MiniLM-L6-v2 model (~90 MB) from Hugging Face on first use. Slow connections, firewalls, or proxy issues can block this.

**Solution:**
1. Ensure you have internet access and can reach `huggingface.co`.
2. If behind a corporate proxy, set `HTTPS_PROXY` before starting ai-memory.
3. Wait -- the first download can take a few minutes on slow connections. Subsequent runs use the cached model.
4. If the download is corrupted, delete the cached model directory (`~/.cache/huggingface/hub/models--sentence-transformers--all-MiniLM-L6-v2/`) and retry.

### "not found" when using a memory ID

**Symptom:** `ai-memory get <id>`, `update`, `delete`, or `promote` returns "not found".

**Cause:** The ID is wrong, the memory was garbage-collected (expired), or you are pointing at a different database file.

**Solution:**
1. Run `ai-memory list` to see current memory IDs.
2. Check that you are using the same `--db` path as the MCP server or previous CLI session.
3. If the memory expired, check the archive: `ai-memory archive list`. If `archive_on_gc = true`, expired memories are preserved there and can be restored with `ai-memory archive restore <id>`.

### "already exists in namespace" on store

**Symptom:** Storing a memory reports that a memory with the same title already exists.

**Cause:** ai-memory deduplicates by title+namespace. A memory with that exact title already exists in the same namespace.

**Solution:** This is expected behavior -- the existing memory is upserted (content updated, priority takes the higher value, tier never downgrades). If you want a separate memory, change the title to be distinct.

### Ollama connection refused (smart/autonomous tier)

**Symptom:** Smart or autonomous tier tools (`memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction`) fail with "connection refused" or timeout errors.

**Cause:** The smart and autonomous tiers require Ollama running locally to serve the LLM.

**Solution:**
1. Install Ollama: `curl -fsSL https://ollama.com/install.sh | sh`
2. Start it: `ollama serve`
3. Pull the required model: `ollama pull gemma4:e2b` (smart) or `ollama pull gemma4:e4b` (autonomous).
4. Verify it is running: `curl http://localhost:11434/api/tags` should return a JSON response.
5. If Ollama is on a non-default port or host, configure it in `~/.config/ai-memory/config.toml` under `[ollama]`.

### Binary not found in PATH after install

**Symptom:** Running `ai-memory` returns "command not found".

**Cause:** The install script placed the binary in a directory that is not in your shell's PATH, or you have not restarted your terminal.

**Solution:**
1. Restart your terminal (or run `source ~/.bashrc` / `source ~/.zshrc`).
2. If installed via `cargo install`, ensure `~/.cargo/bin` is in your PATH.
3. If installed via the install script, check where it placed the binary (usually `~/.local/bin` or `/usr/local/bin`) and add that directory to PATH if needed.
4. Verify: `which ai-memory` should print the binary path.

### Config changes not taking effect

**Symptom:** You edited `~/.config/ai-memory/config.toml` but the behavior has not changed.

**Cause:** Configuration is loaded once at process startup. The running MCP server or HTTP daemon is still using the old config.

**Solution:** Restart the ai-memory process. If running as an MCP server, restart your AI client (which restarts the MCP server). If running `ai-memory serve`, stop and restart it.

### Archive table growing too large

**Symptom:** The database file keeps growing and `ai-memory archive stats` shows thousands of archived memories.

**Cause:** With `archive_on_gc = true`, every expired memory is preserved in the archive instead of being deleted. Over time this accumulates.

**Solution:**
1. Purge old archives: `ai-memory archive purge --older-than-days 30`
2. Purge everything: `ai-memory archive purge`
3. To stop archiving entirely, set `archive_on_gc = false` in `config.toml` and restart.

### "VALIDATION_FAILED" errors

**Symptom:** Store or update fails with a validation error mentioning title length, content size, or invalid priority/confidence.

**Cause:** ai-memory enforces input limits to keep the database healthy.

**Solution:** Check your input against these limits:
- **Title:** must not be empty and must be under 1,024 bytes.
- **Content:** must be under 65,536 bytes (64 KB).
- **Priority:** integer from 1 to 10.
- **Confidence:** float from 0.0 to 1.0.
- **Tags:** each tag must be non-empty and under 128 bytes.

Adjust your input to fit within these constraints. If storing large content, split it into multiple memories or summarize it first.

---

## FAQ

**Q: Where is the database stored?**
A: By default, `ai-memory.db` in the current directory. Override with `--db /path/to/db` or the `AI_MEMORY_DB` environment variable.

**Q: Do I need to run the HTTP daemon?**
A: No. The MCP server and CLI commands work directly against the SQLite database. The HTTP daemon is an alternative interface that adds automatic background garbage collection.

**Q: What happens if I store a memory with a title that already exists in the same namespace?**
A: It upserts -- the content is updated, the priority takes the higher value, and the tier never downgrades (a long memory stays long).

**Q: How big can a memory be?**
A: Content is limited to 65,536 bytes (64 KB).

**Q: What is recency decay?**
A: A factor of `1/(1 + days_old * 0.1)` applied during recall ranking. A memory updated today gets a boost of 1.0, a memory from 10 days ago gets 0.5, and a memory from 100 days ago gets 0.09. This ensures recent memories are preferred when relevance is similar.

**Q: Can I use this with AI tools other than Claude Code?**
A: Absolutely. `ai-memory` is AI-agnostic. The MCP server speaks standard JSON-RPC over stdio and works with any MCP-compatible client -- Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others. The HTTP API at `http://127.0.0.1:9077/api/v1/` is completely platform-independent -- any tool, framework, or script that can make HTTP requests can store and recall memories.

**Q: Are there limits on bulk operations?**
A: Yes. Bulk create (`POST /memories/bulk`) and import (`POST /import`) are limited to 1,000 items per request to prevent abuse and memory exhaustion.

**Q: Is the HTTP API safe from injection attacks?**
A: Yes. All FTS queries are sanitized -- special characters are stripped and tokens are individually quoted before being passed to SQLite FTS5. The API also sanitizes error responses to avoid leaking internal database details to clients.
