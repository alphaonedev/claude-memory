# Admin Guide

`ai-memory` is an AI-agnostic memory management system. It works with **any MCP-compatible AI client** -- including Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others. The HTTP API and CLI are completely platform-independent.

## Deployment Options

### MCP Server (Recommended)

The simplest deployment is as an MCP tool server. No daemon process to manage -- your AI client spawns the process on demand. MCP (Model Context Protocol) is an open standard supported by multiple AI platforms.

Below is an example for **Claude Code** (`~/.claude/.mcp.json`). Other MCP-compatible clients have their own configuration locations -- consult your platform's documentation.

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

The daemon listens on `127.0.0.1:9077` by default and exposes 20 HTTP endpoints.

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
| `keyword` | 14 | No | No | Minimal |
| `semantic` (default) | 14 | Yes (HuggingFace) | No | ~256 MB |
| `smart` | 17 | Yes | Yes (Ollama) | ~1 GB |
| `autonomous` | 17 | Yes | Yes (Ollama) | ~4 GB |

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
| `RUST_LOG` | (none) | Logging filter (e.g., `ai_memory=info,tower_http=debug`) |

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

The schema is auto-migrated on startup. The `schema_version` table tracks the current version (currently 3). Migrations are forward-only and non-destructive.

- v1 -> v2: Added `confidence` (REAL) and `source` (TEXT) columns
- v2 -> v3: Added `embedding` (BLOB) column for storing dense vector embeddings

Migration error handling: only expected errors (e.g., "duplicate column" when re-running a migration) are silently ignored. Real failures are propagated and will prevent startup, ensuring data integrity.

### Database Maintenance

Manually trigger garbage collection:

```bash
# Via CLI
ai-memory gc

# Via API
curl -X POST http://127.0.0.1:9077/api/v1/gc
```

Compact the database (reduces file size after many deletions):

```bash
sqlite3 /path/to/ai-memory.db "VACUUM"
```

Rebuild the FTS index (if it becomes corrupt):

```bash
sqlite3 /path/to/ai-memory.db "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')"
```

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

### No Authentication

There is no authentication mechanism. This is by design -- the daemon is intended for localhost access only by your AI client (Claude AI, ChatGPT, Grok, Llama, or any other). If you expose it to a network, you are responsible for adding a reverse proxy with authentication.

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

The HTTP daemon exposes **20 endpoints** under `/api/v1`:

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
3. **Tests** -- `cargo test` (48 tests: 8 unit + 40 integration)
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
Check that the path in your MCP configuration (e.g., `~/.claude/.mcp.json` for Claude Code) is correct and the binary is executable.

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
