# OpenAI Codex CLI — programmatic system-message prepend

**Category 3 (programmatic).** 100% reliable when implemented.

OpenAI's Codex CLI does not have an MCP host or a session-start hook
mechanism. The integration is at the application boundary: shell out to
`ai-memory boot` and prepend the result to the system message before each
new conversation.

## Wrapper script

Save as `~/.local/bin/codex-with-memory` and make it executable:

```bash
#!/usr/bin/env bash
# Wraps `codex` with ai-memory boot context on the system message.
set -euo pipefail

BOOT_CONTEXT=$(ai-memory boot --quiet --no-header --format text --limit 10 || true)

# Append boot context to the system message via Codex's --system flag (or
# OPENAI_CLI_SYSTEM env var, depending on which Codex CLI you're running).
if [[ -n "$BOOT_CONTEXT" ]]; then
  exec codex --system "$(cat <<EOF
You have access to ai-memory. Recent context follows; reference it when
relevant to the user's request.

$BOOT_CONTEXT
EOF
)" "$@"
else
  exec codex "$@"
fi
```

Then alias `codex` to this wrapper, or invoke `codex-with-memory` instead.

## Caveats

- The exact flag name (`--system`, `--system-prompt`, env var) depends on
  which Codex CLI variant is installed. Check `codex --help`.
- This recipe loads memory **once per CLI invocation**. Multi-turn
  conversations within one invocation share the boot context.
- For richer memory access (mid-session), the developer would need to add
  function-calling support pointing at `ai-memory`'s HTTP API. That's a
  larger integration than the boot recipe and lives outside this doc.

## Related

- [`README.md`](README.md), Issue #487
