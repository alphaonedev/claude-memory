```
        _
   __ _(_)      _ __ ___   ___ _ __ ___   ___  _ __ _   _
  / _` | |___  | '_ ` _ \ / _ \ '_ ` _ \ / _ \| '__| | | |
 | (_| | |___| | | | | | |  __/ | | | | | (_) | |  | |_| |
  \__,_|_|     |_| |_| |_|\___|_| |_| |_|\___/|_|   \__, |
                universal AI memory                   |___/
```

[![CI](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![SQLite](https://img.shields.io/badge/sqlite-FTS5-003B57?logo=sqlite)](https://www.sqlite.org/)
[![Tests](https://img.shields.io/badge/tests-40-brightgreen)]()
[![MCP](https://img.shields.io/badge/MCP-17_tools-blueviolet)]()

**ai-memory is a persistent memory system for AI assistants.** It works with **any AI that supports MCP** -- Claude, ChatGPT, Grok, Llama, and more. It stores what your AI learns in a local SQLite database, ranks memories by relevance when recalling, and auto-promotes important knowledge to permanent storage. Install it once, and every AI assistant you use remembers your architecture, your preferences, your corrections -- forever.

---

## Compatible AI Platforms

ai-memory integrates with any AI platform that supports the **Model Context Protocol (MCP)**. MCP is the universal standard for connecting AI assistants to external tools and data sources.

| Platform | Integration Method | Config Format | Status |
|----------|-------------------|---------------|--------|
| **Claude Code** (Anthropic) | MCP stdio | JSON (`~/.claude/.mcp.json`) | Fully supported |
| **Codex CLI** (OpenAI) | MCP stdio | TOML (`~/.codex/config.toml`) | Fully supported |
| **Gemini CLI** (Google) | MCP stdio | JSON (`~/.gemini/settings.json`) | Fully supported |
| **Grok** (xAI) | MCP remote HTTPS | API-level | Fully supported |
| **Cursor IDE** | MCP stdio | JSON (`~/.cursor/mcp.json`) | Fully supported |
| **Windsurf** (Codeium) | MCP stdio | JSON (`~/.codeium/windsurf/mcp_config.json`) | Fully supported |
| **Continue.dev** | MCP stdio | YAML (`~/.continue/config.yaml`) | Fully supported |
| **Llama Stack** (META) | MCP remote HTTP | YAML / Python SDK | Fully supported |
| **Any MCP client** | MCP stdio or HTTP | Varies | Universal |

MCP is the primary integration layer. For AI platforms that do not yet support MCP natively, the **HTTP API** (20 endpoints on localhost) and the **CLI** (24 commands) provide universal access -- any AI, script, or automation that can make HTTP calls or run shell commands can use ai-memory.

---

## Install in 60 Seconds

You need Rust installed. That is it. No Docker, no Python, no Node.

**Step 1: Install Rust** (skip if you already have it)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the prompts, then restart your terminal (or run `source ~/.cargo/env`).

**Step 2: Install ai-memory**

```bash
cargo install --git https://github.com/alphaonedev/ai-memory-mcp.git
```

This compiles the binary and puts it in your PATH. It takes a minute or two.

**Step 3: Connect your AI**

Configuration varies by platform. Find yours below:

<details>
<summary><strong>Claude Code</strong> (Anthropic)</summary>

Add to `~/.claude/.mcp.json`:

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

</details>

<details>
<summary><strong>OpenAI Codex CLI</strong></summary>

Add to `~/.codex/config.toml` (global) or `.codex/config.toml` (project):

```toml
[mcp_servers.memory]
command = "ai-memory"
args = ["--db", "~/.local/share/ai-memory/memories.db", "mcp"]
enabled = true
```

Or add via CLI: `codex mcp add memory -- ai-memory --db ~/.local/share/ai-memory/memories.db mcp`

> **Notes:** Codex uses TOML format with underscored key `mcp_servers`. Use `enabled_tools` to restrict which memory tools are exposed. Use `/mcp` in the TUI to view server status. See [Codex MCP docs](https://developers.openai.com/codex/mcp).

</details>

<details>
<summary><strong>Google Gemini CLI</strong></summary>

Add to `~/.gemini/settings.json` (user) or `.gemini/settings.json` (project):

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.local/share/ai-memory/memories.db", "mcp"],
      "timeout": 30000
    }
  }
}
```

Or add via CLI: `gemini mcp add memory ai-memory -- --db ~/.local/share/ai-memory/memories.db mcp`

> **Notes:** Avoid underscores in server names (use hyphens). Tool names are auto-prefixed as `mcp_memory_<toolName>`. Gemini sanitizes environment variables -- explicitly declare needed vars in the `env` field. Add `"trust": true` to skip confirmation prompts. See [Gemini CLI MCP docs](https://geminicli.com/docs/tools/mcp-server/).

</details>

<details>
<summary><strong>Cursor IDE</strong></summary>

Add to `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (project-level):

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.local/share/ai-memory/memories.db", "mcp"]
    }
  }
}
```

> **Notes:** Restart Cursor after editing `mcp.json`. Verify server status in Cursor Settings > Tools & MCP (green dot = connected). Cursor supports `env`, `envFile`, and `${env:VAR}` interpolation. ~40 tool limit across all MCP servers. See [Cursor MCP docs](https://cursor.com/docs/context/mcp).

</details>

<details>
<summary><strong>Windsurf</strong> (Codeium)</summary>

Add to `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.local/share/ai-memory/memories.db", "mcp"]
    }
  }
}
```

</details>

<details>
<summary><strong>Continue.dev</strong></summary>

Add to `~/.continue/config.yaml`:

```yaml
mcpServers:
  - name: memory
    command: ai-memory
    args:
      - "--db"
      - "~/.local/share/ai-memory/memories.db"
      - "mcp"
```

</details>

<details>
<summary><strong>xAI Grok</strong> (API-level, remote MCP)</summary>

Grok connects to MCP servers over HTTPS (remote only, no stdio). Start ai-memory as an HTTP server behind HTTPS:

```bash
ai-memory serve --host 127.0.0.1 --port 9077
# Expose via HTTPS reverse proxy (nginx, caddy, cloudflare tunnel, etc.)
```

Then add the MCP server to your Grok API call:

```bash
curl https://api.x.ai/v1/responses \
  -H "Authorization: Bearer $XAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "grok-3",
    "tools": [{
      "type": "mcp",
      "server_url": "https://your-server.example.com/mcp",
      "server_label": "memory",
      "server_description": "Persistent AI memory with recall and search"
    }],
    "input": "What do you remember about our project?"
  }'
```

**Requirements:** HTTPS required. Supports Streamable HTTP and SSE transports. See [xAI Remote MCP docs](https://docs.x.ai/developers/tools/remote-mcp).

</details>

<details>
<summary><strong>META Llama</strong> (via Llama Stack)</summary>

Llama Stack registers MCP servers as toolgroups. Start the HTTP server first:

```bash
ai-memory serve --host 127.0.0.1 --port 9077
```

Then register via the Python SDK:

```python
client.toolgroups.register(
    provider_id="model-context-protocol",
    toolgroup_id="mcp::memory",
    mcp_endpoint={"uri": "http://localhost:9077/sse"}
)
```

</details>

<details>
<summary><strong>Any other MCP client</strong></summary>

ai-memory speaks MCP over stdio (JSON-RPC 2.0). Point your client at:

```
command: ai-memory
args: ["--db", "/path/to/ai-memory.db", "mcp"]
```

For HTTP-only clients, start the REST API:

```bash
ai-memory serve
# 20 endpoints at http://127.0.0.1:9077/api/v1/
```

</details>

**Step 4: Done. Test it.**

Restart your AI assistant. If using MCP, it now has 17 memory tools. Ask it: "Store a memory that my favorite language is Rust." Then in a new conversation, ask: "What is my favorite language?" It will remember.

---

## What Does It Do?

AI assistants forget everything between conversations. ai-memory fixes that.

It runs as an MCP (Model Context Protocol) tool server -- a background process that your AI talks to natively. When your AI learns something important, it stores it. When it needs context, it recalls relevant memories ranked by a 6-factor scoring algorithm. Memories live in three tiers:

- **Short-term** (6 hours) -- throwaway context like current debugging state
- **Mid-term** (7 days) -- working knowledge like sprint goals and recent decisions
- **Long-term** (permanent) -- architecture, user preferences, hard-won lessons

Memories that keep getting accessed automatically promote from mid to long-term. Each recall extends the TTL. Priority increases with usage. The system is self-curating.

Beyond MCP, ai-memory also exposes a full HTTP REST API (20 endpoints on port 9077) and a complete CLI (24 commands) for direct interaction, scripting, and integration with any AI platform or tool.

---

## Features

### Core
- **MCP tool server** -- 17 tools over stdio JSON-RPC, compatible with any MCP client
- **Three-tier memory** -- short (6h TTL), mid (7d TTL), long (permanent)
- **Full-text search** -- SQLite FTS5 with ranked retrieval
- **Hybrid recall** -- FTS5 keyword + cosine similarity blending when running at semantic tier or above
- **6-factor recall scoring** -- FTS relevance + priority + access frequency + confidence + tier boost + recency decay
- **Auto-promotion** -- memories accessed 5+ times promote from mid to long
- **TTL extension** -- each recall extends expiry (short +1h, mid +1d)
- **Priority reinforcement** -- +1 every 10 accesses (max 10)
- **Contradiction detection** -- warns when storing memories that conflict with existing ones
- **Deduplication** -- upsert on title+namespace, tier never downgrades
- **Confidence scoring** -- 0.0-1.0 certainty factored into ranking

### Organization
- **Namespaces** -- isolate memories per project (auto-detected from git remote)
- **Memory linking** -- typed relations: related_to, supersedes, contradicts, derived_from
- **Consolidation** -- merge multiple memories into a single long-term summary
- **Auto-consolidation** -- group by namespace+tag, auto-merge groups above threshold
- **Contradiction resolution** -- mark one memory as superseding another, demote the loser
- **Forget by pattern** -- bulk delete by namespace + FTS pattern + tier
- **Source tracking** -- tracks origin: user, claude, hook, api, cli, import, consolidation, system
- **Tagging** -- comma-separated tags with filter support

### Interfaces
- **20 HTTP endpoints** -- full REST API on 127.0.0.1:9077 (works with any AI or tool)
- **24 CLI commands** -- complete CLI with identical capabilities
- **17 MCP tools** -- native integration for any MCP-compatible AI
- **Interactive REPL shell** -- recall, search, list, get, stats, namespaces, delete with color output
- **JSON output** -- `--json` flag on all CLI commands

### Operations
- **Multi-node sync** -- pull, push, or bidirectional merge between database files
- **Import/Export** -- full JSON roundtrip preserving memory links
- **Garbage collection** -- automatic background expiry every 30 minutes
- **Graceful shutdown** -- SIGTERM/SIGINT checkpoints WAL for clean exit
- **Deep health check** -- verifies DB accessibility and FTS5 integrity
- **Shell completions** -- bash, zsh, fish
- **Man page** -- `ai-memory man` generates roff to stdout
- **Time filters** -- `--since`/`--until` on list and search
- **Human-readable ages** -- "2h ago", "3d ago" in CLI output
- **Color CLI output** -- ANSI tier labels (red/yellow/green), priority bars, bold titles, cyan namespaces

### Quality
- **40 tests** -- unit + integration
- **Criterion benchmarks** -- insert, recall, search at 1K scale
- **GitHub Actions CI/CD** -- fmt, clippy, test, build on Ubuntu + macOS, release on tag

### ML and LLM Dependencies (semantic tier+)
- **candle-core, candle-nn, candle-transformers** -- Hugging Face Candle ML framework for native Rust inference
- **hf-hub** -- download models from Hugging Face Hub
- **tokenizers** -- Hugging Face tokenizers for text preprocessing
- **instant-distance** -- approximate nearest neighbor search
- **reqwest** -- HTTP client for Ollama API communication (smart/autonomous tiers)

---

## Architecture

```
    +-------------+   +-------------+   +-------------+   +-------------+
    | Claude Code |   |   ChatGPT   |   |    Grok     |   |   Llama     |
    |  (Anthropic)|   |   (OpenAI)  |   |    (xAI)    |   |   (META)    |
    +------+------+   +------+------+   +------+------+   +------+------+
           |                 |                 |                 |
           +--------+--------+--------+--------+--------+--------+
                    |                 |                 |
              +-----v------+  +------v--------+  +----v----------+
              |    CLI      |  | MCP Server    |  |  HTTP API     |
              | 24 commands |  | stdio JSON-RPC|  | 127.0.0.1:9077|
              +-----+------+  +------+--------+  +----+----------+
                    |                 |                 |
                    +--------+--------+--------+--------+
                             |                 |
                       +-----v------+    +-----v------+
                       | Validation |    |   Errors   |
                       | validate.rs|    |  errors.rs |
                       +-----+------+    +-----+------+
                             |                 |
                             +--------+--------+
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

     Embedding Pipeline (semantic tier+):
     +--------------------------------------------------+
     | Candle ML Framework (candle-core, candle-nn)      |
     |   all-MiniLM-L6-v2 model (384-dim vectors)       |
     |   Vectors stored as BLOBs in SQLite               |
     |   Hybrid recall: FTS5 keyword + cosine similarity |
     +--------------------------------------------------+

     LLM Pipeline (smart/autonomous tier):
     +--------------------------------------------------+
     | Ollama (local)                                    |
     |   smart: Gemma 4 E2B (query expansion, tagging)  |
     |   autonomous: Gemma 4 E4B + cross-encoder rerank |
     +--------------------------------------------------+
```

---

## Integration Methods

### MCP (Primary -- for MCP-compatible AI platforms)

MCP is the recommended integration. Your AI gets 17 native memory tools with zero glue code. Configure the MCP server in your AI platform's config:

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

### HTTP API (Universal -- for any AI or tool)

Start the HTTP server for REST API access. Any AI, script, or automation that can make HTTP calls can use this:

```bash
ai-memory serve
# 20 endpoints at http://127.0.0.1:9077/api/v1/
```

### CLI (Universal -- for scripting and direct use)

The CLI works standalone or as a building block for AI integrations that run shell commands:

```bash
ai-memory store --tier long --title "Architecture decision" --content "We use PostgreSQL"
ai-memory recall "database choice"
ai-memory search "PostgreSQL"
```

---

## Feature Tiers

ai-memory supports 4 feature tiers, selected at startup with `ai-memory mcp --tier <tier>`. Higher tiers add ML capabilities at the cost of disk and RAM:

| Tier | Recall Method | Extra Capabilities | Approx. Overhead |
|------|---------------|-------------------|-----------------|
| **keyword** | FTS5 only | Baseline 13 tools | 0 MB |
| **semantic** | FTS5 + cosine similarity (hybrid) | MiniLM-L6-v2 embeddings, 384-dim vectors | ~256 MB |
| **smart** | Hybrid + LLM query expansion | + Gemma 4 E2B via Ollama: `memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction` | ~1 GB |
| **autonomous** | Hybrid + LLM expansion + cross-encoder reranking | + Gemma 4 E4B via Ollama, cross-encoder reranking | ~4 GB |

**Keyword tier** is the default and requires no additional dependencies. **Semantic tier** bundles the Candle ML framework and downloads the all-MiniLM-L6-v2 model on first run (~80 MB). **Smart** and **autonomous** tiers require Ollama running locally with the appropriate Gemma 4 models installed (Google, USA-only license).

```bash
# Keyword (default)
ai-memory mcp

# Semantic -- hybrid recall with embeddings
ai-memory mcp --tier semantic

# Smart -- adds LLM-powered query expansion, auto-tagging, contradiction detection
ai-memory mcp --tier smart

# Autonomous -- adds cross-encoder reranking
ai-memory mcp --tier autonomous
```

The `memory_capabilities` tool reports the active tier, loaded models, and available capabilities at runtime.

---

## MCP Tools

These 17 tools are available to any MCP-compatible AI when configured as an MCP server:

| Tool | Description |
|------|-------------|
| `memory_store` | Store a new memory (deduplicates by title+namespace, reports contradictions) |
| `memory_recall` | Recall memories relevant to a context (fuzzy OR search, ranked by 6 factors) |
| `memory_search` | Search memories by exact keyword match (AND semantics) |
| `memory_list` | List memories with optional filters (namespace, tier, tags, date range) |
| `memory_get` | Get a specific memory by ID with its links |
| `memory_update` | Update an existing memory by ID (partial update) |
| `memory_delete` | Delete a memory by ID |
| `memory_promote` | Promote a memory to long-term (permanent, clears expiry) |
| `memory_forget` | Bulk delete by pattern, namespace, or tier |
| `memory_link` | Create a typed link between two memories |
| `memory_get_links` | Get all links for a memory |
| `memory_consolidate` | Merge multiple memories into one long-term summary |
| `memory_stats` | Get memory store statistics |
| `memory_capabilities` | Report active feature tier, loaded models, and available capabilities |
| `memory_expand_query` | Use LLM to expand search query into related terms (smart+ tier) |
| `memory_auto_tag` | Use LLM to auto-generate tags for a memory (smart+ tier) |
| `memory_detect_contradiction` | Use LLM to check if two memories contradict (smart+ tier) |

---

## HTTP API

20 endpoints on `127.0.0.1:9077`. Start with `ai-memory serve`.

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/v1/health` | Health check (verifies DB + FTS5 integrity) |
| GET | `/api/v1/memories` | List memories (supports namespace, tier, tags, since, until, limit) |
| POST | `/api/v1/memories` | Create a memory |
| POST | `/api/v1/memories/bulk` | Bulk create memories (with limits) |
| GET | `/api/v1/memories/{id}` | Get a memory by ID |
| PUT | `/api/v1/memories/{id}` | Update a memory by ID |
| DELETE | `/api/v1/memories/{id}` | Delete a memory by ID |
| POST | `/api/v1/memories/{id}/promote` | Promote a memory to long-term |
| GET | `/api/v1/search` | AND keyword search |
| GET | `/api/v1/recall` | Recall by context (GET with query params) |
| POST | `/api/v1/recall` | Recall by context (POST with JSON body) |
| POST | `/api/v1/forget` | Bulk delete by pattern/namespace/tier |
| POST | `/api/v1/consolidate` | Consolidate memories into one |
| POST | `/api/v1/links` | Create a link between memories |
| GET | `/api/v1/links/{id}` | Get links for a memory |
| GET | `/api/v1/namespaces` | List all namespaces |
| GET | `/api/v1/stats` | Memory store statistics |
| POST | `/api/v1/gc` | Trigger garbage collection |
| GET | `/api/v1/export` | Export all memories + links as JSON |
| POST | `/api/v1/import` | Import memories + links from JSON |

---

## CLI Commands

24 commands. Run `ai-memory <command> --help` for details on any command.

| Command | Description |
|---------|-------------|
| `mcp` | Run as MCP tool server over stdio (primary integration path) |
| `serve` | Start the HTTP daemon on port 9077 |
| `store` | Store a new memory (deduplicates by title+namespace) |
| `update` | Update an existing memory by ID |
| `recall` | Fuzzy OR search with ranked results + auto-touch |
| `search` | AND search for precise keyword matches |
| `get` | Retrieve a single memory by ID (includes links) |
| `list` | Browse memories with filters (namespace, tier, tags, date range) |
| `delete` | Delete a memory by ID |
| `promote` | Promote a memory to long-term (clears expiry) |
| `forget` | Bulk delete by pattern + namespace + tier |
| `link` | Link two memories (related_to, supersedes, contradicts, derived_from) |
| `consolidate` | Merge multiple memories into one long-term summary |
| `resolve` | Resolve a contradiction: mark winner, demote loser |
| `shell` | Interactive REPL with color output |
| `sync` | Sync memories between two database files (pull/push/merge) |
| `auto-consolidate` | Group memories by namespace+tag, merge groups above threshold |
| `gc` | Run garbage collection on expired memories |
| `stats` | Overview of memory state (counts, tiers, namespaces, links, DB size) |
| `namespaces` | List all namespaces with memory counts |
| `export` | Export all memories and links as JSON |
| `import` | Import memories and links from JSON (stdin) |
| `completions` | Generate shell completions (bash, zsh, fish) |
| `man` | Generate roff man page to stdout |

The top-level `ai-memory` binary also accepts global flags:

| Flag | Description |
|------|-------------|
| `--db <path>` | Database path (default: `ai-memory.db`, or `$AI_MEMORY_DB`) |
| `--json` | JSON output on all commands |

The `mcp` subcommand accepts an additional flag:

| Flag | Description |
|------|-------------|
| `--tier <keyword\|semantic\|smart\|autonomous>` | Feature tier (default: `keyword`). See [Feature Tiers](#feature-tiers). |

---

## Recall Scoring

Every recall query ranks memories by 6 factors:

```
score = (fts_relevance * -1)
      + (priority * 0.5)
      + (MIN(access_count, 50) * 0.1)
      + (confidence * 2.0)
      + tier_boost
      + recency_decay
```

| Factor | Weight | Notes |
|--------|--------|-------|
| FTS relevance | -1.0x | SQLite FTS5 rank (negative = better match) |
| Priority | 0.5x | User-assigned 1-10 scale |
| Access count | 0.1x | How often recalled (capped at 50 for scoring) |
| Confidence | 2.0x | 0.0-1.0 certainty score |
| Tier boost | +3.0 / +1.0 / +0.0 | long / mid / short |
| Recency decay | `1/(1 + days*0.1)` | Recent memories rank higher |

---

## Memory Tiers

| Tier | TTL | Use Case | Examples |
|------|-----|----------|----------|
| `short` | 6 hours | Throwaway context | Current debugging state, temp variables, error traces |
| `mid` | 7 days | Working knowledge | Sprint goals, recent decisions, current branch purpose |
| `long` | Permanent | Hard-won knowledge | Architecture, user preferences, corrections, conventions |

### Automatic Behaviors

- **TTL extension on recall**: short memories get +1 hour, mid memories get +1 day
- **Auto-promotion**: mid-tier memories accessed 5+ times promote to long (expiry cleared)
- **Priority reinforcement**: every 10 accesses, priority increases by 1 (capped at 10)
- **Contradiction detection**: warns when a new memory conflicts with an existing one in the same namespace
- **Deduplication**: upsert on title+namespace; tier never downgrades on update

---

## Security

ai-memory includes hardening across all input paths:

- **Transaction safety** -- all multi-step database operations use transactions; no partial writes on failure
- **FTS injection prevention** -- user input is sanitized before reaching FTS5 queries; special characters are escaped
- **Error sanitization** -- internal database paths and system details are stripped from error responses; clients see structured error types (NOT_FOUND, VALIDATION_FAILED, DATABASE_ERROR, CONFLICT)
- **Body size limits** -- HTTP request bodies are capped at 50 MB via Axum's DefaultBodyLimit
- **Bulk operation limits** -- bulk create endpoints enforce maximum batch sizes to prevent resource exhaustion
- **CORS** -- permissive CORS layer enabled for localhost development workflows
- **Input validation** -- every write path validates title length, content length, namespace format, source values, priority range (1-10), confidence range (0.0-1.0), tag format, tier values, relation types, and ID format
- **Link validation in sync** -- all links are validated (both IDs, relation type, no self-links) before import during sync operations
- **Thread-safe color** -- terminal color detection uses `AtomicBool` for safe concurrent access
- **Local-only HTTP** -- the HTTP server binds to 127.0.0.1 by default; not exposed to the network
- **WAL mode** -- SQLite Write-Ahead Logging for safe concurrent reads during writes

---

## Documentation

| Guide | Audience |
|-------|----------|
| [Installation Guide](docs/INSTALL.md) | Getting it running (includes MCP setup for multiple AI platforms) |
| [User Guide](docs/USER_GUIDE.md) | AI assistant users who want persistent memory |
| [Developer Guide](docs/DEVELOPER_GUIDE.md) | Building on or contributing to ai-memory |
| [Admin Guide](docs/ADMIN_GUIDE.md) | Deploying, monitoring, and troubleshooting |
| [GitHub Pages](https://alphaonedev.github.io/ai-memory-mcp/) | Visual overview with animated diagrams |

---

## License

Copyright (c) 2026 **AlphaOne LLC**. All rights reserved.

Licensed under the [MIT License](LICENSE).

THIS SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
