# Claude Memory Integration

> **Note:** `ai-memory` is AI-agnostic and works with any MCP-compatible AI client (Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others). This file contains **Claude Code-specific** integration instructions.

This project is `ai-memory` -- a persistent memory daemon. While it works with any MCP client, this file provides instructions specific to Claude Code.

## Primary Integration: MCP Server

The recommended integration path is the **MCP tool server**. Configure in `~/.claude/.mcp.json` (global config -- applies to all projects):

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp"]
    }
  }
}
```

> MCP server configuration does **not** go in `settings.json`, `settings.local.json`, or project-level `.mcp.json` files. Memory is a global service -- always configure it in `~/.claude/.mcp.json`.

This gives Claude Code 13 native tools: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`, `memory_update`, `memory_get`, `memory_link`, `memory_get_links`, `memory_consolidate`.

## Alternative: CLI Integration

The CLI binary is at `ai-memory` (or `ai-memory` if in PATH).

### At session start -- recall relevant context:
```bash
ai-memory --db /root/.claude/ai-memory.db recall "<current project or task context>"
```

### When you learn something important -- store it:
```bash
ai-memory --db /root/.claude/ai-memory.db store \
  --tier long \
  --namespace "<project-name>" \
  --title "What you learned" \
  --content "The details" \
  --source claude \
  --priority 7
```

### Memory tiers:
- `short` -- ephemeral, expires in 6 hours (debugging context, current task state)
- `mid` -- working knowledge, expires in 7 days (sprint goals, recent decisions)
- `long` -- permanent (architecture, user preferences, hard-won lessons)

### When the user corrects you -- store as high-priority long-term:
```bash
ai-memory --db /root/.claude/ai-memory.db store \
  --tier long --priority 9 --source user \
  --title "User correction: <what>" \
  --content "<the correction and why>"
```

### Namespace auto-detection:
If you omit `--namespace`, it auto-detects from the git remote or directory name.

### All 24 commands:
- `mcp` -- run as MCP tool server over stdio (primary integration path)
- `serve` -- start the HTTP daemon on port 9077
- `store` -- store a new memory (deduplicates by title+namespace)
- `update` -- update an existing memory by ID
- `recall` -- fuzzy OR search with ranked results + auto-touch
- `search` -- AND search for precise keyword matches
- `get` -- retrieve a single memory by ID (includes links)
- `list` -- browse memories with filters (namespace, tier, tags, date range)
- `delete` -- delete a memory by ID
- `promote` -- promote a memory to long-term (clears expiry)
- `forget` -- bulk delete by pattern + namespace + tier
- `link` -- link two memories (related_to, supersedes, contradicts, derived_from)
- `consolidate` -- merge multiple memories into one long-term summary
- `resolve` -- resolve a contradiction: mark one memory as superseding another (creates "supersedes" link, demotes loser to priority=1, confidence=0.1)
- `shell` -- interactive REPL with recall, search, list, get, stats, namespaces, delete (color output)
- `sync` -- sync memories between two database files (pull, push, or bidirectional merge with dedup-safe upsert)
- `auto-consolidate` -- automatically group memories by namespace+primary tag and consolidate groups >= min_count into long-term summaries (supports --dry-run, --short-only, --min-count, --namespace)
- `gc` -- run garbage collection on expired memories
- `stats` -- overview of memory state (counts, tiers, namespaces, links, DB size)
- `namespaces` -- list all namespaces with memory counts
- `export` -- export all memories and links as JSON
- `import` -- import memories and links from JSON (stdin)
- `completions` -- generate shell completions (bash, zsh, fish)
- `man` -- generate roff man page to stdout (pipe to `man -l -` to view)

### Recall scoring (6 factors):
Memories are ranked by: FTS relevance + priority weight + access frequency + confidence + tier boost (long=3.0, mid=1.0) + recency decay (1/(1 + days_old * 0.1)).

### Automatic behaviors:
- TTL extension on recall: short +1h, mid +1d
- Auto-promotion: mid to long at 5 accesses (expiry cleared)
- Priority reinforcement: +1 every 10 accesses (max 10)
- Contradiction detection on store: warns about similar titles in same namespace
- Deduplication: upsert on title+namespace, tier never downgrades
