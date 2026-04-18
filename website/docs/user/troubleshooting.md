---
sidebar_position: 7
title: Troubleshooting
description: Common issues and how to fix them.
---

# Troubleshooting

## "embedder model download failed"

First run at semantic tier or higher downloads MiniLM-L6-v2 (~100 MB) from HuggingFace. Retry, or fall back to `--tier keyword` temporarily:

```bash
ai-memory mcp --tier keyword
```

## MCP server not picking up

MCP servers are configured in **client config files**, not `settings.json`. For Claude Code, edit `~/.claude.json`:

```json
{ "mcpServers": { "memory": { "command": "ai-memory", "args": [...] } } }
```

Restart the AI client after editing.

## Smart / autonomous tier won't start

Smart and autonomous tiers require [Ollama](https://ollama.com) running locally:

```bash
ollama serve  # in one terminal
ollama pull gemma4:e2b
ai-memory mcp --tier smart
```

## Recall is slow

- First recall after fresh DB triggers HNSW backfill — one-time cost
- For >100K memories, consider `--tier semantic` over `--tier smart`/`autonomous` (LLM calls dominate latency)
- Check disk: WAL mode helps but slow disks bottleneck

## "namespace cannot be empty / cannot start with /"

Namespaces must be non-empty, no leading or trailing `/`, no double-slash, max depth 8. See [Namespaces](./namespaces).

## Backup before upgrade

```bash
cp ~/.local/share/ai-memory/memories.db ~/memories.db.backup
ai-memory --version  # verify upgrade
ai-memory list --limit 1  # triggers schema migration
```
