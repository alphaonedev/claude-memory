# Claude Code — SessionStart hook (reference recipe)

**Category 1 (hook-capable). 100% reliable.** This is the load-bearing
remediation for issue [#487](https://github.com/alphaonedev/ai-memory-mcp/issues/487).

## Quick install

```bash
# Preview the change (dry-run is the default — writes nothing):
ai-memory install claude-code

# Commit the change:
ai-memory install claude-code --apply

# Remove later:
ai-memory install claude-code --uninstall --apply
```

The installer writes the SessionStart hook block into `~/.claude/settings.json`
inside a clearly-marked managed block, backs up the original to
`<config>.bak.<timestamp>` first, and is idempotent — re-running `--apply`
with no upstream changes is a no-op. Pass `--config <path>` to target a
non-default settings file (project-scoped or test fixture).

## What it does

Claude Code supports a `SessionStart` hook in `~/.claude/settings.json` (or
the project's `.claude/settings.json`) that runs a shell command at session
boot. The hook's stdout is injected into the conversation as additional
context, **before the model processes the first user message**. We point
that hook at `ai-memory boot` so every fresh session starts memory-aware
with no prompt from the user.

## One-time install

Edit `~/.claude/settings.json` (create it if missing) and add a `hooks`
block. If you already have other hooks, append the entry inside the
existing `SessionStart` array.

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "ai-memory boot --quiet --limit 10 --budget-tokens 4096"
          }
        ]
      }
    ]
  }
}
```

That's it. Restart Claude Code. The next session will see your most-recent
memory context as part of its system prompt.

## Why these flags

- `--quiet`: a DB-unavailable failure becomes silent on stderr (so it
  doesn't pollute the agent log) but the **diagnostic header still
  appears on stdout** so the agent and the human running it always know
  whether boot fired and why context might be missing.
- `--limit 10` + `--budget-tokens 4096`: bounds the cost of every session.
  Tune up if you want richer context, down if your first turns are
  latency-sensitive.

**Do not** add `--no-header` to a hook command. The header is the
end-user-visible signal that ai-memory ran. Suppressing it makes silent
failure indistinguishable from "no memories yet" — exactly the failure
mode issue #487 is fixing.

## End-user diagnostic — how to know boot fired

Every boot invocation emits one of four status headers:

| Header prefix | Meaning |
|---|---|
| `# ai-memory boot: ok — loaded N memories from ns=X` | Normal happy path. |
| `# ai-memory boot: info — namespace empty; loaded N memories from global Long tier fallback` | Requested namespace was empty; we surfaced cross-project context. |
| `# ai-memory boot: info — namespace 'X' is empty and no global Long-tier fallback found — nothing to load (this is normal on a fresh install)` | First-run / greenfield. Not an error. |
| `# ai-memory boot: warn — db unavailable at /path — proceeding without memory context. Run \`ai-memory doctor\` to diagnose.` | The hook fired but the DB couldn't be opened. Common causes: wrong `AI_MEMORY_DB` path, permission denied, brand-new install before first `ai-memory store`. |

If you see **no header at all** in your session log, the hook never fired.
Run the diagnostic from the same shell Claude Code launched from:

```text
ai-memory boot --limit 1

# Should emit one of the four headers above.
# If it errors instead, the binary or DB is misconfigured.
# If it works but Claude Code never sees a header, the SessionStart hook
#   isn't installed correctly — re-check ~/.claude/settings.json.
```

## Project-scoped namespace override

If you want a specific project to load from a sub-namespace (e.g.
`ai-memory-mcp/v0631-release` instead of the default `ai-memory-mcp`), put
a project-level `.claude/settings.json` at the repo root with:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "ai-memory boot --quiet --no-header --namespace ai-memory-mcp/v0631-release --limit 10"
          }
        ]
      }
    ]
  }
}
```

Project-level hooks merge with user-level hooks; both fire. Best practice:
keep user-level hook generic (let `auto_namespace` infer from cwd) and use
project-level hooks only when you need a non-default namespace.

## Verifying

Cold-start test (the issue #487 acceptance criterion):

```text
# 1. Quit Claude Code entirely.
# 2. From a fresh shell, anywhere on the filesystem:
cd /tmp
claude

# 3. First message:
#    > what do you remember?
# 4. Expected: Claude responds with recalled titles, namespaces, ages,
#    no "I do not have context to continue this", no need to type
#    "access your memories".
```

If the cold-start fails, check:

1. `which ai-memory` returns a path (the binary is on `$PATH`).
2. `ai-memory boot --quiet --no-header --limit 3` from your shell returns
   memory rows.
3. `cat ~/.claude/settings.json | jq .hooks.SessionStart` shows the hook
   block.
4. The hook command is on a single line (Claude Code's `command` field is
   a single string, not an array).

## Uninstall

```bash
ai-memory install claude-code --uninstall   # see PR-2 (issue #487 item E)
```

Or remove the entry by hand from `~/.claude/settings.json`.

## What this does NOT solve

- **Long-running sessions**: the hook only fires at session start. If you
  store new memories mid-session, they don't get pulled in until the next
  session. Use `memory_recall` directly when you need fresh context.
- **Tool deferral**: Claude Code may still defer the `mcp__memory__*` tool
  schemas, requiring a `ToolSearch` round-trip the first time the model
  wants to call them. The hook injects context as text — the model has it
  even before tools resolve. This is the right architectural separation.

## Related

- [`README.md`](README.md) — integration matrix and the `ai-memory boot` primitive.
- Issue #487 — the RCA and full remediation plan.
- Issue F (cross-filed at `anthropics/claude-code`) — request that MCP
  servers can mark tools as boot-priority so deferred-tool round-trips
  don't matter even for the second turn.
