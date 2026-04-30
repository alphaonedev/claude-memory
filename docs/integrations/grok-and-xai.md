# xAI Grok — programmatic prepend or via Cursor

**Category 3 (programmatic) for raw API; category 2 if used via Cursor.**

xAI's Grok models are accessible via the xAI API (raw HTTP / OpenAI-compat
SDK), via Cursor (where Grok is one of several model choices), and via the
xAI consumer apps. The integration depends on the surface.

## Via the xAI API (programmatic — recommended)

The xAI API is OpenAI-compatible. Use the `openai-apps-sdk.md` recipe
verbatim, swapping the base URL and model:

```python
import subprocess
from openai import OpenAI

memory = subprocess.check_output(
    ["ai-memory", "boot", "--quiet", "--no-header", "--format", "text", "--limit", "10"],
    text=True,
).strip()

client = OpenAI(
    api_key=os.environ["XAI_API_KEY"],
    base_url="https://api.x.ai/v1",
)

instructions = "You are a helpful assistant."
if memory:
    instructions += f"\n\n## Recent context (ai-memory)\n{memory}"

response = client.chat.completions.create(
    model="grok-2-latest",
    messages=[
        {"role": "system", "content": instructions},
        {"role": "user", "content": user_message},
    ],
)
```

100% reliable.

## Via Cursor (Grok Code Fast 1, etc.)

Use the [`cursor.md`](cursor.md) recipe — Grok runs inside Cursor's MCP
host, so the memory wiring is identical to any other Cursor model.
Category 2 (best-effort directive in `.cursorrules` until Cursor lands a
session-start hook).

## Via the xAI consumer app

The consumer Grok app does not expose tooling for MCP integration today.
No recipe yet — track for when xAI adds developer hooks.

## Related

- [`README.md`](README.md), Issue #487
- [`openai-apps-sdk.md`](openai-apps-sdk.md) — same pattern for any
  OpenAI-compatible API.
