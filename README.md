```
      _                 _
  ___| | __ _ _   _  __| | ___       _ __ ___   ___ _ __ ___   ___  _ __ _   _
 / __| |/ _` | | | |/ _` |/ _ \___  | '_ ` _ \ / _ \ '_ ` _ \ / _ \| '__| | | |
| (__| | (_| | |_| | (_| |  __/___| | | | | | |  __/ | | | | | (_) | |  | |_| |
 \___|_|\__,_|\__,_|\__,_|\___|     |_| |_| |_|\___|_| |_| |_|\___/|_|   \__, |
                                                                          |___/
```

[![CI](https://github.com/alphaonedev/claude-memory/actions/workflows/ci.yml/badge.svg)](https://github.com/alphaonedev/claude-memory/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![SQLite](https://img.shields.io/badge/sqlite-FTS5-003B57?logo=sqlite)](https://www.sqlite.org/)
[![Tests](https://img.shields.io/badge/tests-41-brightgreen)]()
[![MCP](https://img.shields.io/badge/MCP-8_tools-blueviolet)]()

**Persistent memory for Claude Code** -- short, mid, and long-term recall backed by SQLite + FTS5.

---

## What Is This?

`claude-memory` is a Rust daemon that gives Claude Code a real memory system. It stores knowledge in three tiers (short/mid/long), ranks recall by relevance + priority + access frequency + confidence + tier boost + recency decay, and auto-promotes frequently accessed memories to permanent storage. It integrates natively with Claude Code as an **MCP tool server**, and also exposes an HTTP API and CLI.

## MCP Integration (Primary Path)

The recommended way to use claude-memory with Claude Code is as an **MCP (Model Context Protocol) tool server**. This makes memory operations available as native tools that Claude can call directly.

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

This exposes **8 tools** to Claude Code:

| Tool | Description |
|------|-------------|
| `memory_store` | Store a new memory (deduplicates by title+namespace) |
| `memory_recall` | Recall memories relevant to a context (fuzzy OR search) |
| `memory_search` | Search memories by exact keyword match (AND semantics) |
| `memory_list` | List memories with optional filters |
| `memory_delete` | Delete a memory by ID |
| `memory_promote` | Promote a memory to long-term (permanent) |
| `memory_forget` | Bulk delete by pattern, namespace, or tier |
| `memory_stats` | Get memory store statistics |

## Features

- **MCP tool server** -- 8 native Claude Code tools over stdio JSON-RPC
- **Three-tier memory** -- short (6h TTL), mid (7d TTL), long (permanent)
- **Full-text search** -- SQLite FTS5 with ranked retrieval
- **Recency decay** -- `1/(1 + days_old * 0.1)` factor so recent memories rank higher
- **Smart recall** -- 6-factor scoring: FTS relevance + priority + access frequency + confidence + tier boost + recency decay
- **Schema validation** -- full input validation on every write path (title, content, namespace, source, tags, priority, confidence, expires_at, ttl_secs, relation, id)
- **Structured error types** -- typed errors: NOT_FOUND, VALIDATION_FAILED, DATABASE_ERROR, CONFLICT
- **Auto-promotion** -- memories accessed 5+ times automatically promote from mid to long
- **TTL extension** -- each recall extends expiry (1h for short, 1d for mid)
- **Priority reinforcement** -- every 10 accesses, priority increases by 1 (max 10)
- **Contradiction detection** -- warns when storing memories that conflict with existing ones
- **Memory linking** -- connect related memories with typed relations (related_to, supersedes, contradicts, derived_from)
- **Consolidation** -- merge multiple memories into a single long-term summary
- **Forget by pattern** -- bulk delete by namespace + FTS pattern + tier
- **Confidence scoring** -- 0.0-1.0 certainty score factored into ranking
- **Source tracking** -- tracks who created each memory (user, claude, hook, api, cli, import, consolidation, system)
- **Namespaces** -- isolate memories per project (auto-detected from git remote)
- **Deduplication** -- ON CONFLICT upsert by title+namespace (tier never downgrades)
- **Import/Export with links** -- full JSON roundtrip preserving memory links
- **Graceful shutdown** -- SIGTERM/SIGINT checkpoints WAL for clean exit
- **Deep health check** -- verifies DB accessibility and FTS5 integrity
- **Shell completions** -- bash, zsh, fish via `completions` command
- **Time filters** -- `--since`/`--until` on list and search
- **JSON output** -- `--json` flag on all CLI commands
- **Human-readable ages** -- "2h ago", "3d ago" in CLI output
- **Garbage collection** -- automatic background expiry every 30 minutes
- **20 API endpoints** -- full REST API on port 9077
- **19 CLI commands** -- complete CLI with identical capabilities
- **41 tests** -- 8 unit + 33 integration
- **Criterion benchmarks** -- insert, recall, search at 1K scale
- **GitHub Actions CI/CD** -- fmt, clippy, test, build on Ubuntu + macOS, release on tag

## Architecture

```
                        +---------------------+
                        |    Claude Code       |
                        |  (or any client)     |
                        +----+-----+-----+----+
                             |     |     |
              +--------------+     |     +--------------+
              |                    |                     |
        +-----v------+    +-------v--------+    +-------v-------+
        |    CLI      |    | MCP Server     |    |  HTTP API     |
        | claude-     |    | stdio JSON-RPC |    | 127.0.0.1:9077|
        | memory      |    | 8 tools        |    | /api/v1/*     |
        +-----+-------+    +-------+--------+    +-------+-------+
              |                     |                     |
              +----------+----------+----------+----------+
                         |                     |
                   +-----v------+        +-----v------+
                   | Validation |        |   Errors   |
                   | validate.rs|        |  errors.rs |
                   +-----+------+        +-----+------+
                         |                     |
                         +----------+----------+
                                    |
                          +---------v---------+
                          |   SQLite + FTS5   |
                          |   WAL mode        |
                          +---+-----+-----+---+
                              |     |     |
                         +----+  +--+--+  +----+
                         |short| | mid | | long|
                         |6h   | | 7d  | | inf |
                         +-----+ +-----+ +-----+
                              |     ^
                              |     | auto-promote
                              +-----+ (5+ accesses)
```

## Quick Start

### Option 1: MCP Server (Recommended)

```bash
# 1. Build and install
cargo install --path .

# 2. Add to Claude Code settings.json
# See MCP Integration section above

# 3. Claude Code now has memory tools natively
```

### Option 2: CLI

```bash
# 1. Build and install
cargo install --path .

# 2. Store your first memory
claude-memory store -T "Project uses Rust 2021 edition" \
  -c "The claude-memory project targets Rust edition 2021 with Axum for HTTP." \
  --tier long --priority 7

# 3. Recall it
claude-memory recall "what language and framework"
```

### Option 3: HTTP Daemon

```bash
# 1. Start the daemon
claude-memory serve &

# 2. Store via API
curl -X POST http://127.0.0.1:9077/api/v1/memories \
  -H 'Content-Type: application/json' \
  -d '{"title": "Test memory", "content": "It works.", "tier": "short"}'
```

## Recall Scoring Formula

```
score = (fts_relevance * -1)
      + (priority * 0.5)
      + (access_count * 0.1)
      + (confidence * 2.0)
      + tier_boost                                          -- long=3.0, mid=1.0, short=0.0
      + (1.0 / (1.0 + (days_since_update * 0.1)))          -- recency decay
```

## Documentation

| Guide | Audience |
|-------|----------|
| [Installation Guide](docs/INSTALL.md) | Getting it running (includes MCP setup) |
| [User Guide](docs/USER_GUIDE.md) | Claude Code users who want memory to work |
| [Developer Guide](docs/DEVELOPER_GUIDE.md) | Building on or contributing to claude-memory |
| [Admin Guide](docs/ADMIN_GUIDE.md) | Deploying, monitoring, and troubleshooting |
| [GitHub Pages](https://alphaonedev.github.io/claude-memory/) | Visual overview with animated diagrams |

## License

MIT
