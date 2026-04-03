# Installation Guide

> **BLUF (Bottom Line Up Front):** `ai-memory` is an AI-agnostic memory management system that works with **any MCP-compatible AI client** -- including Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others. Install the binary, configure your AI client's MCP settings, and you get 17 memory tools instantly. The default `semantic` tier includes embedding-based hybrid recall out of the box. Total time: ~60 seconds.

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
         "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "semantic"]
       }
     }
   }
   ```
   > The `--tier` flag is optional (defaults to `semantic`). Options: `keyword`, `semantic`, `smart`, `autonomous`.
   > **Other AI platforms** (OpenAI ChatGPT, xAI Grok, META Llama, etc.) have their own MCP configuration locations. Consult your platform's documentation for where to add MCP server entries. The server command and args are the same -- only the config file location differs.

3. **Restart your AI client.**

4. **Verify** -- you should see 17 new tools: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`, `memory_update`, `memory_get`, `memory_link`, `memory_get_links`, `memory_consolidate`, `memory_capabilities`, `memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction`.

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
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

> **Note for Claude Code:** MCP server configuration does **not** go in `settings.json` or `settings.local.json` -- those files do not support `mcpServers`.

**OpenAI Codex CLI** -- create or edit `~/.codex/config.toml` (global) or `.codex/config.toml` (project):

```toml
[mcp_servers.memory]
command = "ai-memory"
args = ["--db", "~/.local/share/ai-memory/memories.db", "mcp"]
enabled = true
```

Or add via CLI:

```bash
codex mcp add memory -- ai-memory --db ~/.local/share/ai-memory/memories.db mcp
```

> **Notes for Codex CLI:** Codex uses TOML format with underscored key `mcp_servers`, not camelCase. Additional supported options include `env`, `cwd`, `startup_timeout_sec`, `tool_timeout_sec`, `enabled_tools` (restrict which memory tools are exposed), and `disabled_tools`. Use `/mcp` in the TUI to view server status. Codex also supports HTTP-based MCP servers via `url` and `bearer_token_env_var`. See [Codex MCP docs](https://developers.openai.com/codex/mcp).

**Google Gemini CLI** -- create or edit `~/.gemini/settings.json` (user) or `.gemini/settings.json` (project):

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

Or add via CLI:

```bash
gemini mcp add memory ai-memory -- --db ~/.local/share/ai-memory/memories.db mcp
```

> **Notes for Gemini CLI:** Avoid underscores in server names (use hyphens). Tool names are auto-prefixed as `mcp_<serverName>_<toolName>`. Gemini sanitizes environment variables -- explicitly declare needed vars in the `env` field (supports `$VAR` expansion). Add `"trust": true` to skip tool confirmation prompts. Additional supported options include `cwd`, `includeTools`, `excludeTools`, `url` (SSE), and `httpUrl` (HTTP). See [Gemini CLI MCP docs](https://geminicli.com/docs/tools/mcp-server/).

**Cursor IDE** -- create or edit `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (project-level):

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

Or add via Cursor Settings > Tools & MCP.

> **Notes for Cursor:** Restart Cursor (or reload window) after editing `mcp.json`. Verify server status in Settings > Tools & MCP (green dot = connected). Supports `env` field for environment variables, `envFile` for `.env` files, and `${env:VAR_NAME}` interpolation in config values. Also supports `url` + `headers` for remote HTTP/SSE servers. ~40 tool limit across all MCP servers combined. Project-level `.cursor/mcp.json` overrides global config for same-named servers. See [Cursor MCP docs](https://cursor.com/docs/context/mcp).

**Windsurf (Codeium)** -- create or edit `~/.codeium/windsurf/mcp_config.json`:

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

**Continue.dev** -- create or edit `~/.continue/config.yaml` (YAML format):

```yaml
mcpServers:
  - name: memory
    command: ai-memory
    args:
      - "--db"
      - "~/.local/share/ai-memory/memories.db"
      - "mcp"
```

> **Note for Continue.dev:** Uses YAML list format. MCP tools only work in agent mode.

**xAI Grok (API-level, remote MCP)** -- Grok connects to MCP servers over HTTPS (remote only, no stdio). Start ai-memory as an HTTP server behind HTTPS:

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

**Requirements:** HTTPS required. `server_label` is required. Supports Streamable HTTP and SSE transports. See [xAI Remote MCP docs](https://docs.x.ai/developers/tools/remote-mcp).

**META Llama (via Llama Stack)** -- Start the HTTP server, then register as a toolgroup:

```bash
ai-memory serve --host 127.0.0.1 --port 9077
```

```python
client.toolgroups.register(
    provider_id="model-context-protocol",
    toolgroup_id="mcp::memory",
    mcp_endpoint={"uri": "http://localhost:9077/sse"}
)
```

If `ai-memory` is not in your PATH, use the full path to the binary in any of the configurations above:

```json
{
  "mcpServers": {
    "memory": {
      "command": "/usr/local/bin/ai-memory",
      "args": ["--db", "/var/lib/ai-memory/ai-memory.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

### Step 2: Verify

Restart your AI client. You should see 17 new tools available: `memory_store`, `memory_recall`, `memory_search`, `memory_list`, `memory_delete`, `memory_promote`, `memory_forget`, `memory_stats`, `memory_update`, `memory_get`, `memory_link`, `memory_get_links`, `memory_consolidate`, `memory_capabilities`, `memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction`.

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
Description=AI Memory Daemon
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

## Ollama Installation (Smart & Autonomous Tiers)

Smart and autonomous tiers require [Ollama](https://ollama.com) running locally for LLM inference (Gemma 4 models). The `keyword` and `semantic` tiers do **not** require Ollama.

### macOS

```bash
# Install via Homebrew
brew install ollama

# Or download directly from https://ollama.com/download/mac
# Drag Ollama.app to Applications

# Start Ollama (runs as a background service)
ollama serve &

# Pull the model for your tier
ollama pull gemma4:e2b    # Smart tier (~1GB)
ollama pull gemma4:e4b    # Autonomous tier (~2.3GB)
```

### Linux

```bash
# One-line install script
curl -fsSL https://ollama.com/install.sh | sh

# Start the service
sudo systemctl enable ollama
sudo systemctl start ollama

# Or run manually
ollama serve &

# Pull the model for your tier
ollama pull gemma4:e2b    # Smart tier (~1GB)
ollama pull gemma4:e4b    # Autonomous tier (~2.3GB)
```

### Windows

```powershell
# Download installer from https://ollama.com/download/windows
# Run OllamaSetup.exe — installs and starts as a background service

# Or install via winget
winget install Ollama.Ollama

# Pull the model (in PowerShell or Command Prompt)
ollama pull gemma4:e2b    # Smart tier (~1GB)
ollama pull gemma4:e4b    # Autonomous tier (~2.3GB)
```

### Verify Ollama is Running

```bash
# Check Ollama status
curl http://localhost:11434/api/tags

# Test the model
ollama run gemma4:e2b "Hello, world"
```

### Configure ai-memory for Smart/Autonomous Tier

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "smart"]
    }
  }
}
```

> ai-memory connects to Ollama at `http://localhost:11434` automatically. No additional configuration needed. If Ollama is not running, ai-memory gracefully falls back to the semantic tier.

> **Note:** The `semantic` tier (default) downloads a HuggingFace embedding model (~100 MB) on first startup. No account or API key is required. The model is cached in `~/.cache/huggingface/`.

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
