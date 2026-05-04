# OpenClaw — MCP server + system-message directive

**Category 2 (MCP-capable, no native session-start hook).**
Reference: <https://docs.openclaw.ai/cli/mcp#mcp-client-config>

OpenClaw supports MCP servers via a JSON config block but does not document
a session-start hook today. Until it does, the recipe is two-part: register
`ai-memory-mcp` as an MCP server (so the model can call memory tools at any
time), and add a one-line system-message directive instructing the model to
call `memory_session_start` before responding to the first user turn.

## Quick install

OpenClaw's canonical config path is documented at
<https://docs.openclaw.ai/cli/mcp> but isn't auto-discoverable yet, so the
installer requires `--config <path>` until upstream pins a stable location:

```bash
# TODO(#487): once OpenClaw publishes a stable canonical path,
# --config will be optional.
ai-memory install openclaw --config <your-openclaw-config.json>
ai-memory install openclaw --config <your-openclaw-config.json> --apply
ai-memory install openclaw --config <your-openclaw-config.json> --uninstall --apply
```

Handles **Part 1** below. Part 2 (system-message directive) is still
manual and best-effort until OpenClaw lands a native session-start hook.

## Part 1 — register `ai-memory-mcp` as an MCP server

Edit your OpenClaw config file (the canonical location is documented at
<https://docs.openclaw.ai/cli/mcp>) and add:

```json
{
  "mcp": {
    "servers": {
      "ai-memory": {
        "command": "ai-memory",
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

- `command: "ai-memory"` requires the binary to be on OpenClaw's `$PATH`.
  If it isn't, use the absolute path (`/opt/homebrew/bin/ai-memory` on
  macOS Homebrew, `/usr/local/bin/ai-memory` typical on Linux).
- `AI_MEMORY_DB` defaults to a path inside the user's home — set it
  explicitly to whichever DB you want shared with Claude Code or other
  agents. Sharing one DB across agents is the whole point of the design.
- Restart OpenClaw after editing the config so the MCP server is picked up.

## Part 2 — system-message directive (best-effort fallback)

OpenClaw does not document a SessionStart hook today, so we cannot
guarantee mechanical context injection on the first turn. The fallback is a
text directive in OpenClaw's system prompt / preset / agent definition
(consult the OpenClaw docs for the exact knob — agent presets are the
canonical surface) telling the model to call `memory_session_start` and
`memory_recall` before responding:

> Before responding to any user message in a fresh conversation, call the
> `ai-memory.memory_session_start` MCP tool and a `memory_recall` against
> the relevant project namespace (default: the current working directory's
> basename). Surface the recalled titles in your first reply so the user
> can confirm continuity.

Caveat: text directives are best-effort (issue #487 layer 6) — the model
may skip the call under load or competing instructions. Mechanical hook
support upstream is the only path to "100%."

## Better, when OpenClaw lands a session-start hook

We have an open feature request at the OpenClaw repo to add a documented
session-start hook (cross-filed from issue #487). When that ships, replace
Part 2 with a hook entry pointing at:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

This recipe will be updated in place once the hook lands.

## Verifying

```bash
# 1. Quit OpenClaw
# 2. cd /tmp && openclaw
# 3. Ask: "what do you remember?"
# 4. Expected (with Part 1 + Part 2 in place): the model calls
#    memory_session_start, sees recent memory rows, and references them in
#    its first reply.
```

If the model doesn't call `memory_session_start`, the directive isn't
firing — usually means the system prompt isn't being applied to the
session. Refer to OpenClaw's preset / agent docs for the correct surface.

## Related

- [`README.md`](README.md) — integration matrix and the universal primitive.
- Issue #487 — RCA + cross-files for category 2 native-hook requests.
