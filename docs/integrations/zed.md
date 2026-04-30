# Zed assistant — MCP server + assistant directive

**Category 2 (MCP-capable, no native session-start hook).**
Reference: <https://zed.dev/docs/assistant>

[Zed](https://zed.dev/) is a high-performance editor with a built-in
AI assistant panel. Recent Zed builds have MCP support; configuration
lives in Zed's `settings.json`. There is no documented session-start
hook today, so the recipe is two-part: register `ai-memory-mcp` as an
MCP server **plus** add an assistant-rules directive telling the model
to call `memory_session_start` before responding to the first user
turn.

## Part 1 — register `ai-memory-mcp` as an MCP context server

Zed's settings live at `~/.config/zed/settings.json` (Linux/macOS) or
`%APPDATA%\Zed\settings.json` (Windows). The MCP key name has shifted
across Zed versions — recent builds use `context_servers`, earlier
experimental builds used variants under `assistant`. The canonical
path varies; if your build does not pick up the entry under
`context_servers`, consult `Cmd+,` (Settings) → search for "mcp" or
"context server" to confirm the key name.

Add to `~/.config/zed/settings.json`:

```json
{
  "context_servers": {
    "ai-memory": {
      "command": {
        "path": "ai-memory",
        "args": ["mcp"],
        "env": {
          "AI_MEMORY_DB": "${HOME}/.claude/ai-memory.db"
        }
      }
    }
  }
}
```

Notes:

- `path: "ai-memory"` requires the binary to be on Zed's `$PATH` at
  launch. macOS GUI launches inherit a minimal PATH, so prefer the
  absolute path (`/opt/homebrew/bin/ai-memory`) here.
- Restart Zed after editing settings so the context server is picked
  up.
- If you launch Zed from a terminal (`zed .`) the inherited PATH is
  richer and the bare command form usually works.

## Part 2 — assistant directive (best-effort fallback)

Zed's assistant panel supports a per-workspace or global default
prompt / system instruction surface (Settings → Assistant → Default
Model / Default Prompt). Add the following directive there:

> Before responding to any user message in a fresh conversation, call
> the `ai-memory.memory_session_start` MCP tool and a `memory_recall`
> against the relevant project namespace (default: the current working
> directory's basename). Surface the recalled titles in your first
> reply so the user can confirm continuity.

Caveat: text directives are best-effort (issue #487 layer 6) — the
model may skip the call under load or competing instructions.

## Quick install

Manual install only until PR-2's installer follow-up adds explicit
Zed support. The settings.json edit above is the manual form; track
the installer issue for one-line bootstrap that handles the platform
path differences (`~/.config/zed/` vs `%APPDATA%\Zed\`).

## End-user diagnostic

Zed surfaces MCP tool calls in the assistant panel transcript, so
when the directive fires you will see `memory_session_start` in the
tool-call log on the first turn. If you don't, either MCP is not
loading the server (check the Zed log: `zed --foreground` on
Linux/macOS, or the bottom panel on launch) or the directive is not
being applied. The `ai-memory boot` status-header diagnostic (see
[`README.md`](README.md)) does not apply here because Zed's
integration is MCP-only, not boot-shellout-based.

## Limitations

- Category-2: text-directive in assistant settings is subject to
  model compliance.
- The `context_servers` key is current at time of writing but Zed's
  MCP surface is evolving rapidly — verify with the Zed docs before
  filing bugs against the recipe.
- Zed's assistant supports multiple models (Anthropic, OpenAI,
  others); the directive applies regardless of underlying model, but
  tool-call format varies and the user-visible transcript will look
  different across providers.
- Zed launched from Spotlight / Launchpad inherits a minimal PATH;
  prefer the absolute `path` form to avoid "command not found" on
  macOS GUI starts.

## Better, when Zed lands a session-start hook

We have an open feature request at the Zed repo to add a documented
session-start hook for the assistant panel (cross-filed from issue
#487). When that ships, replace Part 2 with a hook entry pointing at:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

This recipe will be updated in place once the hook lands.

## Related

- [`README.md`](README.md) — integration matrix and the universal
  primitive.
- [`cursor.md`](cursor.md), [`cline.md`](cline.md) — same category-2
  pattern for editor-hosted MCP clients.
- Issue #487 — RCA + cross-files for category-2 native-hook requests.
