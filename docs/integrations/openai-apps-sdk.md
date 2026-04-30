# OpenAI Apps SDK / Assistants / Responses — system-message prepend

**Category 3 (programmatic).** 100% reliable when implemented.

OpenAI's Assistants API, Responses API, and Apps SDK all expose system
messages / instructions as the integration point. Prepend `ai-memory boot`
output before creating the assistant or before the first request.

## Assistants API (Python)

```python
import subprocess
from openai import OpenAI

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
instructions = "You are a helpful assistant."
if memory:
    instructions += f"\n\n## Recent context (ai-memory)\n{memory}"

client = OpenAI()
assistant = client.beta.assistants.create(
    name="memory-aware",
    instructions=instructions,
    model="gpt-4.1",
)
```

## Responses API (TypeScript)

```typescript
import OpenAI from "openai";
import { execSync } from "node:child_process";

const memory = (() => {
  try {
    return execSync(
      "ai-memory boot --quiet --no-header --format text --limit 10",
      { encoding: "utf-8" }
    ).trim();
  } catch { return ""; }
})();

const client = new OpenAI();
const response = await client.responses.create({
  model: "gpt-4.1",
  instructions: `You are a helpful assistant.${memory ? `\n\n## Recent context (ai-memory)\n${memory}` : ""}`,
  input: userMessage,
});
```

## Apps SDK

The Apps SDK uses an `instructions` field on the App Definition. Build the
string the same way as the other examples and pass it at app construction.

## Caveats

- For long-lived assistants, boot context becomes stale. Prefer recreating
  the assistant per session, or use the `additional_instructions` field
  on `runs.create` to inject fresh boot context per run.
- For Responses API: `instructions` is per-request, so freshness is free.

## Related

- [`README.md`](README.md), Issue #487
