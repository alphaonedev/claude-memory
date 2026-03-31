# Installation Guide

## Prerequisites

- **Rust toolchain** (1.75+): Install via [rustup](https://rustup.rs/)
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

## Install from Source (One-Liner)

```bash
cargo install --git https://github.com/alphaonedev/claude-memory.git
```

This builds a release binary and places it in `~/.cargo/bin/claude-memory`.

Or clone and build locally:

```bash
git clone https://github.com/alphaonedev/claude-memory.git
cd claude-memory
cargo install --path .
```

## Binary Download

Pre-built binaries are available on the [Releases](https://github.com/alphaonedev/claude-memory/releases) page for Linux (x86_64) and macOS (aarch64). Download the tarball for your platform:

```bash
tar xzf claude-memory-x86_64-unknown-linux-gnu.tar.gz
chmod +x claude-memory
sudo mv claude-memory /usr/local/bin/
```

## MCP Server Setup (Recommended)

The primary integration path is the **MCP tool server**. This makes memory operations available as native tools inside Claude Code.

### Step 1: Add to Claude Code settings

Edit your Claude Code `settings.json` and add:

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

If `claude-memory` is not in your PATH, use the full path to the binary:

```json
{
  "mcpServers": {
    "memory": {
      "command": "/usr/local/bin/claude-memory",
      "args": ["--db", "/var/lib/claude-memory/claude-memory.db", "mcp"]
    }
  }
}
```

### Step 2: Verify

Restart Claude Code. You should see 8 new tools available: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`.

### Step 3: Test

Ask Claude to store a memory. It should use the `memory_store` tool automatically.

## Hook Installation (Optional)

The `hooks/session-start.sh` script auto-recalls relevant memories at the start of each Claude Code session.

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
| `CLAUDE_MEMORY_DB` | `claude-memory.db` | Path to the database |
| `CLAUDE_MEMORY_BIN` | `claude-memory` | Path to the binary |

## Systemd Service Setup (HTTP Daemon)

If you want to run the HTTP daemon as a background service (alternative to MCP):

```bash
sudo tee /etc/systemd/system/claude-memory.service > /dev/null << 'EOF'
[Unit]
Description=Claude Memory Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/claude-memory --db /var/lib/claude-memory/claude-memory.db serve
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=claude_memory=info

# Graceful shutdown checkpoints the WAL
KillSignal=SIGINT
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
EOF
```

Create the data directory and enable the service:

```bash
sudo mkdir -p /var/lib/claude-memory
sudo systemctl daemon-reload
sudo systemctl enable --now claude-memory
```

## Verify Installation

```bash
# Check the binary
claude-memory --help

# If running as MCP server, test manually:
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | claude-memory mcp
# Expected: JSON-RPC response with serverInfo

# If running as HTTP daemon, check health:
curl http://127.0.0.1:9077/api/v1/health
# Expected: {"status":"ok","service":"claude-memory"}

# Store a test memory via CLI
claude-memory store -T "Installation test" -c "It works." --tier short

# Recall it
claude-memory recall "installation"
```

## Shell Completions

Generate completions for your shell:

```bash
# Bash
claude-memory completions bash > ~/.local/share/bash-completion/completions/claude-memory

# Zsh
claude-memory completions zsh > ~/.zfunc/_claude-memory

# Fish
claude-memory completions fish > ~/.config/fish/completions/claude-memory.fish
```

## Uninstall

```bash
# Stop and remove the service (if using systemd)
sudo systemctl stop claude-memory
sudo systemctl disable claude-memory
sudo rm /etc/systemd/system/claude-memory.service
sudo systemctl daemon-reload

# Remove MCP configuration from Claude Code settings.json

# Remove the binary
cargo uninstall claude-memory
# or: sudo rm /usr/local/bin/claude-memory

# Remove the database (WARNING: deletes all memories)
rm -f claude-memory.db claude-memory.db-wal claude-memory.db-shm
# or if using the systemd path:
# sudo rm -rf /var/lib/claude-memory
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CLAUDE_MEMORY_DB` | `claude-memory.db` | Path to the SQLite database file |
| `RUST_LOG` | (none) | Log level filter (e.g., `claude_memory=info,tower_http=info`) |
