# Local models (Hermes, Llama, Mistral, etc.) — wrap the chat call

**Category 3 (programmatic).** 100% reliable when implemented.

Hermes (Nous Research), Llama, Mistral, Qwen, and other open-weight models
run locally via LM Studio, Ollama, vLLM, llama.cpp, etc. None of these
runtimes ship a session-start hook today; integration is at the application
boundary. The pattern is the same regardless of runtime: the front-end app
or wrapper script prepends `ai-memory boot` output to the system message
before the first request.

## LM Studio (HTTP API, OpenAI-compatible)

LM Studio exposes an OpenAI-compat server on port 1234 by default. Use the
[`openai-apps-sdk.md`](openai-apps-sdk.md) recipe with `base_url` set to
`http://localhost:1234/v1`.

## Ollama (HTTP API)

```python
import subprocess
import requests

memory = subprocess.check_output(
    ["ai-memory", "boot", "--quiet", "--no-header", "--format", "text", "--limit", "10"],
    text=True,
).strip()

system = "You are a helpful assistant."
if memory:
    system += f"\n\n## Recent context (ai-memory)\n{memory}"

resp = requests.post(
    "http://localhost:11434/api/chat",
    json={
        "model": "hermes3:8b",
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_message},
        ],
        "stream": False,
    },
).json()
```

## vLLM / llama.cpp server

Same OpenAI-compatible API as LM Studio. Use the
[`openai-apps-sdk.md`](openai-apps-sdk.md) recipe with the appropriate
`base_url`.

## Why no MCP recipe for local models

Most local-model front-ends (Open WebUI, AnythingLLM, Continue, etc.) talk
to MCP differently. If you're using a front-end with first-class MCP
support, see the relevant agent's recipe in this directory (e.g.
[`continue.md`](continue.md)) — local models work the same way as cloud
models behind that front-end.

If you're calling a runtime directly (Ollama HTTP, vLLM HTTP), there's no
MCP host in the loop, so the boot recipe is purely the system-message
prepend pattern shown above.

## Related

- [`README.md`](README.md), Issue #487
- [`openai-apps-sdk.md`](openai-apps-sdk.md) — the canonical
  OpenAI-compatible-API pattern.
