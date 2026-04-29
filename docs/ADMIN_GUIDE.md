# Admin Guide

`ai-memory` is an AI-agnostic memory management system. It works with **any MCP-compatible AI client** -- including Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others. The HTTP API and CLI are completely platform-independent.

**Key features for admins:** Zero token cost until recall (replaces built-in auto-memory), TOON compact default response format (79% smaller than JSON), MCP prompts for proactive AI behavior (`recall-first`, `memory-workflow`), 4 feature tiers (keyword → autonomous with local LLMs via Ollama), 191 tests with 95%+ coverage across 15/15 modules.

## Deployment Options

### MCP Server (Recommended)

The simplest deployment is as an MCP tool server. No daemon process to manage -- your AI client spawns the process on demand. MCP (Model Context Protocol) is an open standard supported by multiple AI platforms.

Below is an example for **Claude Code** (user scope: merge `mcpServers` into `~/.claude.json`; or project scope: `.mcp.json` in project root). Other MCP-compatible clients have their own configuration locations — consult your platform's documentation.

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

The MCP server:
- Starts when your AI client opens a session
- Communicates over stdio (JSON-RPC) -- the standard MCP transport
- Stops when the session ends
- Uses the same SQLite database as the CLI and HTTP daemon
- Correctly skips all JSON-RPC notifications (no response sent)
- Works with any MCP-compatible client, not just Claude Code

### Standalone (Development)

Run the HTTP daemon directly in the foreground:

```bash
ai-memory --db /path/to/ai-memory.db serve
```

The daemon listens on `127.0.0.1:9077` by default and exposes 24 HTTP endpoints.

### Systemd (Production HTTP Daemon)

```bash
sudo tee /etc/systemd/system/ai-memory.service > /dev/null << 'EOF'
[Unit]
Description=AI Memory Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/ai-memory --db /var/lib/ai-memory/ai-memory.db serve
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=ai_memory=info,tower_http=info

# Graceful shutdown: checkpoints WAL before exit
KillSignal=SIGINT
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
EOF

sudo mkdir -p /var/lib/ai-memory
sudo systemctl daemon-reload
sudo systemctl enable --now ai-memory
```

**Production Hardening:** Add security directives to the `[Service]` section to restrict the daemon's privileges:

```ini
[Service]
User=ai-memory
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
NoNewPrivileges=yes
ReadWritePaths=/var/lib/ai-memory
```

Check status:

```bash
sudo systemctl status ai-memory
sudo journalctl -u ai-memory -f
```

### Docker

Example Dockerfile:

```dockerfile
FROM rust:1.75-slim AS builder
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /src/target/release/ai-memory /usr/local/bin/
VOLUME /data
EXPOSE 9077
CMD ["ai-memory", "--db", "/data/ai-memory.db", "serve"]
```

Build and run:

```bash
docker build -t ai-memory .
docker run -d -p 127.0.0.1:9077:9077 -v ai-memory-data:/data ai-memory
```

## Configuration

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--db <path>` | `ai-memory.db` | Path to SQLite database |
| `--host <addr>` | `127.0.0.1` | Bind address (serve only) |
| `--port <port>` | `9077` | Bind port (serve only) |
| `--json` | `false` | JSON output for CLI commands |
| `--tier <tier>` | `semantic` | Feature tier: `keyword`, `semantic`, `smart`, `autonomous` (mcp/serve only) |

### Feature Tiers

The `--tier` flag controls which features are enabled. Each tier builds on the previous one:

| Tier | Tools | Embedding Model | LLM Required | Approx. Memory |
|------|-------|----------------|--------------|----------------|
| `keyword` | 21 | No | No | Minimal |
| `semantic` (default) | 21 | Yes (HuggingFace) | No | ~256 MB |
| `smart` | 21 | Yes | Yes (Ollama) | ~1 GB |
| `autonomous` | 21 | Yes | Yes (Ollama) | ~4 GB |

Set the tier when starting the MCP server or HTTP daemon:

```bash
ai-memory mcp --tier semantic        # default
ai-memory mcp --tier smart           # enables LLM-powered tools
ai-memory serve --tier autonomous    # full feature set
```

### Ollama Setup (Smart & Autonomous Tiers)

The `smart` and `autonomous` tiers require a running [Ollama](https://ollama.com) instance for LLM inference (Gemma 4 models).

#### macOS
```bash
brew install ollama
# Or download from https://ollama.com/download/mac
ollama serve &
ollama pull gemma4:e2b    # Smart tier (~1GB)
ollama pull gemma4:e4b    # Autonomous tier (~2.3GB)
```

#### Linux
```bash
curl -fsSL https://ollama.com/install.sh | sh
sudo systemctl enable ollama
sudo systemctl start ollama
ollama pull gemma4:e2b    # Smart tier (~1GB)
ollama pull gemma4:e4b    # Autonomous tier (~2.3GB)
```

#### Windows
```powershell
# Download from https://ollama.com/download/windows, or:
winget install Ollama.Ollama
ollama pull gemma4:e2b    # Smart tier (~1GB)
ollama pull gemma4:e4b    # Autonomous tier (~2.3GB)
```

#### Verify
```bash
curl http://localhost:11434/api/tags
ollama run gemma4:e2b "Hello, world"
```

ai-memory connects to Ollama at `http://localhost:11434` by default. Set `OLLAMA_HOST` to override. If Ollama is not running, ai-memory gracefully falls back to the semantic tier.

### Embedding Model (semantic tier and above)

At the `semantic` tier and above, ai-memory downloads a sentence-transformer model from HuggingFace on first startup. The model is cached in the HuggingFace cache directory (`~/.cache/huggingface/` by default).

- **First startup** may take 30-60 seconds while the model downloads (~100 MB)
- **Subsequent startups** load from cache (2-5 seconds)
- Set `HF_HOME` to override the cache directory
- No HuggingFace account or API key is required

### Memory Budget Guidance

| Tier | RAM Requirement | Notes |
|------|----------------|-------|
| `keyword` | Minimal (~10 MB) | SQLite + FTS5 only |
| `semantic` | ~256 MB | Embedding model loaded in memory |
| `smart` | ~1 GB | Embedding model + Ollama with smaller LLM |
| `autonomous` | ~4 GB | Embedding model + Ollama with larger LLM |

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `AI_MEMORY_DB` | `ai-memory.db` | Database path (overridden by `--db`) |
| `AI_MEMORY_AGENT_ID` | (auto) | Default `agent_id` stamped on memories this process writes. Used when no `--agent-id` flag is passed. See §Agent Identity below. |
| `RUST_LOG` | (none) | Logging filter (e.g., `ai_memory=info,tower_http=debug`) |
| `AI_MEMORY_NO_CONFIG` | (none) | Set to `1` to skip config file loading (useful for testing) |

### Configuration File (config.toml)

`ai-memory` supports an optional configuration file at `~/.config/ai-memory/config.toml`. This file is read once at process startup and supports the following keys:

> **Note:** Configuration is loaded once at process startup. Changes to `config.toml` require restarting the ai-memory process (MCP server, HTTP daemon, or CLI) to take effect.

| Key | Type | Default | Valid Values | Description |
|-----|------|---------|--------------|-------------|
| `tier` | String | `"semantic"` | `"keyword"`, `"semantic"`, `"smart"`, `"autonomous"` | Feature tier controlling which AI capabilities are active |
| `db` | String | `"ai-memory.db"` | Any valid file path | Path to the SQLite database file |
| `ollama_url` | String | `"http://localhost:11434"` | Any URL | Ollama base URL for LLM generation (smart/autonomous tiers) |
| `embed_url` | String | Value of `ollama_url` | Any URL | Separate URL for the embedding service; falls back to `ollama_url` if unset |
| `embedding_model` | String | Tier-dependent | `"mini_lm_l6_v2"` (384-dim, ~90 MB), `"nomic_embed_v15"` (768-dim, ~270 MB) | HuggingFace sentence-transformer model for semantic search |
| `llm_model` | String | Tier-dependent | `"gemma4:e2b"` (~1 GB Q4), `"gemma4:e4b"` (~2.3 GB Q4) | Ollama LLM model tag for smart/autonomous features |
| `cross_encoder` | **Bool** | `false` (`true` for autonomous tier) | `true`, `false` | Enable neural cross-encoder reranking (not a string -- must be bare `true`/`false` without quotes) |
| `default_namespace` | String | `"global"` | Any valid namespace (max 128 bytes, no slashes/spaces/nulls) | Default namespace applied to new memories |
| `max_memory_mb` | Integer | Tier-dependent | Any positive integer | Maximum memory budget in MB; used for automatic tier selection via `from_memory_budget()` |
| `archive_on_gc` | Bool | `true` | `true`, `false` | Archive expired memories instead of permanently deleting them during GC |
| `[ttl]` | Section | -- | -- | Per-tier TTL overrides (all sub-fields are integers in seconds) |
| `ttl.short_ttl_secs` | Integer | `21600` (6 hours) | `0` = never expires, or positive integer | TTL for short-tier memories in seconds |
| `ttl.mid_ttl_secs` | Integer | `604800` (7 days) | `0` = never expires, or positive integer | TTL for mid-tier memories in seconds |
| `ttl.long_ttl_secs` | Integer | `0` (never expires) | `0` = never expires, or positive integer | TTL for long-tier memories in seconds |
| `ttl.short_extend_secs` | Integer | `3600` (1 hour) | Non-negative integer | TTL extension on access for short-tier memories |
| `ttl.mid_extend_secs` | Integer | `86400` (1 day) | Non-negative integer | TTL extension on access for mid-tier memories |

> **Note:** Set any TTL to `0` to disable expiry for that tier. Values are clamped to a 10-year maximum (315,360,000 seconds). Negative extension values are clamped to 0.

> **Note:** Restored memories have their `expires_at` cleared (set to NULL) and become permanent.

#### Complete Annotated config.toml

Below is a complete example showing every supported field with explanatory comments. Copy this to `~/.config/ai-memory/config.toml` and uncomment the lines you want to customize.

```toml
# =============================================================================
# ai-memory configuration
# Location: ~/.config/ai-memory/config.toml
# Docs: https://github.com/alphaonedev/ai-memory-mcp
#
# All fields are optional. CLI flags and MCP args override these values.
# Changes require restarting the ai-memory process to take effect.
# =============================================================================

# ---------------------------------------------------------------------------
# Feature tier (controls which AI capabilities are active)
# ---------------------------------------------------------------------------
# Valid values: "keyword", "semantic", "smart", "autonomous"
#   keyword    — FTS5 keyword search only, no models, minimal RAM
#   semantic   — adds embedding-based hybrid recall (~256 MB)
#   smart      — adds query expansion, auto-tagging, contradiction detection (~1 GB, requires Ollama)
#   autonomous — full feature set with cross-encoder reranking (~4 GB, requires Ollama)
# Default: "semantic"
# tier = "semantic"

# ---------------------------------------------------------------------------
# Database path
# ---------------------------------------------------------------------------
# Path to the SQLite database file.
# Default: "ai-memory.db" (relative to working directory)
# db = "~/.claude/ai-memory.db"

# ---------------------------------------------------------------------------
# Ollama URLs (smart and autonomous tiers only)
# ---------------------------------------------------------------------------
# Base URL for Ollama LLM generation.
# Default: "http://localhost:11434"
# ollama_url = "http://localhost:11434"

# Separate URL for embedding requests. Falls back to ollama_url if unset.
# Default: same as ollama_url
# embed_url = "http://localhost:11434"

# ---------------------------------------------------------------------------
# Model selection
# ---------------------------------------------------------------------------
# Embedding model for semantic search (semantic tier and above).
# Valid values:
#   "mini_lm_l6_v2"   — sentence-transformers/all-MiniLM-L6-v2, 384-dim, ~90 MB
#   "nomic_embed_v15"  — nomic-ai/nomic-embed-text-v1.5, 768-dim, ~270 MB
# Default: tier-dependent (mini_lm_l6_v2 for semantic, nomic_embed_v15 for smart/autonomous)
# embedding_model = "mini_lm_l6_v2"

# LLM model served via Ollama (smart and autonomous tiers).
# Valid values:
#   "gemma4:e2b"  — Google Gemma 4 Effective 2B, ~1 GB Q4 (smart tier default)
#   "gemma4:e4b"  — Google Gemma 4 Effective 4B, ~2.3 GB Q4 (autonomous tier default)
# Default: tier-dependent (gemma4:e2b for smart, gemma4:e4b for autonomous)
# llm_model = "gemma4:e2b"

# ---------------------------------------------------------------------------
# Cross-encoder reranking
# ---------------------------------------------------------------------------
# Enable neural cross-encoder reranking for improved recall precision.
# NOTE: This is a boolean, NOT a string. Use bare true/false without quotes.
# Default: false (true for autonomous tier)
# cross_encoder = true

# ---------------------------------------------------------------------------
# Namespace and memory limits
# ---------------------------------------------------------------------------
# Default namespace applied to new memories when none is specified.
# Default: "global"
# default_namespace = "global"

# Maximum memory budget in MB. Used for automatic tier selection when tier
# is not explicitly set — the highest tier that fits within this budget is chosen.
# Default: tier-dependent (0/256/1024/4096 for keyword/semantic/smart/autonomous)
# max_memory_mb = 4096

# ---------------------------------------------------------------------------
# Garbage collection
# ---------------------------------------------------------------------------
# Archive expired memories before GC permanently deletes them.
# When true, expired memories are moved to the archive table and can be
# restored later. When false, GC permanently deletes expired memories.
# Default: true
# archive_on_gc = true

# ---------------------------------------------------------------------------
# Per-tier TTL overrides
# ---------------------------------------------------------------------------
# Customize time-to-live and access-extension durations per memory tier.
# Set any TTL to 0 to disable expiry for that tier.
# Values are clamped to a 10-year maximum (315,360,000 seconds).
# Negative extension values are clamped to 0.
# [ttl]
# short_ttl_secs = 21600        # 6 hours (default)
# mid_ttl_secs = 604800         # 7 days (default)
# long_ttl_secs = 0             # 0 = never expires (default)
# short_extend_secs = 3600      # +1 hour on access (default)
# mid_extend_secs = 86400       # +1 day on access (default)
```

**Precedence:** CLI flags and MCP args take precedence over `config.toml` values. When the MCP server is launched by an AI client, the `--tier` flag in the MCP args is used, not the `config.toml` `tier` setting.

### Compile-Time Constants

These are set in the source code and require recompilation to change:

| Constant | Value | Location |
|----------|-------|----------|
| `DEFAULT_PORT` | 9077 | `main.rs` |
| `GC_INTERVAL_SECS` | 1800 (30 min) | `main.rs` |
| `MAX_CONTENT_SIZE` | 65536 (64 KB) | `models.rs` |
| `PROMOTION_THRESHOLD` | 5 accesses | `models.rs` |
| `SHORT_TTL_EXTEND_SECS` | 3600 (1 hour) | `models.rs` |
| `MID_TTL_EXTEND_SECS` | 86400 (1 day) | `models.rs` |

## Graceful Shutdown

The HTTP daemon handles SIGINT (Ctrl+C) gracefully:

1. Stops accepting new connections
2. Waits for in-flight requests to complete
3. Checkpoints the WAL (`PRAGMA wal_checkpoint(TRUNCATE)`)
4. Exits cleanly

For systemd, use `KillSignal=SIGINT` and `TimeoutStopSec=10` to ensure the checkpoint completes.

> **Note:** The HTTP daemon handles SIGINT (Ctrl+C) gracefully with WAL checkpoint. Systemd sends SIGTERM by default -- the service file sets `KillSignal=SIGINT` to ensure clean shutdown.

The MCP server exits cleanly when stdin closes (AI client session ends).

## Database Management

### SQLite Settings

The database uses these pragmas (set automatically on open):

- **WAL mode** -- write-ahead logging for concurrent reads
- **busy_timeout = 5000** -- 5 second wait on lock contention
- **synchronous = NORMAL** -- balanced durability/performance
- **foreign_keys = ON** -- enforced referential integrity (links cascade on delete)

### Backup

**Live backup (while daemon is running):**

```bash
sqlite3 /path/to/ai-memory.db ".backup /path/to/backup.db"
```

**JSON export (includes links):**

```bash
ai-memory --db /path/to/ai-memory.db export > backup.json
```

**File copy (daemon must be stopped or use WAL checkpoint first):**

```bash
systemctl stop ai-memory
cp /path/to/ai-memory.db /path/to/backup.db
cp /path/to/ai-memory.db-wal /path/to/backup.db-wal 2>/dev/null
systemctl start ai-memory
```

### Restore

**From JSON (preserves links):**

```bash
ai-memory --db /path/to/new.db import < backup.json
```

**From SQLite backup:**

```bash
systemctl stop ai-memory
cp /path/to/backup.db /var/lib/ai-memory/ai-memory.db
systemctl start ai-memory
```

### Migration

The schema is auto-migrated on startup. The `schema_version` table tracks the current version (currently 4). Migrations are forward-only and non-destructive.

- v1 -> v2: Added `confidence` (REAL) and `source` (TEXT) columns
- v2 -> v3: Added `embedding` (BLOB) column for storing dense vector embeddings
- v3 -> v4: Added `archived_memories` table for GC archival

Migration error handling: only expected errors (e.g., "duplicate column" when re-running a migration) are silently ignored. Real failures are propagated and will prevent startup, ensuring data integrity.

### Upgrade Procedure

1. Stop the service: `sudo systemctl stop ai-memory`
2. Backup the database: `sqlite3 /var/lib/ai-memory/ai-memory.db ".backup /var/lib/ai-memory/ai-memory-backup.db"`
3. Install the new binary (e.g., `cargo install ai-memory` or replace the binary at `/usr/local/bin/ai-memory`)
4. Start the service: `sudo systemctl start ai-memory`

Schema migrations run automatically on startup. No manual migration steps are required.

### Database Maintenance

Manually trigger garbage collection:

```bash
# Via CLI
ai-memory gc

# Via API
curl -X POST http://127.0.0.1:9077/api/v1/gc
```

By default, GC archives expired memories before deleting them. To disable archiving and permanently delete instead, set `archive_on_gc = false` in `config.toml`. Archived memories are moved to a separate archive table and can be listed, restored, or purged:

```bash
# List archived memories
curl http://127.0.0.1:9077/api/v1/archive

# Restore an archived memory
curl -X POST http://127.0.0.1:9077/api/v1/archive/<id>/restore

# Purge all archived memories permanently (optional: ?older_than_days=N)
curl -X DELETE http://127.0.0.1:9077/api/v1/archive

# View archive statistics
curl http://127.0.0.1:9077/api/v1/archive/stats
```

**Disk space guidance:** Approximate database growth: ~2KB per memory (keyword tier), ~3.5KB per memory (semantic tier, 384-dim embeddings), ~5KB per memory (768-dim embeddings). WAL file may grow up to ~50MB during heavy write bursts; checkpoint occurs on graceful shutdown. Archive table grows unboundedly -- use `ai-memory archive purge` periodically.

Compact the database (reduces file size after many deletions):

```bash
sqlite3 /path/to/ai-memory.db "VACUUM"
```

Rebuild the FTS index (if it becomes corrupt):

```bash
sqlite3 /path/to/ai-memory.db "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')"
```

## Agent Identity (NHI)

Introduced in v0.6.0 via Task 1.2. Every memory carries `metadata.agent_id`, a
best-effort Non-Human Identity marker for the agent that stored it. Design
context and the threat model are tracked on issue [#148](https://github.com/alphaonedev/ai-memory-mcp/issues/148).

### Trust model

**`metadata.agent_id` is a *claimed* identity, not an *attested* one.** Any
caller able to invoke the CLI / MCP / HTTP API can set any well-formed
`agent_id`. Use it for provenance, audit, and filter scoping — **never as an
authorization gate on its own.** True attestation arrives with agent
registration (Task 1.3).

### Resolution precedence

**CLI and MCP (process-scoped):**

1. Explicit caller value (`--agent-id`, MCP `agent_id` tool param, or
   `metadata.agent_id` embedded in an MCP store request)
2. `AI_MEMORY_AGENT_ID` environment variable
3. (MCP only) `initialize.clientInfo.name` → `ai:<client>@<hostname>:pid-<pid>`
4. `host:<hostname>:pid-<pid>-<uuid8>` (stable for the process's lifetime)
5. `anonymous:pid-<pid>-<uuid8>` (only when hostname is unavailable)

**HTTP daemon (request-scoped, no process-level default):**

1. `agent_id` field in `POST /api/v1/memories` body
2. `X-Agent-Id` request header
3. `anonymous:req-<uuid8>` (synthesized per-request, logged at WARN)

### Validation

Server-side validator:
`^[A-Za-z0-9_\-:@./]{1,128}$`

This admits prefixed forms (`ai:`, `host:`, `anonymous:`, `human:`, `system:`),
the `@` scope separator, `/` for future SPIFFE ids, and dots. Rejects whitespace,
null bytes, ASCII control chars, and shell metacharacters. Payloads attempting
SQL injection, JSON-path break-outs, or path traversal are all either validator-
rejected or neutralized by the sanitizer (Unicode homoglyphs rejected outright).

### Immutability guarantees

Once a memory is stored, `metadata.agent_id` is preserved across every mutation:

| Path | Preservation mechanism |
|---|---|
| `db::insert` UPSERT (dedup) | SQL `CASE WHEN json_extract(...) IS NOT NULL THEN json_set(...) ELSE excluded.metadata END` |
| `db::insert_if_newer` (sync merge) | Same SQL CASE WHEN clause |
| `db::update` with caller-supplied metadata | Caller preserves via `identity::preserve_agent_id` (every caller does — MCP `handle_store` dedup, MCP `handle_update`, HTTP `update_memory`) |
| `db::consolidate` | Takes `consolidator_agent_id` parameter; original authors preserved in `metadata.consolidated_from_agents` |

Admins running audit queries can rely on `metadata.agent_id` never changing
post-write unless the memory is deleted and recreated.

### Special metadata keys produced by the system

These are written by the server; treat as read-only in queries:

| Key | Written when | Shape |
|---|---|---|
| `agent_id` | Every write | String matching validator regex |
| `imported_from_agent_id` | `ai-memory import` without `--trust-source`, when the incoming JSON's `agent_id` differed from the caller's | String |
| `consolidated_from_agents` | `memory_consolidate` / `auto-consolidate` merges N sources | Array of deduplicated strings |
| `mined_from` | `ai-memory mine` (Claude / ChatGPT / Slack export import) | String: `"claude"`, `"chatgpt"`, `"slack"` |
| `derived_from` | `memory_consolidate` — array of source memory ids | Array of UUID strings |

### Filtering by `agent_id`

`list` and `search` accept an `agent_id` filter (exact match via SQLite
`json_extract`):

- CLI: `ai-memory list --agent-id alice`, `ai-memory search "x" --agent-id alice`
- MCP: `agent_id` property on the `memory_list` / `memory_search` tool inputs
- HTTP: `GET /api/v1/memories?agent_id=alice`, `GET /api/v1/search?q=x&agent_id=alice`

`recall` does **not** accept the filter (by spec).

### Operational warnings

- **Default identities leak infrastructure.** When no explicit `agent_id` is
  set, memories are stamped `host:<hostname>:pid-<pid>-<uuid8>`, exposing the
  host's name and the running PID. For multi-tenant databases or any scenario
  where the DB is shared outside its origin host, require callers to set
  `AI_MEMORY_AGENT_ID` or `--agent-id` explicitly. See [#198] for tracked work
  on a config-level opt-out.
- **HTTP per-request anonymous fallback** emits a WARN log line
  (`HTTP memory write without agent_id body field or X-Agent-Id header;
  assigned anonymous:req-<uuid8>`). Grep for this in production logs to spot
  unauthenticated writes.
- **Import provenance** is restamped to the current caller by default. If you
  need to restore legacy `agent_id` values verbatim (e.g., migrating a backup),
  pass `--trust-source` explicitly.

### Related tracked issues

- [#148](https://github.com/alphaonedev/ai-memory-mcp/issues/148) — Task 1.2 design & NHI assessment
- [#196](https://github.com/alphaonedev/ai-memory-mcp/issues/196) — Store responses don't echo resolved agent_id
- [#197](https://github.com/alphaonedev/ai-memory-mcp/issues/197) — Filter values should run through validator
- [#198](https://github.com/alphaonedev/ai-memory-mcp/issues/198) — Config-level opt-out for hostname/PID leak

## Security Hardening

### Transaction Safety

Critical operations use `BEGIN IMMEDIATE` / `COMMIT` transactions to prevent data corruption under concurrent access:
- **`touch()`** -- the read-modify-write cycle for access count, TTL extension, auto-promotion, and priority reinforcement is fully atomic
- **`consolidate()`** -- the multi-step merge (create new memory, delete originals, aggregate tags) is fully atomic

This prevents race conditions where two concurrent recalls could cause incorrect access counts or missed auto-promotions.

### FTS Query Injection Protection

All full-text search queries are sanitized before being passed to SQLite FTS5:
- Special characters (`*`, `"`, `(`, `)`, `:`, `+`, `-`, `^`, etc.) are stripped
- Remaining tokens are individually double-quoted (e.g., `auth flow` becomes `"auth" "flow"`)
- This prevents FTS query syntax injection that could cause errors or unexpected results

The sanitization is applied in `recall()`, `search()`, and `forget()` operations.

### Error Sanitization

The HTTP API never leaks internal database error details to clients. All `rusqlite::Error` and `anyhow::Error` responses are replaced with a generic `"Internal server error"` message. Detailed errors are logged server-side for debugging.

### Bulk Input Limits

To prevent memory exhaustion and abuse:
- **Bulk create** (`POST /memories/bulk`): Limited to 1,000 items per request
- **Import** (`POST /import`): Limited to 1,000 memories per request

Requests exceeding these limits receive a `400 Bad Request` response.

### Path Parameter Validation

All ID path parameters (e.g., `/memories/{id}`, `/links/{id}`) are validated before database queries are executed. Invalid IDs (empty, too long, containing null bytes) are rejected with a `400 Bad Request` response before any database access occurs.

### Input Validation

All write paths go through the validation layer (`validate.rs`):
- Title: max 512 bytes, no null bytes
- Content: max 64KB, no null bytes
- Namespace: max 128 bytes, no slashes/spaces/nulls
- Source: whitelist (user, claude, hook, api, cli, import, consolidation, system)
- Tags: max 50 tags, each max 128 bytes
- Priority: 1-10
- Confidence: 0.0-1.0, finite
- Relations: whitelist (related_to, supersedes, contradicts, derived_from)
- IDs: max 128 bytes, no null bytes
- Timestamps: valid RFC3339
- TTL: positive, max 1 year

### Localhost Binding

By default, the HTTP daemon binds to `127.0.0.1` only. It is **not accessible from the network**. This is intentional -- `ai-memory` is a local-machine tool.

The MCP server communicates over stdio only -- no network exposure.

### CORS

The HTTP server uses `CorsLayer::new()` (deny-by-default) since v0.5.4-patch.6. Cross-origin requests are rejected unless explicitly configured. For production, use a reverse proxy with restrictive CORS headers if you need to allow specific origins.

### Authentication

There is no API-key or token authentication mechanism for the standard MCP / HTTP / CLI surface. This is by design — the daemon is intended for localhost access only by your AI client.

For the **peer-to-peer sync mesh** (v0.6.0+), authentication is provided by mTLS fingerprint pinning — see "Peer-mesh security" above. Sync endpoints WITHOUT mTLS are unauthenticated and MUST NOT be exposed to untrusted networks.

### Multi-User Warning

ai-memory is a single-user tool. Namespaces do not provide access control. If multiple users share a database, any user can read/write any namespace.

### TLS / HTTPS (v0.6.0+)

**ai-memory now supports native TLS** via `--tls-cert <pem>` + `--tls-key <pem>` on `serve`. rustls under the hood — no OpenSSL dep, no reverse proxy required:

```bash
ai-memory serve --tls-cert server.pem --tls-key server.key
```

Reverse proxy termination still works if you prefer it (nginx / Caddy / Traefik). For most deployments, the native TLS path removes a moving part.

### Peer-mesh security (v0.6.0+) — MUST READ before deploying sync

The peer-to-peer sync mesh introduces new trust assumptions. Disclosed gaps and required mitigations:

#### Sync endpoints are unauthenticated without TLS (issue #231)

`POST /api/v1/sync/push` and `GET /api/v1/sync/since` accept connections from any caller when `serve` runs without `--tls-cert + --tls-key`. The handler accepts `sender_agent_id` from the request body without cryptographic proof.

**Production deployments MUST set `--tls-cert + --tls-key + --mtls-allowlist`** for the peer mesh. Without all three, any network-positioned attacker can push spoofed memories or pull the entire database.

#### sync-daemon does no server-cert verification without --client-cert (issue #232)

When `sync-daemon` is invoked without `--client-cert`, the underlying reqwest client uses `danger_accept_invalid_certs(true)` — it accepts ANY server cert, no validation against system trust roots, no peer-cert pinning.

**For untrusted networks, ALWAYS use mTLS in both directions.** Set `--client-cert` + `--client-key` on the daemon and `--mtls-allowlist` on the peer's `serve`.

#### Any valid mTLS peer can dump the full database (issue #239)

`GET /api/v1/sync/since?since=<old-ts>` paginates the entire database. By design — the trust boundary IS the mTLS cert — but the implication is that **a compromised peer cert grants access to every memory**, including `scope: private` memories from other agents' namespaces. Sync endpoints bypass the per-memory visibility filtering used by `/recall`.

**Allowlist only peers you fully trust.** Per-namespace / per-scope sync filtering is a Phase 5 feature (post-v0.6.0).

#### Body-claimed sender_agent_id is not yet attested (issue #238)

mTLS gates network access but the receiving handler accepts `sender_agent_id` from the body without checking it matches the cert's CN/SAN. A peer with a valid cert can claim any agent_id. Tracked as Layer 2b for v0.7.

### mTLS setup recipe

1. Generate cert pairs (or reuse existing X.509 keypairs):

```bash
openssl req -x509 -newkey rsa:2048 -keyout server.key -out server.pem \
  -days 365 -nodes -subj "/CN=peer-a.local"
openssl req -x509 -newkey rsa:2048 -keyout client.key -out client.pem \
  -days 365 -nodes -subj "/CN=peer-a.client"
```

2. Compute and exchange SHA-256 fingerprints:

```bash
openssl x509 -in client.pem -outform DER | sha256sum
```

3. Build the allowlist file (one fingerprint per line; `sha256:` prefix and `:` separators are optional). Full-line `#` comments and inline trailing `# label` annotations after a fingerprint are both tolerated:

```
# peer A's client cert
sha256:25ab790783dbe969f994063db0412f1930e187e5e1e6c7d79bb76224a76b7bb7  # node-1
```

4. Run with all three flags:

```bash
ai-memory serve --tls-cert server.pem --tls-key server.key \
  --mtls-allowlist ./peers.allow

ai-memory sync-daemon --peers https://peer-b:9077 \
  --client-cert client.pem --client-key client.key
```

A peer without an allowlisted cert is rejected at the **TLS handshake** — well before any HTTP request reaches the application.

### Data at Rest

The SQLite database is stored as a regular file. It is not encrypted. If you need encryption at rest, use filesystem-level encryption (LUKS, FileVault, BitLocker).

### MCP Notification Handling

The MCP server correctly handles all JSON-RPC notifications (requests without an `id` field). Notifications are processed but no response is sent, per the JSON-RPC 2.0 specification. This prevents protocol errors when any MCP client sends `notifications/initialized` or other notification messages.

### WAL Files

SQLite WAL mode creates two additional files alongside the database:
- `ai-memory.db-wal` -- write-ahead log
- `ai-memory.db-shm` -- shared memory file

Both are cleaned up on graceful shutdown (the daemon runs `PRAGMA wal_checkpoint(TRUNCATE)` on SIGINT). If the daemon crashes, these files persist but are automatically recovered on next open.

## HTTP API Endpoints

Maximum request body size: 50 MB.

The HTTP daemon exposes **24 endpoints** under `/api/v1`:

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Deep health check (DB + FTS integrity) |
| `POST` | `/memories` | Create a memory |
| `POST` | `/memories/bulk` | Bulk create (max 1,000) |
| `GET` | `/memories/{id}` | Get a memory by ID (includes links) |
| `PUT` | `/memories/{id}` | Update a memory |
| `DELETE` | `/memories/{id}` | Delete a memory |
| `POST` | `/memories/{id}/promote` | Promote a memory to long-term |
| `GET` | `/memories` | List memories with filters |
| `GET` | `/search` | AND search with 6-factor scoring |
| `GET` | `/recall` | OR recall with touch + auto-promote |
| `POST` | `/recall` | OR recall (POST body) |
| `POST` | `/forget` | Bulk delete by pattern/namespace/tier |
| `POST` | `/consolidate` | Consolidate 2-100 memories |
| `POST` | `/links` | Create a link between memories |
| `GET` | `/links/{id}` | Get links for a memory |
| `GET` | `/namespaces` | List namespaces with counts |
| `GET` | `/stats` | Aggregate statistics |
| `POST` | `/gc` | Trigger garbage collection |
| `GET` | `/export` | Export all memories and links |
| `POST` | `/import` | Import memories and links (max 1,000) |
| `GET` | `/archive` | List archived memories |
| `POST` | `/archive/{id}/restore` | Restore an archived memory |
| `DELETE` | `/archive` | Permanently delete archived memories (optional `?older_than_days=N`) |
| `GET` | `/archive/stats` | Archive statistics |

### HTTP API Request/Response Examples

Below are curl examples showing the exact JSON request bodies and response formats for the most important endpoints. The base URL is `http://127.0.0.1:9077/api/v1`.

#### POST /memories (Store)

Create a new memory. Only `title` and `content` are required; all other fields have defaults.

```bash
curl -X POST http://127.0.0.1:9077/api/v1/memories \
  -H "Content-Type: application/json" \
  -d '{
    "title": "Project uses PostgreSQL 16",
    "content": "The production database runs PostgreSQL 16 with pgvector for embeddings.",
    "tier": "long",
    "namespace": "infra",
    "tags": ["postgres", "database"],
    "priority": 9,
    "confidence": 1.0,
    "source": "user",
    "ttl_secs": 604800
  }'
```

**Required fields:**
| Field | Type | Description |
|-------|------|-------------|
| `title` | string | Memory title (max 512 bytes) |
| `content` | string | Memory content (max 64 KB) |

**Optional fields:**
| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `tier` | string | `"mid"` | `"short"`, `"mid"`, or `"long"` |
| `namespace` | string | `"global"` | Namespace for grouping (max 128 bytes, no slashes/spaces) |
| `tags` | array | `[]` | String tags (max 50 tags, each max 128 bytes) |
| `priority` | integer | `5` | 1-10 (clamped) |
| `confidence` | float | `1.0` | 0.0-1.0 (clamped) |
| `source` | string | `"api"` | One of: `user`, `claude`, `hook`, `api`, `cli`, `import`, `consolidation`, `system` |
| `expires_at` | string | (none) | Explicit expiry timestamp (RFC3339) |
| `ttl_secs` | integer | (none) | TTL in seconds (overrides tier default) |

**Response (201 Created):**

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "tier": "long",
  "namespace": "infra",
  "title": "Project uses PostgreSQL 16"
}
```

If potential contradictions are found (memories with similar titles in the same namespace), the response includes:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "tier": "long",
  "namespace": "infra",
  "title": "Project uses PostgreSQL 16",
  "potential_contradictions": ["existing-id-1", "existing-id-2"]
}
```

Deduplication: if a memory with the same title+namespace already exists, it is upserted (tier never downgrades, priority keeps the maximum).

**Minimal example (defaults applied):**

```bash
curl -X POST http://127.0.0.1:9077/api/v1/memories \
  -H "Content-Type: application/json" \
  -d '{"title": "Quick note", "content": "Something to remember."}'
```

Response: `{"id": "...", "tier": "mid", "namespace": "global", "title": "Quick note"}`

#### GET /memories/{id} (Get)

Retrieve a single memory by ID, including its links to other memories.

```bash
curl http://127.0.0.1:9077/api/v1/memories/a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

**Response (200 OK):**

```json
{
  "memory": {
    "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "tier": "long",
    "namespace": "infra",
    "title": "Project uses PostgreSQL 16",
    "content": "The production database runs PostgreSQL 16 with pgvector for embeddings.",
    "tags": ["postgres", "database"],
    "priority": 9,
    "confidence": 1.0,
    "source": "user",
    "access_count": 3,
    "created_at": "2026-04-03T15:00:00+00:00",
    "updated_at": "2026-04-03T15:00:00+00:00",
    "last_accessed_at": "2026-04-10T09:30:00+00:00",
    "expires_at": null
  },
  "links": [
    {
      "source_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "target_id": "f7e8d9c0-b1a2-3456-7890-abcdef123456",
      "relation": "related_to",
      "created_at": "2026-04-05T12:00:00+00:00"
    }
  ]
}
```

**Response (404 Not Found):** `{"error": "not found"}`

Note: `last_accessed_at` and `expires_at` are omitted from the JSON when null.

#### GET /recall?context=... (Recall)

Fuzzy OR search with ranked results. Automatically bumps access count, extends TTL, and auto-promotes frequently accessed mid-tier memories to long-term.

```bash
curl "http://127.0.0.1:9077/api/v1/recall?context=database+migration+postgres&namespace=infra&limit=5"
```

**Query parameters:**
| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `context` | string | (required) | Search context / query text |
| `namespace` | string | (none) | Filter by namespace |
| `limit` | integer | `10` | Max results (capped at 50) |
| `tags` | string | (none) | Comma-separated tag filter |
| `since` | string | (none) | Only memories updated after this RFC3339 timestamp |
| `until` | string | (none) | Only memories updated before this RFC3339 timestamp |

**Response (200 OK):**

```json
{
  "memories": [
    {
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "tier": "long",
      "namespace": "infra",
      "title": "Project uses PostgreSQL 16",
      "content": "The production database runs PostgreSQL 16 with pgvector for embeddings.",
      "tags": ["postgres", "database"],
      "priority": 9,
      "confidence": 1.0,
      "source": "user",
      "access_count": 4,
      "created_at": "2026-04-03T15:00:00+00:00",
      "updated_at": "2026-04-03T15:00:00+00:00",
      "last_accessed_at": "2026-04-12T10:00:00+00:00",
      "score": 0.763
    }
  ],
  "count": 1
}
```

Each memory in the response includes a `score` field (float, rounded to 3 decimal places) representing the composite relevance score. Memories are returned sorted by score descending.

Recall is also available via POST for larger query bodies:

```bash
curl -X POST http://127.0.0.1:9077/api/v1/recall \
  -H "Content-Type: application/json" \
  -d '{
    "context": "database migration postgres",
    "namespace": "infra",
    "limit": 5,
    "tags": "postgres",
    "since": "2026-01-01T00:00:00Z"
  }'
```

#### PUT /memories/{id} (Update)

Partial update -- only provided fields are modified. All fields are optional.

```bash
curl -X PUT http://127.0.0.1:9077/api/v1/memories/a1b2c3d4-e5f6-7890-abcd-ef1234567890 \
  -H "Content-Type: application/json" \
  -d '{
    "content": "PostgreSQL 16.2 with pgvector 0.7 for embeddings. Upgraded 2026-04-10.",
    "priority": 10,
    "tags": ["postgres", "database", "pgvector"]
  }'
```

**Updatable fields:**
| Field | Type | Description |
|-------|------|-------------|
| `title` | string | New title |
| `content` | string | New content |
| `tier` | string | New tier (`"short"`, `"mid"`, `"long"`) |
| `namespace` | string | New namespace |
| `tags` | array | Replace tags entirely |
| `priority` | integer | New priority (1-10) |
| `confidence` | float | New confidence (0.0-1.0) |
| `expires_at` | string | New expiry (RFC3339) |

**Response (200 OK):** Returns the full updated memory object:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "tier": "long",
  "namespace": "infra",
  "title": "Project uses PostgreSQL 16",
  "content": "PostgreSQL 16.2 with pgvector 0.7 for embeddings. Upgraded 2026-04-10.",
  "tags": ["postgres", "database", "pgvector"],
  "priority": 10,
  "confidence": 1.0,
  "source": "user",
  "access_count": 4,
  "created_at": "2026-04-03T15:00:00+00:00",
  "updated_at": "2026-04-12T10:05:00+00:00"
}
```

**Response (404 Not Found):** `{"error": "not found"}`

**Response (409 Conflict):** `{"error": "title already exists in namespace ..."}` (if updating the title to one that already exists in the same namespace)

#### GET /archive (List Archived)

List memories that were archived by garbage collection.

```bash
curl "http://127.0.0.1:9077/api/v1/archive?namespace=infra&limit=20&offset=0"
```

**Query parameters:**
| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `namespace` | string | (none) | Filter by namespace |
| `limit` | integer | `50` | Max results (capped at 1000) |
| `offset` | integer | `0` | Pagination offset |

**Response (200 OK):**

```json
{
  "archived": [
    {
      "id": "expired-memory-id",
      "tier": "short",
      "namespace": "infra",
      "title": "Temp debug session",
      "content": "Debugging connection pooling issue...",
      "tags": ["debug"],
      "priority": 3,
      "confidence": 1.0,
      "source": "claude",
      "access_count": 1,
      "created_at": "2026-04-01T10:00:00+00:00",
      "updated_at": "2026-04-01T10:00:00+00:00",
      "expires_at": "2026-04-01T16:00:00+00:00",
      "archived_at": "2026-04-02T00:30:00+00:00",
      "archive_reason": "gc"
    }
  ],
  "count": 1
}
```

#### POST /archive/{id}/restore (Restore)

Restore an archived memory back to the active memories table. The restored memory has its `expires_at` cleared (becomes permanent).

```bash
curl -X POST http://127.0.0.1:9077/api/v1/archive/expired-memory-id/restore
```

**Response (200 OK):**

```json
{
  "restored": true,
  "id": "expired-memory-id"
}
```

**Response (404 Not Found):** `{"error": "not found in archive"}`

## Monitoring

### Health Endpoint (Deep Check)

```bash
curl http://127.0.0.1:9077/api/v1/health
```

The health check performs a **deep verification**:
1. Database is readable (runs `SELECT COUNT(*) FROM memories`)
2. FTS5 index integrity check (`INSERT INTO memories_fts(memories_fts) VALUES('integrity-check')`)

Returns `200 OK` with `{"status": "ok", "service": "ai-memory"}` if healthy.
Returns `503 Service Unavailable` with `{"status": "error", "service": "ai-memory"}` if the database or FTS index is unhealthy.

### Stats Endpoint

```bash
curl http://127.0.0.1:9077/api/v1/stats
```

Returns:
- Total memory count
- Breakdown by tier
- Breakdown by namespace
- Memories expiring within 1 hour
- Total link count
- Database file size in bytes

### MCP Server Monitoring

The MCP server logs to stderr. Monitor via:

```bash
# If running via an AI client, check your client's MCP logs
# If running manually:
ai-memory mcp 2>mcp-server.log
```

Key log messages:
- `ai-memory MCP server started (stdio)` -- server is ready
- `ai-memory MCP server stopped` -- stdin closed (AI client session ended), server exiting

### Logs

The HTTP daemon logs via `tracing` with configurable levels:

```bash
# Info level (default recommended)
RUST_LOG=ai_memory=info,tower_http=info ai-memory serve

# Debug level (verbose, includes all HTTP requests)
RUST_LOG=ai_memory=debug,tower_http=debug ai-memory serve

# Trace level (extremely verbose)
RUST_LOG=ai_memory=trace ai-memory serve
```

With systemd, logs go to the journal:

```bash
sudo journalctl -u ai-memory -f
sudo journalctl -u ai-memory --since "1 hour ago"
```

### Monitoring Script Example

```bash
#!/bin/bash
HEALTH=$(curl -sf http://127.0.0.1:9077/api/v1/health | jq -r '.status')
if [ "$HEALTH" != "ok" ]; then
    echo "ai-memory health check failed"
    systemctl restart ai-memory
fi
```

## CI/CD Pipeline

The project uses GitHub Actions for continuous integration and release automation.

### CI (Every Push and PR)

Runs on `ubuntu-latest` and `macos-latest`:

1. **Formatting** -- `cargo fmt --check`
2. **Linting** -- `cargo clippy -- -D warnings`
3. **Tests** -- `cargo test` (191 tests: 140 unit + 51 integration, 15/15 modules)
4. **Build** -- `cargo build --release`

Uses `Swatinem/rust-cache@v2` for build caching.

### Release (On Tag Push)

Triggered by tags matching `v*` (e.g., `v0.1.0`):

1. Builds release binaries for:
   - `x86_64-unknown-linux-gnu` (Ubuntu)
   - `aarch64-apple-darwin` (macOS ARM)
2. Packages each as `ai-memory-<target>.tar.gz`
3. Creates a GitHub Release with the artifacts

### Running CI Locally

```bash
# Replicate the CI checks
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --release
```

## Multi-Node Sync

For multi-machine deployments (e.g., laptop + server, or multiple workstations), use the `sync` command to keep databases in sync.

### Manual Sync

```bash
# Pull remote changes to local
ai-memory sync /mnt/shared/ai-memory.db --direction pull

# Push local changes to remote
ai-memory sync /mnt/shared/ai-memory.db --direction push

# Bidirectional merge (recommended)
ai-memory sync /mnt/shared/ai-memory.db --direction merge
```

### Automated Sync via Cron

```bash
# Sync every 15 minutes (bidirectional merge)
*/15 * * * * /usr/local/bin/ai-memory --db /var/lib/ai-memory/ai-memory.db sync /mnt/shared/remote-memory.db --direction merge --json >> /var/log/ai-memory-sync.log 2>&1
```

Sync uses the same dedup-safe upsert as regular stores:
- Title+namespace conflicts are resolved by keeping the higher priority
- Tier never downgrades
- Links are synced alongside memories
- Safe to run concurrently from multiple machines (SQLite WAL mode handles locking)

### Sync via sshfs or rsync

If the remote database is on another machine, mount it or copy it first:

```bash
# Option 1: sshfs mount
mkdir -p /mnt/remote-memory
sshfs user@server:/var/lib/ai-memory /mnt/remote-memory
ai-memory sync /mnt/remote-memory/ai-memory.db --direction merge

# Option 2: rsync + sync + rsync
rsync -a server:/var/lib/ai-memory/ai-memory.db /tmp/remote.db
ai-memory sync /tmp/remote.db --direction merge
rsync -a /tmp/remote.db server:/var/lib/ai-memory/ai-memory.db
```

## Auto-Consolidation (Maintenance)

Auto-consolidation groups memories by namespace and primary tag, then merges groups with enough members into a single long-term summary. This reduces memory count and improves recall relevance.

### Manual Run

```bash
# Preview what would be consolidated
ai-memory auto-consolidate --dry-run

# Consolidate all namespaces (groups of 3+)
ai-memory auto-consolidate

# Only short-term memories, minimum 5 per group
ai-memory auto-consolidate --short-only --min-count 5
```

### Cron Schedule

```bash
# Run auto-consolidation daily at 3am, short-term memories only
0 3 * * * /usr/local/bin/ai-memory --db /var/lib/ai-memory/ai-memory.db auto-consolidate --short-only --json >> /var/log/ai-memory-consolidate.log 2>&1
```

## Man Page

Install the man page for system-wide documentation:

```bash
ai-memory man | sudo tee /usr/local/share/man/man1/ai-memory.1 > /dev/null
sudo mandb
man ai-memory
```

## Scaling Considerations

`ai-memory` is designed for single-machine use. It is not a distributed system.

- **Concurrency**: The daemon uses `Arc<Mutex<Connection>>` -- one write at a time, but this is fine for a single-user tool. SQLite WAL mode allows concurrent reads.
- **MCP concurrency**: The MCP server is single-threaded (synchronous stdio loop), one request at a time. This is by design -- MCP clients typically send one request at a time.
- **Database size**: SQLite handles databases up to 281 TB. Practically, performance stays excellent up to millions of rows.
- **Memory usage**: Minimal. The daemon holds only the connection and a path in memory. All data is on disk.
- **Multiple instances**: You can run multiple daemons on different ports with different databases. Do not point two daemons at the same database file. The MCP server and CLI can share a database (both use WAL mode).

## Troubleshooting

### Daemon won't start

**Port already in use:**
```bash
ss -tlnp | grep 9077
# Kill the existing process or use a different port
ai-memory serve --port 9078
```

**Database locked:**
```bash
# Remove stale WAL files (only if daemon is not running)
rm -f ai-memory.db-wal ai-memory.db-shm
```

**Permission denied:**
```bash
# Check file permissions
ls -la /path/to/ai-memory.db
# Ensure the user running the daemon has read/write access
```

### MCP server not connecting

**Binary not found:**
Check that the path in your MCP configuration (e.g., `~/.claude.json` for Claude Code user scope, or `.mcp.json` for project scope) is correct and the binary is executable.

**Database path issues:**
The MCP server opens the database at the path specified by `--db`. Ensure the directory exists and is writable.

**Protocol errors:**
Check stderr output. The MCP server logs parse errors and protocol issues to stderr.

### Slow queries

If recall or search is slow:

```bash
# Rebuild the FTS index
sqlite3 /path/to/ai-memory.db "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')"

# Compact the database
sqlite3 /path/to/ai-memory.db "VACUUM"
```

### FTS index corruption

Symptoms: search returns no results or errors.

```bash
# Check integrity
sqlite3 /path/to/ai-memory.db "INSERT INTO memories_fts(memories_fts) VALUES('integrity-check')"

# Rebuild if corrupt
sqlite3 /path/to/ai-memory.db "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')"
```

### Database is growing too large

```bash
# Check what's taking space
ai-memory stats

# Delete expired memories
ai-memory gc

# Delete all short-term memories in a namespace
ai-memory forget --tier short --namespace my-app

# Compact after deletion
sqlite3 /path/to/ai-memory.db "VACUUM"
```
