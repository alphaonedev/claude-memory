---
sidebar_position: 6
title: CLI reference
description: All 26 CLI commands.
---

# CLI reference

```bash
ai-memory --help
```

## Server / daemon

| Command | Purpose |
|---|---|
| `serve` | HTTP daemon (HTTPS optional via `--tls-cert`) |
| `mcp` | MCP server over stdio |
| `sync-daemon` | Peer-to-peer sync mesh |
| `sync <remote-db>` | Two-DB sync (file-to-file) with `--dry-run` option |

## CRUD

| Command | Purpose |
|---|---|
| `store` | Store / upsert |
| `get <id>` | Retrieve (accepts 8-char short ID) |
| `list` | List with filter |
| `update <id>` | Update fields |
| `delete <id>` | Delete |
| `promote <id>` | Promote to long (or to ancestor namespace via `--to-namespace`) |
| `forget` | Bulk delete |

## Search + recall

| Command | Purpose |
|---|---|
| `recall <context>` | Hybrid recall (`--budget-tokens`, `--as-agent`) |
| `search <query>` | Exact-keyword search |

## Relationships

| Command | Purpose |
|---|---|
| `link <src> <dst>` | Create memory link |
| `consolidate <ids>` | Merge memories |
| `resolve <winner> <loser>` | Mark contradiction resolution |

## Namespaces + agents

| Command | Purpose |
|---|---|
| `namespaces` | List namespaces (CLI: list-only; MCP for set-standard) |
| `agents register --agent-id <id> --agent-type <type>` | Register an agent |
| `agents list` | List registered agents |
| `pending list \| approve <id> \| reject <id>` | Governance queue |

## Bulk + I/O

| Command | Purpose |
|---|---|
| `export` | Export all as JSONL |
| `import` | Import from JSONL (stdin) |
| `mine` | Import from Claude/ChatGPT/Slack exports |
| `gc` | Run garbage collection |
| `auto-consolidate` | Auto-merge short-term memories |
| `archive list \| restore <id> \| purge \| stats` | Archive ops |
| `stats` | Memory statistics |
| `shell` | Interactive REPL |
| `completions <shell>` | Generate shell completions |
| `man` | Generate man page |

## Global flags

| Flag | Purpose |
|---|---|
| `--db <path>` | Database path (also via `AI_MEMORY_DB`) |
| `--json` | Machine-parseable JSON output |
| `--agent-id <id>` | Override agent identity (also via `AI_MEMORY_AGENT_ID`) |

## Source

`src/main.rs` — clap-based command tree.
