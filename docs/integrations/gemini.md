# Google Gemini CLI / Gemini Code Assist — programmatic prepend

**Category 3 (programmatic).** 100% reliable when implemented.

Google's Gemini CLI is the reference command-line entry point for the
Gemini API and Gemini Code Assist. There is no documented session-start
hook today, and MCP support is not native (the CLI is partially
OpenAI-API-compatible in some variants but not an MCP host). The
integration is at the application boundary: shell out to `ai-memory boot`
and prepend the result to the system instruction (or the first message
of the conversation when no system slot exists).

The canonical Rust-native cross-platform replacement for the wrapper
script below is `ai-memory wrap gemini` (PR-6 of issue #487) — same
semantics, no shell required, works on Windows / Docker / Kubernetes.

## Wrapper script

Save as `~/.local/bin/gemini-with-memory` and make it executable:

```bash
#!/usr/bin/env bash
# Wraps `gemini` (Google Gemini CLI) with ai-memory boot context on the
# system instruction. Recipe shown in bash for clarity; PR-6 of issue
# #487 ships an `ai-memory wrap gemini` Rust subcommand with identical
# semantics.
set -euo pipefail

BOOT_CONTEXT=$(ai-memory boot --quiet --no-header --format text --limit 10 || true)

# Some Gemini CLI builds accept --system, others use GEMINI_SYSTEM_INSTRUCTION
# env var or a -s short form. Check `gemini --help` to confirm. The
# OpenAI-compatible variants accept --system verbatim.
if [[ -n "$BOOT_CONTEXT" ]]; then
  PREAMBLE="You have access to ai-memory. Recent context follows; reference it when relevant to the request."
  exec gemini --system "${PREAMBLE}

${BOOT_CONTEXT}" "$@"
else
  exec gemini "$@"
fi
```

Then alias `gemini` to this wrapper, or invoke `gemini-with-memory`
instead. For the Rust-native version (no shell, works on Windows):

```bash
ai-memory wrap gemini -- <gemini args>
```

## Programmatic — Gemini API directly

If you are calling the Gemini API from your own application (Python, Go,
TypeScript, etc.), prepend the boot context to `system_instruction` on
session start:

```python
import subprocess
import google.generativeai as genai

def boot_context() -> str:
    try:
        return subprocess.check_output(
            ["ai-memory", "boot", "--quiet", "--no-header",
             "--format", "text", "--limit", "10"],
            text=True,
        ).strip()
    except Exception:
        return ""

memory = boot_context()
system_instruction = "You are a helpful assistant."
if memory:
    system_instruction += f"\n\n## Recent context (ai-memory)\n{memory}\n"

model = genai.GenerativeModel(
    model_name="gemini-2.0-flash",
    system_instruction=system_instruction,
)
response = model.generate_content(user_message)
```

100% reliable when implemented.

## Quick install

Manual install only until PR-2's installer follow-up adds explicit
Gemini support. The wrapper script above is the manual form; track the
installer issue for one-line bootstrap.

## End-user diagnostic

Every wrapper invocation emits `ai-memory boot`'s status header on
stdout (when `--no-header` is omitted). The four headers documented in
[`README.md`](README.md) tell `ok` / `info-empty` / `info-greenfield`
/ `warn-db` apart. If you see no header at all, the wrapper itself
isn't firing — check `which gemini` resolves to the wrapper and not the
upstream binary.

## Limitations

- The exact flag (`--system`, `-s`, `GEMINI_SYSTEM_INSTRUCTION` env var)
  varies across Gemini CLI builds. Check `gemini --help` for your
  install before committing the wrapper to production.
- Gemini CLI does not have an MCP host today. `ai-memory-mcp` cannot be
  registered as an MCP server here. Mid-session recall would require
  adding function-calling support pointing at `ai-memory`'s HTTP API —
  out of scope for the boot recipe.
- This recipe loads memory **once per CLI invocation**. Multi-turn
  conversations within one invocation share the boot context.
- Gemini Code Assist (the IDE plug-in surface) does not yet expose a
  user-configurable system prompt or hook, so the wrapper recipe does
  not apply to that surface — track upstream for developer hooks.

## Better, when Gemini CLI lands a session-start hook

We have an open feature request at the Google Gemini CLI repo to add a
documented session-start hook (cross-filed from issue #487). When that
ships, replace the wrapper with a hook entry pointing at:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

This recipe will be updated in place once the hook lands.

## Related

- [`README.md`](README.md) — integration matrix and the universal
  primitive.
- [`codex-cli.md`](codex-cli.md) — same wrapper pattern, OpenAI variant.
- [`openai-apps-sdk.md`](openai-apps-sdk.md) — same prepend pattern for
  any OpenAI-compatible API.
- Issue #487 — RCA + cross-files for the Gemini CLI hook request.
