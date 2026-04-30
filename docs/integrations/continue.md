# Continue (VS Code / JetBrains) — MCP server + systemMessage

**Category 2.** Continue supports MCP via `~/.continue/config.json`.

## Quick install

```bash
ai-memory install continue              # dry-run (default)
ai-memory install continue --apply      # write ~/.continue/config.json
ai-memory install continue --uninstall --apply
```

Handles **Part 1** (MCP server registration). Part 2 (`systemMessage`)
is best-effort and still manual.

## Part 1 — MCP server

In `~/.continue/config.json`:

```json
{
  "experimental": {
    "modelContextProtocolServers": [
      {
        "transport": {
          "type": "stdio",
          "command": "ai-memory",
          "args": ["mcp"]
        }
      }
    ]
  }
}
```

## Part 2 — systemMessage (best-effort)

Add to the same config:

```json
{
  "systemMessage": "At the start of every conversation, before responding to the user's first message, call memory_session_start then memory_recall against the project's namespace and reference the recalled titles in your first reply. Default namespace: derived from the current workspace folder."
}
```

## Limitation + better

Same category-2 limitation as Cursor / Cline. Cross-file at Continue
upstream tracked in #487.

## Related

- [`README.md`](README.md), Issue #487
