# Roo Code (Cline fork) — MCP server + custom instructions

**Category 2 (MCP-capable, no native session-start hook).**

[Roo Code](https://github.com/RooCodeInc/Roo-Code) (formerly Roo Cline)
is a fork of Cline maintained by the Roo team. The recipe is
**largely identical to [`cline.md`](cline.md)** — same MCP-server
registration shape, same custom-instructions surface — with the
divergences noted below. If you have already installed the Cline
recipe, you can copy the same MCP-server JSON across; only the config
file path differs.

## Part 1 — MCP server

Roo Code's MCP config path differs from upstream Cline. Recent
versions store it under the VS Code extension storage:

- macOS:
  `~/Library/Application Support/Code/User/globalStorage/rooveterinaryinc.roo-cline/settings/mcp_settings.json`
  (the "rooveterinaryinc" publisher id is the historical Roo Cline id;
  newer builds may use a `roo-code` slug — check
  `~/Library/Application Support/Code/User/globalStorage/` for the
  active directory)
- Linux:
  `~/.config/Code/User/globalStorage/rooveterinaryinc.roo-cline/settings/mcp_settings.json`
- Windows:
  `%APPDATA%\Code\User\globalStorage\rooveterinaryinc.roo-cline\settings\mcp_settings.json`

If the canonical path varies on your install, the Roo Code Settings
panel (Command Palette → "Roo Code: Open MCP Settings") opens the
active file — preferred over hand-typing the path. The schema is
identical to Cline's:

```json
{
  "mcpServers": {
    "ai-memory": {
      "command": "ai-memory",
      "args": ["mcp"],
      "env": { "AI_MEMORY_DB": "${HOME}/.claude/ai-memory.db" },
      "disabled": false,
      "autoApprove": ["memory_session_start", "memory_recall", "memory_capabilities"]
    }
  }
}
```

`autoApprove` lets the model call read-only memory tools without
prompting for permission on every call — required for a smooth boot
path.

## Part 2 — Custom Instructions (best-effort)

Roo Code's custom-instructions surface is reachable via Settings →
Roo Code → Custom Instructions (same UI affordance as Cline). Roo Code
also supports per-mode instructions (the "Mode" abstraction is a Roo
addition not present in upstream Cline) — if you use modes, set the
directive on the default / "Code" mode for first-turn coverage.

> At the start of every conversation, before responding to the user's
> first message, call `memory_session_start` then `memory_recall`
> against the current project's namespace. Reference recalled titles
> in your first reply.

## Divergence from Cline

| Aspect | Cline | Roo Code |
|---|---|---|
| Extension publisher id | `saoudrizwan.claude-dev` | `rooveterinaryinc.roo-cline` (historical) / `roo-code` (newer) |
| MCP settings file | `~/.cline/mcp_settings.json` (varies by version) | VS Code extension storage path (see above) |
| Modes / personas | Single agent | Per-mode customization (Code / Architect / Ask / Debug) |
| Custom Instructions surface | Single textbox | Per-mode + global textboxes |

Otherwise the shapes are identical. If a future Roo Code release
diverges further (e.g., changes the MCP settings schema), this recipe
will pick up the delta and re-document.

## Quick install

Manual install only until PR-2's installer follow-up adds explicit
Roo Code support. The MCP-settings JSON edit above is the manual form;
track the installer issue for one-line bootstrap that handles the
publisher-id variance and per-mode instructions.

## End-user diagnostic

Roo Code surfaces MCP tool calls in its chat transcript, so when the
directive fires you will see `memory_session_start` in the tool-call
log on the first turn. If you don't, either MCP is not loading the
server (check the Roo Code output panel in VS Code: View → Output →
"Roo Code") or the directive is not being applied to the active mode.
The `ai-memory boot` status-header diagnostic (see
[`README.md`](README.md)) does not apply here because Roo Code's
integration is MCP-only.

## Limitations

- Category-2: text-directive in custom instructions is subject to
  model compliance.
- Roo Code's per-mode instructions are powerful but easy to
  misconfigure — verify the directive is in the active mode, not just
  the global textbox.
- The publisher id and storage path have changed at least once across
  Roo Code's history (Roo Cline → Roo Code rename). The recipe pins
  the historical id; on a fresh install confirm the active directory
  before pasting.

## Better, when Roo Code lands a session-start hook

We have an open feature request at the Roo Code repo to add a
documented session-start hook (cross-filed from issue #487). When that
ships, replace Part 2 with a hook entry pointing at:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

This recipe will be updated in place once the hook lands.

## Related

- [`README.md`](README.md) — integration matrix and the universal
  primitive.
- [`cline.md`](cline.md) — the upstream parent recipe; most of the
  shape is shared.
- Issue #487 — RCA + cross-files for category-2 native-hook requests.
