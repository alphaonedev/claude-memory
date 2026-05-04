# Global `~/.claude/CLAUDE.md` template

**Belt-and-suspenders for category 1 (Claude Code).** The
[claude-code.md](claude-code.md) SessionStart hook is the load-bearing
mechanism — text directives in CLAUDE.md alone are best-effort (see issue
#487 layer 6). But a global CLAUDE.md helps in two cases:

1. Sessions launched from a directory whose project doesn't have its own
   CLAUDE.md (the bug surfaced in issue #487 was exactly this).
2. Sessions where the SessionStart hook fails silently — the model still
   has a written reminder to recover.

Drop the snippet below into `~/.claude/CLAUDE.md` (create the file if it
doesn't exist). Claude Code merges this into the system prompt for every
session regardless of cwd.

## Snippet

```markdown
# Global session conventions

## ai-memory at session start

This machine has the ai-memory MCP server installed. Memory persists
across all sessions and projects via a shared local DB.

If a SessionStart hook (see ~/.claude/settings.json) is configured, recent
context is already in the system prompt — confirm by skimming for a
"# ai-memory boot context" header. If absent, before responding to the
user's first message, call `mcp__memory__memory_session_start` (or, if
that tool isn't loaded yet, `ToolSearch` with
`select:mcp__memory__memory_session_start` and then call it). Reference
recalled titles in your first reply.

Default namespace: derived from the working directory's basename. Override
per-project in that project's CLAUDE.md or .claude/settings.json hook.

If the memory tools are unavailable for any reason, proceed without — do
not block the conversation on memory load.
```

## Verifying the global directive fires

```bash
# 1. cd /tmp (a directory with no project CLAUDE.md)
# 2. claude
# 3. First message: "what do you remember?"
# 4. Expected: Claude calls memory_session_start without you having to ask.
```

If step 4 fails, the global CLAUDE.md isn't being loaded (check the
file's path is exactly `~/.claude/CLAUDE.md`) or the model is being
overridden by a stronger directive in the system prompt.

## Related

- [`claude-code.md`](claude-code.md) — the load-bearing SessionStart hook.
- [`README.md`](README.md) — full integration matrix.
- Issue #487 — RCA explaining why text directives are belt-and-suspenders
  rather than a fix.
