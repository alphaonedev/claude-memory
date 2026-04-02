# Installation Guide

> **BLUF (Bottom Line Up Front):** `ai-memory` is an AI-agnostic memory management system that works with **any MCP-compatible AI client** -- including Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others. Install the binary, configure your AI client's MCP settings, and you get 13 memory tools instantly. Total time: ~60 seconds.

## Install in 60 Seconds

1. **Install the binary:**
   ```bash
   cargo install --git https://github.com/alphaonedev/ai-memory-mcp.git
   ```

2. **Configure MCP in your AI client.** The example below is for **Claude Code** (`~/.claude/.mcp.json`):
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
   > **Other AI platforms** (OpenAI ChatGPT, xAI Grok, META Llama, etc.) have their own MCP configuration locations. Consult your platform's documentation for where to add MCP server entries. The server command and args are the same -- only the config file location differs.

3. **Restart your AI client.**

4. **Verify** -- you should see 13 new tools: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`, `memory_update`, `memory_get`, `memory_link`, `memory_get_links`, `memory_consolidate`.

5. **Test** -- ask your AI assistant to store a memory. It should use `memory_store` automatically.

That's it. Everything below is optional detail.

---

## Prerequisites

- **Rust toolchain** (1.75+): Install via [rustup](https://rustup.rs/)
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

## Install from Source (One-Liner)

```bash
cargo install --git https://github.com/alphaonedev/ai-memory-mcp.git
```

This builds a release binary and places it in `~/.cargo/bin/ai-memory`.

Or clone and build locally:

```bash
git clone https://github.com/alphaonedev/ai-memory-mcp.git
cd ai-memory
cargo install --path .
```

## Binary Download

Pre-built binaries are available on the [Releases](https://github.com/alphaonedev/ai-memory-mcp/releases) page for Linux (x86_64) and macOS (aarch64). Download the tarball for your platform:

```bash
tar xzf ai-memory-x86_64-unknown-linux-gnu.tar.gz
chmod +x ai-memory
sudo mv ai-memory /usr/local/bin/
```

## MCP Server Setup (Recommended)

The primary integration path is the **MCP tool server**. MCP (Model Context Protocol) is an open standard -- `ai-memory` works with **any MCP-compatible AI client**, including Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others.

### Step 1: Add MCP configuration

Each AI platform has its own MCP configuration location. The server command and arguments are identical across all platforms.

**Claude Code** -- create or edit `~/.claude/.mcp.json` (global -- applies to all projects):

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

> **Note for Claude Code:** MCP server configuration does **not** go in `settings.json` or `settings.local.json` -- those files do not support `mcpServers`.

**Other MCP-compatible clients** -- consult your platform's documentation for where to register MCP servers. The server entry is the same:
- **Command:** `ai-memory` (or full path if not in PATH)
- **Args:** `["--db", "/path/to/memory.db", "mcp"]`

If `ai-memory` is not in your PATH, use the full path to the binary:

```json
{
  "mcpServers": {
    "memory": {
      "command": "/usr/local/bin/ai-memory",
      "args": ["--db", "/var/lib/ai-memory/ai-memory.db", "mcp"]
    }
  }
}
```

### Step 2: Verify

Restart your AI client. You should see 13 new tools available: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`, `memory_update`, `memory_get`, `memory_link`, `memory_get_links`, `memory_consolidate`.

### Step 3: Test

Ask your AI assistant to store a memory. It should use the `memory_store` tool automatically.

## Hook Installation (Optional, Claude Code-Specific)

The `hooks/session-start.sh` script auto-recalls relevant memories at the start of each Claude Code session. Other AI platforms may have their own hook/plugin mechanisms -- the CLI commands used in this hook work with any platform.

### Install the hook

```bash
# Copy the hook
cp hooks/session-start.sh ~/.claude/hooks/

# Make it executable
chmod +x ~/.claude/hooks/session-start.sh
```

### Configure the hook in settings.json

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "command": "~/.claude/hooks/session-start.sh"
      }
    ]
  }
}
```

### Environment variables for the hook

| Variable | Default | Description |
|----------|---------|-------------|
| `AI_MEMORY_DB` | `ai-memory.db` | Path to the database |
| `AI_MEMORY_BIN` | `ai-memory` | Path to the binary |

## Systemd Service Setup (HTTP Daemon)

If you want to run the HTTP daemon as a background service (alternative to MCP). The HTTP API at `localhost:9077` works with **any AI platform, framework, or tool** -- no MCP required:

```bash
sudo tee /etc/systemd/system/ai-memory.service > /dev/null << 'EOF'
[Unit]
Description=Claude Memory Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/ai-memory --db /var/lib/ai-memory/ai-memory.db serve
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=ai_memory=info

# Graceful shutdown checkpoints the WAL
KillSignal=SIGINT
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
EOF
```

Create the data directory and enable the service:

```bash
sudo mkdir -p /var/lib/ai-memory
sudo systemctl daemon-reload
sudo systemctl enable --now ai-memory
```

## Verify Installation

```bash
# Check the binary
ai-memory --help

# If running as MCP server, test manually:
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | ai-memory mcp
# Expected: JSON-RPC response with serverInfo

# If running as HTTP daemon, check health:
curl http://127.0.0.1:9077/api/v1/health
# Expected: {"status":"ok","service":"ai-memory"}

# Store a test memory via CLI
ai-memory store -T "Installation test" -c "It works." --tier short

# Recall it
ai-memory recall "installation"
```

## Man Page

Generate and install the man page:

```bash
# View immediately
ai-memory man | man -l -

# Install system-wide
ai-memory man | sudo tee /usr/local/share/man/man1/ai-memory.1 > /dev/null
sudo mandb
man ai-memory
```

## Shell Completions

Generate completions for your shell:

```bash
# Bash
ai-memory completions bash > ~/.local/share/bash-completion/completions/ai-memory

# Zsh
ai-memory completions zsh > ~/.zfunc/_ai-memory

# Fish
ai-memory completions fish > ~/.config/fish/completions/ai-memory.fish
```

## Multi-Node Sync Setup

If you use ai-memory on multiple machines (e.g., laptop and server), you can sync databases:

```bash
# Pull memories from a remote database (e.g., over NFS, sshfs, or rsync'd copy)
ai-memory sync /mnt/server/ai-memory.db --direction pull

# Push local memories to remote
ai-memory sync /mnt/server/ai-memory.db --direction push

# Bidirectional merge (both sides get all memories, dedup-safe)
ai-memory sync /mnt/server/ai-memory.db --direction merge
```

The sync operation uses the same dedup-safe upsert as regular stores -- title+namespace conflicts are resolved by keeping the higher priority and never downgrading tier.

## Uninstall

```bash
# Stop and remove the service (if using systemd)
sudo systemctl stop ai-memory
sudo systemctl disable ai-memory
sudo rm /etc/systemd/system/ai-memory.service
sudo systemctl daemon-reload

# Remove MCP configuration from ~/.claude/.mcp.json

# Remove the binary
cargo uninstall ai-memory
# or: sudo rm /usr/local/bin/ai-memory

# Remove the database (WARNING: deletes all memories)
rm -f ai-memory.db ai-memory.db-wal ai-memory.db-shm
# or if using the systemd path:
# sudo rm -rf /var/lib/ai-memory
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `AI_MEMORY_DB` | `ai-memory.db` | Path to the SQLite database file |
| `RUST_LOG` | (none) | Log level filter (e.g., `ai_memory=info,tower_http=info`) |
