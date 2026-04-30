# Windsurf (Codeium) — MCP server + windsurfrules

**Category 2.** Windsurf is MCP-capable; configure in Settings → Cascade →
MCP Servers, or via `~/.codeium/windsurf/mcp_config.json`.

## Part 1 — MCP server

```json
{
  "mcpServers": {
    "ai-memory": {
      "command": "ai-memory",
      "args": ["mcp"],
      "env": { "AI_MEMORY_DB": "${HOME}/.claude/ai-memory.db" }
    }
  }
}
```

## Part 2 — `.windsurfrules` (best-effort)

At the project root:

```
At the start of every new conversation, call memory_session_start then
memory_recall against the project's namespace before responding. Reference
recalled titles in your first reply.
```

## Limitation + better

Category-2 limitation. Cross-file upstream tracked in #487.

## Related

- [`README.md`](README.md), Issue #487
