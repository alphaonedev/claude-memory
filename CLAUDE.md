# Claude Memory Integration

This project is `claude-memory` -- a persistent memory daemon for Claude Code.

## Primary Integration: MCP Server

The recommended integration path is the **MCP tool server**. Configure in Claude Code `settings.json`:

```json
{
  "mcpServers": {
    "memory": {
      "command": "claude-memory",
      "args": ["--db", "/path/to/claude-memory.db", "mcp"]
    }
  }
}
```

This gives Claude Code 8 native tools: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`.

## Alternative: CLI Integration

The CLI binary is at `/opt/cybercommand/bin/claude-memory` (or `claude-memory` if in PATH).

### At session start -- recall relevant context:
```bash
claude-memory --db /opt/cybercommand/claude-memory.db recall "<current project or task context>"
```

### When you learn something important -- store it:
```bash
claude-memory --db /opt/cybercommand/claude-memory.db store \
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
claude-memory --db /opt/cybercommand/claude-memory.db store \
  --tier long --priority 9 --source user \
  --title "User correction: <what>" \
  --content "<the correction and why>"
```

### Namespace auto-detection:
If you omit `--namespace`, it auto-detects from the git remote or directory name.

### All commands:
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
- `gc` -- run garbage collection on expired memories
- `stats` -- overview of memory state (counts, tiers, namespaces, links, DB size)
- `namespaces` -- list all namespaces with memory counts
- `export` -- export all memories and links as JSON
- `import` -- import memories and links from JSON (stdin)
- `completions` -- generate shell completions (bash, zsh, fish)

### Recall scoring (6 factors):
Memories are ranked by: FTS relevance + priority weight + access frequency + confidence + tier boost (long=3.0, mid=1.0) + recency decay (1/(1 + days_old * 0.1)).

### Automatic behaviors:
- TTL extension on recall: short +1h, mid +1d
- Auto-promotion: mid to long at 5 accesses (expiry cleared)
- Priority reinforcement: +1 every 10 accesses (max 10)
- Contradiction detection on store: warns about similar titles in same namespace
- Deduplication: upsert on title+namespace, tier never downgrades
