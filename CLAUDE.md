# Claude Memory Integration

> **Note:** `ai-memory` is AI-agnostic and works with any MCP-compatible AI client (Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others). This file contains **Claude Code-specific** integration instructions.

This project is `ai-memory` -- a persistent memory daemon that replaces Claude Code's built-in auto-memory. **Zero token cost until recall** -- unlike auto-memory which loads 200+ lines into every conversation, ai-memory uses zero context tokens until explicitly called. **TOON compact** is the default response format (79% smaller than JSON). 158 tests, 14/14 modules, 95%+ coverage.

## Step 1: Disable Auto-Memory

ai-memory replaces Claude Code's built-in auto-memory. Disable it to stop paying for idle context tokens on every message:

```json
// Add to ~/.claude/settings.json
{ "autoMemoryEnabled": false }
```

## Step 2: Configure MCP Server

Configure in your project's `.mcp.json` or `~/.claude/.mcp.json`:

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "smart"]
    }
  }
}
```

> The `--tier` flag is **required** in MCP args (config.toml tier is not used when launched by AI clients). Options: `keyword`, `semantic`, `smart`, `autonomous`. Smart and autonomous tiers require [Ollama](https://ollama.com).

This gives Claude Code 17 native tools: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`, `memory_update`, `memory_get`, `memory_link`, `memory_get_links`, `memory_consolidate`, `memory_capabilities`, `memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction`.

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

### Feature tiers:
- `keyword` -- FTS5 only, no embedding model, lowest resource usage
- `semantic` (default) -- adds embedding-based recall with hybrid scoring (semantic + keyword blending)
- `smart` -- adds query expansion, auto-tagging, and contradiction detection (requires Ollama)
- `autonomous` -- full autonomous memory management (requires Ollama)

### TOON Format (Token-Oriented Object Notation):
All recall, search, and list responses default to **TOON compact** format -- 79% smaller than JSON. Field names declared once as a header, values as pipe-delimited rows. Pass `format: "json"` only when you need structured parsing.

Example TOON compact recall:
```
count:3|mode:hybrid
memories[id|title|tier|namespace|priority|score|tags]:
abc123|PostgreSQL 16|long|infra|9|0.763|postgres,db
def456|Redis cache|long|infra|8|0.541|redis,cache
```

### MCP Prompts (recall-first behavior):
The MCP server provides 2 prompts via `prompts/list`:
- **recall-first** -- System prompt with 9 rules: recall at session start, store corrections, TOON format, tier strategy, dedup awareness, namespace organization, capabilities check
- **memory-workflow** -- Quick reference card for all 17 tool usage patterns

These prompts teach AI clients to use memory proactively. The `recall-first` prompt supports an optional `namespace` argument for scoped recall.

### Recall scoring (6 factors + hybrid):
Memories are ranked by: FTS relevance + priority weight + access frequency + confidence + tier boost (long=3.0, mid=1.0) + recency decay (1/(1 + days_old * 0.1)). At the `semantic` tier and above, recall uses **hybrid scoring** that blends semantic (embedding similarity) and keyword (FTS5) results for better relevance.

### Automatic behaviors:
- TTL extension on recall: short +1h, mid +1d
- Auto-promotion: mid to long at 5 accesses (expiry cleared)
- Priority reinforcement: +1 every 10 accesses (max 10)
- Contradiction detection on store: warns about similar titles in same namespace
- Deduplication: upsert on title+namespace, tier never downgrades
