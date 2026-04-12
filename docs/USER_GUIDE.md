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

## Zero Token Cost

Unlike built-in memory systems (Claude Code auto-memory, ChatGPT memory) that load your entire memory into every conversation, ai-memory uses **zero context tokens until recalled**. Only relevant memories come back, ranked by a 6-factor scoring algorithm. For Claude Code users: disable auto-memory (`"autoMemoryEnabled": false` in settings.json) to stop paying for 200+ lines of idle context.

## TOON Format (Token-Oriented Object Notation)

All recall, search, and list responses default to **TOON compact** format -- 79% smaller than JSON. Field names are declared once as a header, then values as pipe-delimited rows. This saves tokens on every recall.

- `format: "toon_compact"` (default) -- 79% smaller, omits timestamps
- `format: "toon"` -- 61% smaller, includes all fields
- `format: "json"` -- full JSON (use only when you need structured parsing)

## MCP Prompts

The server provides 2 MCP prompts via `prompts/list` that teach AI clients to use memory proactively:

- **recall-first** -- 9 behavioral rules: recall at session start, store corrections as long-term, use TOON format, tier strategy, dedup awareness
- **memory-workflow** -- Quick reference card for all tool usage patterns

## Feature Tiers

ai-memory supports 4 feature tiers, controlled by the `--tier` flag when starting the MCP server (e.g., `ai-memory mcp --tier semantic`). Each tier builds on the previous one:

| Tier | Recall Method | Extra Features | Requirements |
|------|--------------|----------------|--------------|
| **keyword** | FTS5 only | None | None (lightest) |
| **semantic** (default) | Hybrid: semantic + keyword blending | Embedding-based recall | HuggingFace embedding model (~256 MB RAM) |
| **smart** | Hybrid | Query expansion, auto-tagging, contradiction detection | Ollama + LLM (~1 GB RAM) |
| **autonomous** | Hybrid | Full autonomous memory management | Ollama + LLM (~4 GB RAM) |

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

Relation types:
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
ai-memory mine              # import from Claude conversation history
ai-memory mine --chatgpt    # import from ChatGPT exports
ai-memory mine --slack       # import from Slack exports
```

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
short_secs = 43200   # 12 hours (default: 21600 = 6 hours)
mid_secs = 1209600   # 14 days (default: 604800 = 7 days)
```

Long-term memories never expire regardless of TTL settings. CLI flags `--ttl-secs` and `--expires-at` on individual memories override the tier defaults.

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

This moves the memory back to active status with its original tier and content intact.

### Purge the archive

```bash
ai-memory archive purge
```

Permanently deletes all archived memories. This cannot be undone.

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
