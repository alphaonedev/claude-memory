---
sidebar_position: 2
title: Quickstart
description: Store and recall your first memory in 60 seconds.
---

# Quickstart

## 60-second tour

```bash
# 1. Store something
ai-memory store \
  -T "Project DB is PostgreSQL 16" \
  -c "Main database is Postgres 16 with pgvector extension." \
  --tier long --priority 8

# 2. Recall it
ai-memory recall "what database do we use"

# 3. List
ai-memory list --tier long

# 4. Stats
ai-memory stats
```

That's the loop: **store → recall → use**.

## Wire it into your AI client

ai-memory exposes itself as an **MCP server** (Model Context Protocol). Most AI assistants pick it up automatically once configured.

### Claude Code

Add to `~/.claude.json` (or project-local `.mcp.json`):

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

### ChatGPT / OpenAI Codex

```toml
# ~/.codex/config.toml
[mcp_servers.memory]
command = "ai-memory"
args = ["--db", "~/.local/share/ai-memory/memories.db", "mcp", "--tier", "semantic"]
enabled = true
```

### Other clients

ai-memory works with **xAI Grok**, **Cursor**, **OpenClaw**, **Gemini CLI**, and any MCP-compatible client. See the [README integration guide](https://github.com/alphaonedev/ai-memory-mcp#integration-methods) for the full set of configs.

## What just happened?

- Your AI assistant can now call MCP tools like `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_consolidate`, etc.
- Memories live in a single SQLite file at the path you set in `--db`
- Recall scoring blends **6 factors**: text relevance, priority, access frequency, confidence, tier boost, recency decay
- At `semantic` tier (default), the local MiniLM-L6-v2 embedding model is downloaded on first run (~100 MB) and cached forever

## Next

→ [Tiers](./tiers) — short/mid/long memory + keyword/semantic/smart/autonomous features
→ [Recall](./recall) — how scoring works
→ [Workflows](./workflows) — consolidate, link, supersede, archive
