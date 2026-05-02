# ai-memory Quickstart — first memory in under 5 minutes

This guide gets you from zero to a working ai-memory install and your
first stored + recalled memory. Choose one of three paths depending
on how you want to use it.

## Install

```bash
# macOS / Linux (with Homebrew or prebuilt binary)
curl -sSL https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.sh | sh

# Or from cargo (any platform with Rust 1.88+)
cargo install --git https://github.com/alphaonedev/ai-memory-mcp ai-memory
```

Verify:

```bash
ai-memory --version
# ai-memory 0.6.3+patch.1   (release tag: v0.6.3.1; +patch.N is the crates.io-compatible encoding)
```

Full install reference including Windows, Docker, Fedora COPR, Ubuntu
PPA, and Homebrew tap: `docs/INSTALL.md`.

## Path A — CLI (fastest, 60 seconds)

```bash
# 1. Store your first memory
ai-memory store \
  --title "My first memory" \
  --content "ai-memory keeps this around for 7 days by default" \
  --tier mid

# 2. Recall it
ai-memory recall "what did I store"

# 3. See the stats
ai-memory stats
```

That's it. Memories live in `~/ai-memory.db` (override with `--db` or
`AI_MEMORY_DB`). Store anything, recall anything, no server running.

## Path B — Claude Code / Claude Desktop / Cursor / Codex (MCP)

ai-memory is an MCP server. Wire it into your AI IDE and every
conversation gets persistent memory across sessions.

**Claude Code** — add to `~/.claude/mcp_servers.json`:

```json
{
  "mcpServers": {
    "ai-memory": {
      "command": "ai-memory",
      "args": ["mcp", "--tier", "semantic"]
    }
  }
}
```

**Claude Desktop** — add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "ai-memory": { "command": "ai-memory", "args": ["mcp"] }
  }
}
```

**Cursor** — Settings → Features → Model Context Protocol → Add:

```
Command: ai-memory
Args: mcp --tier semantic
```

Restart the IDE. You'll now see 23 `memory_*` tools in the tool list.
Ask the assistant "remember that my preferred deploy target is
Kubernetes" and next session it'll recall it.

Full MCP setup for every IDE: `docs/INSTALL.md` § "MCP client setup".

## Path C — HTTP daemon (for applications + services)

```bash
# Start the daemon (plain HTTP, loopback only)
ai-memory serve --host 127.0.0.1 --port 9077 &

# Store via curl
curl -X POST http://127.0.0.1:9077/api/v1/memories \
  -H "Content-Type: application/json" \
  -d '{
    "title": "My first HTTP memory",
    "content": "Via the REST API",
    "tier": "mid"
  }'

# Recall via curl
curl -X POST http://127.0.0.1:9077/api/v1/recall \
  -H "Content-Type: application/json" \
  -d '{"context": "HTTP memory", "limit": 5}'

# Stop
kill %1
```

Use the TypeScript or Python SDK instead of hand-rolling HTTP:
`sdk/typescript/README.md` and `sdk/python/README.md`.

For production (TLS, API key, mTLS, systemd): `docs/ADMIN_GUIDE.md`.

## Verify everything works

```bash
# Counts by tier + namespace
ai-memory stats

# Full list
ai-memory list --limit 20

# Keyword search
ai-memory search "first"

# Semantic recall (needs the embedding model; first run downloads it)
ai-memory recall "memories I recently created"
```

First semantic recall on a fresh install downloads the
sentence-transformers/all-MiniLM-L6-v2 embedding model (~90 MB). This
is one-time; subsequent calls are instant.

## What to read next

- **Learning what each concept means** → `docs/GLOSSARY.md`
- **All CLI flags** → `docs/CLI_REFERENCE.md`
- **All HTTP endpoints** → `docs/API_REFERENCE.md`
- **MCP tool reference** → `docs/USER_GUIDE.md`
- **Running in production** → `docs/ADMIN_GUIDE.md`
- **Common errors** → `docs/TROUBLESHOOTING.md`
- **Contributing code** → `docs/DEVELOPER_GUIDE.md`
