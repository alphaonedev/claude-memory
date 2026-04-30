# Sourcegraph Cody — programmatic system-message prepend

**Category 3 (programmatic).** 100% reliable when implemented.

[Cody](https://sourcegraph.com/cody) is Sourcegraph's AI coding
assistant. It ships as VS Code / JetBrains extensions and a CLI
(`cody`), and exposes a programmatic chat API for Sourcegraph
Enterprise customers. There is no documented session-start hook and no
MCP host today, so the integration is at the application boundary:
shell out to `ai-memory boot` and prepend the result to the system
message of the Cody chat request.

## Cody CLI wrapper

If you drive Cody via its CLI, save as `~/.local/bin/cody-with-memory`
and make it executable:

```bash
#!/usr/bin/env bash
# Wraps `cody chat` with ai-memory boot context on the system message.
set -euo pipefail

BOOT_CONTEXT=$(ai-memory boot --quiet --no-header --format text --limit 10 || true)

# Cody CLI's chat surface accepts a custom prompt via --message or
# --context-file in some builds. Check `cody chat --help` for the
# flag your install supports.
if [[ -n "$BOOT_CONTEXT" ]]; then
  PREAMBLE="You have access to ai-memory. Recent context follows; reference it when relevant to the request."
  CONTEXT_FILE=$(mktemp -t cody-memory.XXXXXX)
  trap 'rm -f "$CONTEXT_FILE"' EXIT
  printf '%s\n\n%s\n' "$PREAMBLE" "$BOOT_CONTEXT" > "$CONTEXT_FILE"
  exec cody chat --context-file "$CONTEXT_FILE" "$@"
else
  exec cody chat "$@"
fi
```

## Programmatic — Cody chat API directly

If you call the Cody chat API from your own application (Sourcegraph
Enterprise customers using the GraphQL or REST surface), inject the
boot context as a system-role message at the head of `messages`:

```python
import os
import subprocess
import requests

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
system_content = "You are a helpful coding assistant."
if memory:
    system_content += f"\n\n## Recent context (ai-memory)\n{memory}\n"

# Cody Enterprise chat API endpoint — adjust to your instance.
endpoint = f"{os.environ['SOURCEGRAPH_URL']}/.api/completions/stream"
headers = {
    "Authorization": f"token {os.environ['SOURCEGRAPH_TOKEN']}",
    "Content-Type": "application/json",
}
body = {
    "messages": [
        {"speaker": "system", "text": system_content},
        {"speaker": "human", "text": user_message},
    ],
}
response = requests.post(endpoint, json=body, headers=headers)
```

100% reliable when implemented.

```typescript
import { execSync } from "node:child_process";

function bootContext(): string {
  try {
    return execSync(
      "ai-memory boot --quiet --no-header --format text --limit 10",
      { encoding: "utf-8" }
    ).trim();
  } catch {
    return "";
  }
}

const memory = bootContext();
let systemContent = "You are a helpful coding assistant.";
if (memory) {
  systemContent += `\n\n## Recent context (ai-memory)\n${memory}\n`;
}

const body = {
  messages: [
    { speaker: "system", text: systemContent },
    { speaker: "human", text: userMessage },
  ],
};
// POST body to your Sourcegraph instance's /.api/completions/stream
```

## Quick install

Manual install only until PR-2's installer follow-up adds explicit
Cody support. The wrapper / API recipe above is the manual form.

## End-user diagnostic

When using the wrapper script with the header preserved, the
`ai-memory boot` status header appears in stdout / the chat
transcript. The four headers documented in [`README.md`](README.md)
tell `ok` / `info-empty` / `info-greenfield` / `warn-db` apart. For
the programmatic API recipe, add a logging line after `boot_context()`
returns — the empty string vs a populated payload is itself a
diagnostic.

## Limitations

- Cody does not host MCP servers; `ai-memory-mcp` cannot be registered
  as a tool here. Mid-session recall (beyond the boot prepend) would
  require Cody to grow MCP support upstream.
- The Cody chat API surface differs between Sourcegraph Cloud
  (managed, OAuth) and Sourcegraph Enterprise (self-hosted, token
  auth). The recipe above targets Enterprise; for Cloud, adapt the
  auth header and endpoint accordingly.
- The Cody VS Code / JetBrains extensions do not yet expose a
  user-configurable system prompt or hook, so the wrapper does not
  apply to those surfaces — track upstream for a developer hook.
- This recipe loads memory **once per CLI invocation / API call**.
  Multi-turn conversations within one invocation share the boot
  context.

## Better, when Cody lands a session-start hook

We have an open feature request at the Sourcegraph Cody repo to add
a documented session-start hook (cross-filed from issue #487). When
that ships, replace the wrapper / prepend with a hook entry pointing
at:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

This recipe will be updated in place once the hook lands.

## Related

- [`README.md`](README.md) — integration matrix and the universal
  primitive.
- [`claude-agent-sdk.md`](claude-agent-sdk.md),
  [`openai-apps-sdk.md`](openai-apps-sdk.md) — same prepend pattern
  for other programmatic SDKs.
- Issue #487 — RCA + cross-files for the Cody hook request.
