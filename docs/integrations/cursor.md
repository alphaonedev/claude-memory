# Cursor — MCP server + `.cursorrules` directive

**Category 2 (MCP-capable, no native session-start hook).**

Cursor supports MCP servers via Settings → Features → Model Context
Protocol, and supports project-scoped behavior via `.cursorrules` at the
repo root. There is no documented SessionStart hook today, so the recipe
is two-part.

## Quick install

```bash
ai-memory install cursor              # dry-run (default)
ai-memory install cursor --apply      # write ~/.cursor/mcp.json
ai-memory install cursor --uninstall --apply
```

This handles **Part 1** (MCP-server registration) automatically. Part 2
(`.cursorrules` directive) is project-scoped and still manual — see below.

## Part 1 — register the MCP server

Cursor's MCP config (Settings → Features → MCP, or `~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "ai-memory": {
      "command": "ai-memory",
      "args": ["mcp"],
      "env": {
        "AI_MEMORY_DB": "${HOME}/.claude/ai-memory.db"
      }
    }
  }
}
```

Restart Cursor.

## Part 2 — `.cursorrules` directive (best-effort)

At the project root, in `.cursorrules`:

```
At the start of every new conversation, before responding to the user's first
message, call the `memory_session_start` MCP tool from the `ai-memory` server
and a `memory_recall` against the current project's namespace. Reference the
recalled titles in your first reply.

Default namespace for this project: <set from cwd basename or project name>
```

Or, in workspace `.cursorrules`, you can specify the namespace explicitly:

```
Default ai-memory namespace: ai-memory-mcp/v0631-release
```

Caveat: text-directive only. Issue #487 layer 6.

## Better, when Cursor lands a session-start hook

Replace Part 2 with the hook command:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

Cross-file at the Cursor repo (linked from #487 comments).

## Related

- [`README.md`](README.md) — matrix
- Issue #487 — RCA
