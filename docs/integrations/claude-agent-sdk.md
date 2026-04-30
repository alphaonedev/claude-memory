# Claude Agent SDK — programmatic system-message prepend

**Category 3 (programmatic).** 100% reliable when implemented.

The Claude Agent SDK is programmatic by design — the developer constructs
the messages array. Prepend `ai-memory boot` output to the system message
on session/conversation start.

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
