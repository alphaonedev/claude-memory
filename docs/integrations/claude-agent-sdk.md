# Claude Agent SDK — programmatic system-message prepend

**Category 3 (programmatic).** 100% reliable when implemented.

The Claude Agent SDK is programmatic by design — the developer constructs
the messages array. Prepend `ai-memory boot` output to the system message
on session/conversation start.

## Or for the simple wrapper case — `ai-memory wrap`

If your integration is just "spawn a CLI that calls Claude", PR-6 of
issue #487 ships a built-in cross-platform Rust subcommand:

```bash
ai-memory wrap claude-cli -- chat --model claude-opus-4-7
```

`ai-memory wrap` runs `ai-memory boot` in-process, builds a system
message of the form `<preamble>\n\n<boot output>`, spawns the named
agent CLI with that message delivered via the appropriate strategy
(`--system <msg>` for most agents, env var for Ollama, message file
for aider), and propagates the agent's exit code. Pure Rust — same
binary works on macOS / Linux / Windows / Docker / Kubernetes with
no shell wrapper.

For SDK code that constructs requests directly, the patterns below
are what you want — `wrap` is for the launcher case where the SDK
isn't in your code path.

## TypeScript

```typescript
import Anthropic from "@anthropic-ai/sdk";
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

const client = new Anthropic();
const memory = bootContext();

const systemMessage =
  `You are a helpful assistant. ` +
  (memory
    ? `\n\n## Recent context (ai-memory)\n${memory}\n`
    : "");

const response = await client.messages.create({
  model: "claude-opus-4-7",
  max_tokens: 4096,
  system: systemMessage,
  messages: [{ role: "user", content: userMessage }],
});
```

## Python

```python
import subprocess
import anthropic

def boot_context() -> str:
    try:
        out = subprocess.check_output(
            ["ai-memory", "boot", "--quiet", "--no-header",
             "--format", "text", "--limit", "10"],
            text=True,
        )
        return out.strip()
    except Exception:
        return ""

memory = boot_context()
system_message = "You are a helpful assistant."
if memory:
    system_message += f"\n\n## Recent context (ai-memory)\n{memory}\n"

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-opus-4-7",
    max_tokens=4096,
    system=system_message,
    messages=[{"role": "user", "content": user_message}],
)
```

## Prompt caching

Wrap the `system_message` in a cache breakpoint so the boot context (which
is stable across turns within a session) hits cache after the first
request. See the `claude-api` skill for the exact pattern — boot context
is one of the canonical examples for cache-friendly system prompts.

## Optional — register `ai-memory-mcp` as a tool too

For mid-session recall (beyond the boot context), expose `memory_recall`
as a tool. The boot prepend is for *first-turn awareness*; tool access is
for *active recall*. Both layers are valuable; ship both for the richest
agent.

## Related

- [`README.md`](README.md), Issue #487
- The `claude-api` skill in this repo for prompt caching patterns.
