# Claude Memory Integration

> **Note:** `ai-memory` is AI-agnostic and works with any MCP-compatible AI client (Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, OpenClaw, and others). This file contains **Claude Code-specific** integration instructions.

This project is `ai-memory` -- a persistent memory daemon that replaces Claude Code's built-in auto-memory. **Zero token cost until recall** -- unlike auto-memory which loads 200+ lines into every conversation, ai-memory uses zero context tokens until explicitly called. **TOON compact** is the default response format (79% smaller than JSON). 161 tests, 15/15 modules, 95%+ coverage. **LongMemEval benchmark: 97.8% R@5, 99.0% R@10, 99.8% R@20** (489/500, ICLR 2025 dataset).

## Step 1: Disable Auto-Memory

ai-memory replaces Claude Code's built-in auto-memory. Disable it to stop paying for idle context tokens on every message:

```json
// Add to ~/.claude/settings.json
{ "autoMemoryEnabled": false }
```

## Step 2: Configure MCP Server

Claude Code supports three MCP configuration scopes:

| Scope | File | Applies to |
|-------|------|------------|
| **User** (global) | `~/.claude.json` — add `mcpServers` key | All projects on your machine |
| **Project** (shared) | `.mcp.json` in project root | Everyone on the project |
| **Local** (private) | `~/.claude.json` — under `projects."/path".mcpServers` | One project, just you |

**User scope (recommended)** — merge into your existing `~/.claude.json`:

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

**Project scope** — create `.mcp.json` in your project root:

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

> **Important:** MCP servers do **not** go in `settings.json` or `settings.local.json`.

> The `--tier` flag is **required** in MCP args (config.toml tier is not used when launched by AI clients). Options: `keyword`, `semantic`, `smart`, `autonomous`. Smart and autonomous tiers require [Ollama](https://ollama.com).

> **Windows:** Use `%USERPROFILE%\.claude.json` and forward slashes in db paths: `"C:/Users/YourName/.claude/ai-memory.db"`.

This gives Claude Code 21 native tools: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`, `memory_update`, `memory_get`, `memory_link`, `memory_get_links`, `memory_consolidate`, `memory_capabilities`, `memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction`, `memory_archive_list`, `memory_archive_restore`, `memory_archive_purge`, `memory_archive_stats`.

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

### All 26 commands:
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
- `mine` -- import memories from historical conversations (Claude, ChatGPT, Slack exports)
- `archive` -- manage the memory archive (list, restore, purge, stats)

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
- **recall-first** -- System prompt with 8 rules: recall at session start, store corrections, TOON format, tier strategy, dedup awareness, namespace organization, capabilities check
- **memory-workflow** -- Quick reference card for all 21 tool usage patterns

These prompts teach AI clients to use memory proactively. The `recall-first` prompt supports an optional `namespace` argument for scoped recall.

### Recall scoring (6 factors + hybrid):
Memories are ranked by: FTS relevance + priority weight + access frequency + confidence + tier boost (long=3.0, mid=1.0) + recency decay (1/(1 + days_old * 0.1)). At the `semantic` tier and above, recall uses **hybrid scoring** that blends semantic (embedding similarity) and keyword (FTS5) results for better relevance.

### Automatic behaviors:
- TTL extension on recall: short +1h, mid +1d
- Auto-promotion: mid to long at 5 accesses (expiry cleared)
- Priority reinforcement: +1 every 10 accesses (max 10)
- Contradiction detection on store: warns about similar titles in same namespace
- Deduplication: upsert on title+namespace, tier never downgrades

### Configurable TTL and Archive

TTL defaults are configurable via the `[ttl]` section in `~/.config/ai-memory/config.toml`:

```toml
[ttl]
short_ttl_secs = 21600      # 6h  (0 = never expires)
mid_ttl_secs = 604800        # 7d  (0 = never expires)
long_ttl_secs = 0            # never expires
short_extend_secs = 3600     # +1h on recall
mid_extend_secs = 86400      # +1d on recall
archive_on_gc = true         # archive expired memories before GC deletes them
```

Set any TTL to `0` to disable expiry for that tier.

**Archive:** When `archive_on_gc = true` (default), expired memories are archived before GC deletion. Manage the archive via CLI:

```bash
ai-memory archive list              # browse archived memories
ai-memory archive restore <id>      # restore (expires_at cleared — becomes permanent)
ai-memory archive purge             # permanently delete archived memories
ai-memory archive stats             # archive size and counts
```

**Per-memory overrides:** Use `--expires-at` or `--ttl-secs` on `store`/`update` to override config defaults for individual memories.

> **Note:** Configuration is loaded once at process startup. Changes to `config.toml` require restarting the ai-memory process (MCP server, HTTP daemon, or CLI) to take effect.

---

## Using CLAUDE.md in Your Projects

> **This section is for users who want Claude Code to proactively use ai-memory in their own projects.** The instructions above describe how ai-memory works internally. The template below is what you put in **your** project's `CLAUDE.md` to instruct Claude to use ai-memory as its primary memory facility.

### Why CLAUDE.md?

Claude Code reads `CLAUDE.md` files at the start of every conversation. By adding ai-memory directives to your project's `CLAUDE.md`, you ensure Claude **always** recalls relevant context before starting work and **always** stores important findings — without being prompted.

### CLAUDE.md Template

Copy the following into your project's `CLAUDE.md` (create it if it doesn't exist). Customize the namespace and any project-specific notes.

````markdown
# Project — Claude Instructions

## AI Memory (MANDATORY)

Use `ai-memory` for persistent memory across conversations. This is NOT optional.

### On every conversation start:
1. Run `ai-memory recall "<topic>"` to check for relevant context before starting work
2. If the user references prior work, recall related memories first

### While working:
- Store important findings, decisions, and bug fixes as they happen — don't wait until the end
- Use namespace `my-project` for all project memories
- Default tier: `long`, default priority: `5` (use `9-10` for critical knowledge)

### When finishing work:
- Store a memory summarizing what was done, why, and any gotchas for next time
- Update existing memories if your work changes previously recorded facts

### Quick reference:
```bash
ai-memory recall "search query"                                      # fuzzy search
ai-memory search "exact keywords"                                    # precise match
ai-memory store -T "Title" -n my-project -t long -p 5 -c "content"  # store
ai-memory update <id> -c "new content"                               # update
ai-memory list -n my-project                                         # browse
```
````

### Where to Place CLAUDE.md

| Location | Scope | Use when |
|----------|-------|----------|
| `<project-root>/CLAUDE.md` | Project-wide | Default — applies to every Claude Code session in the project |
| `<subfolder>/CLAUDE.md` | Subdirectory | Adds directives when Claude works in that subdirectory |
| `~/.claude/CLAUDE.md` | User-global | Applies to all projects (put ai-memory directives here for universal recall) |

Claude Code loads all applicable `CLAUDE.md` files hierarchically — project root + any parent/child directories + user-global. The ai-memory directives in any of them will take effect.

### Claude Code Desktop

Claude Code Desktop reads the same `CLAUDE.md` files. The same template works for both CLI and Desktop — no separate configuration needed. Place `CLAUDE.md` in your project root and it applies to both.

### Combining with MCP

For the best experience, use **both** MCP and CLAUDE.md together:

1. **MCP** (in `~/.claude.json`) gives Claude native `memory_recall`, `memory_store`, etc. tools
2. **CLAUDE.md** (in your project) instructs Claude **when** and **how** to use those tools proactively

Without CLAUDE.md, Claude has the tools but may not use them unless asked. Without MCP, Claude falls back to the CLI commands in CLAUDE.md. Both together gives you proactive, tool-native memory.

### Session Hooks (Optional)

For automatic recall at session start without relying on CLAUDE.md directives, use the session-start hook in `hooks/session-start.sh`:

```json
// Add to ~/.claude/settings.json
{
  "hooks": {
    "PreToolUse": [
      { "command": "~/.claude/hooks/session-start.sh" }
    ]
  }
}
```

This auto-recalls memories matching the current working directory on every session start.
