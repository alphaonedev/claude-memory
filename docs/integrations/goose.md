# Block Goose — MCP server + system-instructions directive

**Category 2 (MCP-capable, no native session-start hook).**
Reference: <https://block.github.io/goose/>

[Block Goose](https://block.github.io/goose/) is an open-source CLI /
desktop AI agent from Block (Square / Cash App). It has documented MCP
support, but no session-start hook today — so the recipe is two-part:
register `ai-memory-mcp` as an MCP server (so the model can call memory
tools at any time), and add a one-line system-instructions directive
telling the model to call `memory_session_start` before responding to
the first user turn.

## Part 1 — register `ai-memory-mcp` as an MCP server

Goose stores its config in a YAML / JSON file. The canonical path
varies across Goose versions and platforms (commonly
`~/.config/goose/config.yaml` on Linux/macOS), so verify with
`goose --help` or the Goose docs before editing. If your install uses
a non-default path, pass `--config <path>` to override (the same
pattern PR-2's installer uses).

Add an `ai-memory` entry under MCP servers (YAML form, since that's
Goose's idiomatic config):

```yaml
extensions:
  ai-memory:
    type: stdio
    cmd: ai-memory
    args:
      - mcp
    envs:
      AI_MEMORY_DB: ${HOME}/.claude/ai-memory.db
    enabled: true
```

If your Goose version uses JSON config instead, the equivalent block
(illustrative — the exact key names vary by version, do not blindly
paste):

```text
{
  "extensions": {
    "ai-memory": {
      "type": "stdio",
      "cmd": "ai-memory",
      "args": ["mcp"],
      "envs": { "AI_MEMORY_DB": "${HOME}/.claude/ai-memory.db" },
      "enabled": true
    }
  }
}
```

Notes:

- `cmd: "ai-memory"` requires the binary to be on Goose's `$PATH`.
  If it isn't, use the absolute path (`/opt/homebrew/bin/ai-memory` on
  macOS Homebrew, `/usr/local/bin/ai-memory` typical on Linux).
- `AI_MEMORY_DB` defaults to a path inside the user's home — set it
  explicitly to whichever DB you want shared with Claude Code or other
  agents. Sharing one DB across agents is the whole point of the
  design.
- Restart Goose after editing the config so the MCP server is picked
  up. `goose info` should list `ai-memory` under available extensions.

## Part 2 — system-instructions directive (best-effort fallback)

Goose supports per-agent system instructions via its profile / preset
config. Add the following directive to the system instructions for the
agent you use as your default driver:

> Before responding to any user message in a fresh conversation, call
> the `ai-memory.memory_session_start` MCP tool and a `memory_recall`
> against the relevant project namespace (default: the current working
> directory's basename). Surface the recalled titles in your first
> reply so the user can confirm continuity.

Caveat: text directives are best-effort (issue #487 layer 6) — the
model may skip the call under load or competing instructions.
Mechanical hook support upstream is the only path to "100%."

## Quick install

Manual install only until PR-2's installer follow-up adds explicit
Goose support. The YAML / JSON edit above is the manual form; track
the installer issue for one-line bootstrap that handles both config
shapes.

## End-user diagnostic

Goose surfaces MCP tool calls in its transcript, so when the directive
fires you will see `memory_session_start` in the tool-call log on the
first turn. If you don't, the directive isn't firing — usually means
the system instructions aren't being applied to the active profile.
The `ai-memory boot` status-header diagnostic (see
[`README.md`](README.md)) does not apply here because Goose's
integration is MCP-only, not boot-shellout-based.

## Limitations

- Category-2: text-directive in system instructions is subject to
  model compliance.
- Goose's config schema has evolved across releases; a recipe pinned
  to one schema can break on upgrade. The `--config <path>` override
  insulates against the canonical-path drift but not against schema
  drift.
- Some Goose builds gate MCP behind an experimental flag — confirm
  with `goose --help` / `goose info` that MCP is enabled before
  expecting the recipe to fire.

## Better, when Goose lands a session-start hook

We have an open feature request at the Block Goose repo to add a
documented session-start hook (cross-filed from issue #487). When that
ships, replace Part 2 with a hook entry pointing at:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

This recipe will be updated in place once the hook lands.

## Related

- [`README.md`](README.md) — integration matrix and the universal
  primitive.
- [`cline.md`](cline.md), [`continue.md`](continue.md) — same
  category-2 pattern for VS Code MCP hosts.
- Issue #487 — RCA + cross-files for category-2 native-hook requests.
