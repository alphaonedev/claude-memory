# User Guide

## What Is This and Why Do I Need It?

`claude-memory` gives Claude Code persistent memory across sessions. Without it, every conversation starts from zero. With it, Claude can:

- Remember your project architecture, preferences, and past decisions
- Recall debugging context from yesterday
- Build up institutional knowledge over time
- Never repeat the same mistakes twice

Think of it as a brain for your AI assistant -- short-term for what you're doing right now, mid-term for this week's work, and long-term for things that should never be forgotten.

## MCP Integration (Recommended)

The easiest way to use claude-memory is as an **MCP tool server**. Once configured, Claude Code can store and recall memories natively without any manual CLI usage.

### Setup

Add to your Claude Code `settings.json`:

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

### How It Works

With MCP configured, Claude Code gains 8 memory tools:

- **memory_store** -- Store new knowledge (auto-deduplicates by title+namespace)
- **memory_recall** -- Recall relevant memories for the current context
- **memory_search** -- Search for specific memories by keyword
- **memory_list** -- Browse memories with filters
- **memory_delete** -- Remove a specific memory
- **memory_promote** -- Make a memory permanent (long-term)
- **memory_forget** -- Bulk delete memories by pattern
- **memory_stats** -- View memory statistics

Claude uses these tools automatically during conversations. You can also ask Claude directly: "Remember that we use PostgreSQL 15" or "What do you remember about our auth system?"

## Getting Started (CLI)

### Store Your First Memory

```bash
claude-memory store \
  -T "Project uses PostgreSQL 15" \
  -c "The main database is PostgreSQL 15 with pgvector for embeddings." \
  --tier long \
  --priority 7
```

That's it. The memory is now stored permanently (long tier) with priority 7/10.

### Recall Memories

```bash
claude-memory recall "database setup"
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
claude-memory search "PostgreSQL"
```

Search uses AND semantics -- all terms must match. Use this when you know exactly what you're looking for. Search uses the same 6-factor ranking but without the tier boost.

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
- **Garbage collection**: Expired memories are cleaned up every 30 minutes
- **Deduplication**: Storing a memory with the same title+namespace updates the existing one (tier never downgrades, priority takes the higher value)

## Namespaces

Namespaces isolate memories per project. If you omit `--namespace`, it auto-detects from the git remote URL or the current directory name.

```bash
# These are equivalent when run inside a git repo named "my-app":
claude-memory store -T "API uses REST" -c "..." --namespace my-app
claude-memory store -T "API uses REST" -c "..."  # auto-detects "my-app"
```

List all namespaces:

```bash
claude-memory namespaces
```

Filter recall or search to a specific namespace:

```bash
claude-memory recall "auth flow" --namespace my-app
```

## Memory Linking

Connect related memories with typed relations:

```bash
claude-memory link <source-id> <target-id> --relation supersedes
```

Relation types:
- `related_to` (default) -- general association
- `supersedes` -- this memory replaces the other
- `contradicts` -- these memories conflict
- `derived_from` -- this memory was created from the other

When you `get` a memory, its links are shown alongside it:

```bash
claude-memory get <id>
# Shows the memory plus all its links
```

## Contradiction Detection

When you store a memory, claude-memory automatically checks for existing memories in the same namespace with similar titles. If potential contradictions are found, you get a warning:

```
stored: abc123 [long] (ns=my-app)
warning: 2 similar memories found in same namespace (potential contradictions)
```

In JSON mode (`--json`), the response includes `potential_contradictions` with the IDs of conflicting memories, so you can review and resolve them.

## Consolidation

After accumulating scattered memories about a topic, merge them into a single long-term summary:

```bash
claude-memory consolidate "id1,id2,id3" \
  -T "Auth system architecture" \
  -s "JWT tokens with refresh rotation, RBAC via middleware, sessions in Redis."
```

Consolidation:
- Creates a new long-term memory with the combined tags and highest priority
- Deletes the source memories
- Requires at least 2 IDs (max 100)

## Common Workflows

### Start of Session

Recall context relevant to what you're about to work on:

```bash
claude-memory recall "auth module refactor" --namespace my-app --limit 5
```

### Learning Something New

When you discover something important during a session:

```bash
claude-memory store \
  -T "Rate limiter uses token bucket" \
  -c "The rate limiter in middleware.rs uses a token bucket algorithm with 100 req/min default." \
  --tier mid --priority 6
```

### User Correction

When the user corrects you, store it as high-priority long-term:

```bash
claude-memory store \
  -T "User correction: always use snake_case for API fields" \
  -c "The user prefers snake_case for all JSON API response fields, not camelCase." \
  --tier long --priority 9 --source user
```

### Promoting a Memory

If a mid-tier memory turns out to be permanently valuable:

```bash
claude-memory promote <memory-id>
```

### Bulk Cleanup

Delete all short-term memories in a namespace:

```bash
claude-memory forget --namespace my-app --tier short
```

Delete memories matching a pattern:

```bash
claude-memory forget --pattern "deprecated API"
```

### Time-Filtered Queries

List memories created in the last week:

```bash
claude-memory list --since 2026-03-23T00:00:00Z
```

Search within a date range:

```bash
claude-memory search "migration" --since 2026-01-01T00:00:00Z --until 2026-03-01T00:00:00Z
```

### Export and Backup

```bash
claude-memory export > memories-backup.json
```

Restore (preserves links):

```bash
claude-memory import < memories-backup.json
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
claude-memory store \
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
claude-memory store -T "Deploy process" -c "..." --tags "devops,ci,deploy"
claude-memory recall "deployment" --tags "devops"
```

## FAQ

**Q: Where is the database stored?**
A: By default, `claude-memory.db` in the current directory. Override with `--db /path/to/db` or the `CLAUDE_MEMORY_DB` environment variable.

**Q: Do I need to run the HTTP daemon?**
A: No. The MCP server and CLI commands work directly against the SQLite database. The HTTP daemon is an alternative interface that adds automatic background garbage collection.

**Q: What happens if I store a memory with a title that already exists in the same namespace?**
A: It upserts -- the content is updated, the priority takes the higher value, and the tier never downgrades (a long memory stays long).

**Q: How big can a memory be?**
A: Content is limited to 65,536 bytes (64 KB).

**Q: What is recency decay?**
A: A factor of `1/(1 + days_old * 0.1)` applied during recall ranking. A memory updated today gets a boost of 1.0, a memory from 10 days ago gets 0.5, and a memory from 100 days ago gets 0.09. This ensures recent memories are preferred when relevance is similar.

**Q: Can I use this with tools other than Claude Code?**
A: Yes. The MCP server speaks standard JSON-RPC over stdio. The HTTP API at `http://127.0.0.1:9077/api/v1/` is language-agnostic. Any tool that can make HTTP requests or speak JSON-RPC can store and recall memories.
