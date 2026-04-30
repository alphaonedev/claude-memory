# Session-boot integrations — agent matrix

This directory documents how to wire `ai-memory` into every AI agent so the
**first turn of every session** sees relevant memory context, with no manual
prompt from the user. It is the user-facing remediation for issue
[#487](https://github.com/alphaonedev/ai-memory-mcp/issues/487) (cold-start
sessions don't auto-load memory).

## The universal primitive

Every recipe in this directory invokes the same CLI:

```bash
ai-memory boot \
  --namespace "<inferred-or-explicit>" \
  --limit 10 \
  --budget-tokens 4096 \
  --format text \
  --quiet
```

`ai-memory boot` is read-only, fast (no embedder, no daemon, indexed list
only), and graceful by default. With `--quiet`, a missing or unreachable
DB still exits 0 — a misconfigured agent never wedges its first turn —
but **a status header always appears on stdout** so the agent (and the
human running it) can see whether boot fired and why context might be
missing. See `ai-memory boot --help` for the full surface.

### End-user diagnostic — always-visible manifest header

Every invocation emits a transparent multi-field manifest (per [#487
addendum](https://github.com/alphaonedev/ai-memory-mcp/issues/487), PR-4):
agents and humans always know exactly what's loaded and what's configured.

```text
# ai-memory boot: ok
#   version:    0.6.3+patch.1
#   db:         /home/u/.claude/ai-memory.db (schema=v19, 161 memories)
#   tier:       autonomous (embedder=nomic-ai/nomic-embed-text-v1.5, reranker=ms-marco-MiniLM-L-6-v2, llm=gemma4:e4b)
#   latency:    12ms
#   namespace:  ai-memory-mcp/v0631-release (loaded 3 memories)
```

The four status variants share the same manifest shape; only the first
line's status word and the `namespace:` line's parenthetical change:

| Status | First line | `namespace:` line |
|---|---|---|
| Happy path | `# ai-memory boot: ok` | `(loaded N memories)` |
| Namespace empty, fell back | `# ai-memory boot: info` | `(fallback: loaded N memories from global Long tier)` |
| First-run / greenfield | `# ai-memory boot: info` | `(empty — nothing to load; this is normal on a fresh install)` |
| DB unavailable | `# ai-memory boot: warn` | `(db unavailable — see `ai-memory doctor`)` |

In the warn variant, `db:` reports `<unavailable>` for `schema` and
the live-memories count, while `version`, `tier`, and `latency` still
surface — the manifest never disappears, only what depends on a live
DB is sentinelled.

**If no manifest appears at all**, the integration recipe didn't
fire the hook — the agent host either skipped the hook, the binary isn't
on `$PATH`, or the recipe is misconfigured. This absence is itself a
diagnostic: silent vs. "warn" vs. "ok" tell the user three different
failure modes apart.

`--no-header` is supported but should NOT be used in production hooks —
suppressing the header makes silent failure indistinguishable from "no
memories yet."

The output body (after the manifest, when memories are loaded) is one of
three formats:

- `text` (default) — human-readable bulleted list. Works in any agent's
  system message. Easiest to scan.
- `json` — single object containing every manifest field as a top-level
  key (`status`, `version`, `db_path`, `schema_version`, `total_memories`,
  `tier`, `embedder`, `reranker`, `llm`, `latency_ms`, `namespace`,
  `count`, `note`, `memories`, `fell_back_to_global`) for programmatic
  ingest (Claude Agent SDK, OpenAI Apps SDK, Codex CLI prepend).
- `toon` — the canonical token-efficient memory format, byte-identical
  to a `memory_recall` MCP response.

## Three integration categories

| Category | Agent host has… | How memory gets loaded on first turn | Example agents |
|---|---|---|---|
| **1. Hook-capable** | A documented session-start hook the user can configure | Hook runs `ai-memory boot`; stdout is injected as additional context. **100% reliable.** | Claude Code |
| **2. MCP-capable, no hook** | An MCP client and a project-rules / system-prompt file but no session-start hook | `ai-memory-mcp` registered as an MCP server **plus** a one-line directive in the agent's rules file telling the model to call `memory_session_start` first. **Best-effort** (text-directive subject to model compliance). | Cursor, Cline, Continue, Windsurf, OpenClaw |
| **3. Programmatic only** | An SDK or raw API where the developer assembles each request | Application code shells out to `ai-memory boot --quiet --format json` and prepends the result to the system message at session/conversation start. **100% reliable when implemented.** | Codex CLI, Claude Agent SDK, OpenAI Apps SDK / Assistants API / Responses API, Grok via xAI API, Hermes / local models via LM Studio / Ollama / vLLM |

The bar for "100% remediated" is: every supported agent has a recipe that
loads memory on the first turn without user prompting. Categories 1 and 3
hit that bar today; category 2 is best-effort until upstream agents grow a
proper session-start hook (see issue #487 cross-files).

## Per-agent recipes

| File | Agent | Category | Status |
|---|---|---|---|
| [`claude-code.md`](claude-code.md) | Claude Code (CLI, Mac/Win desktop, IDE) | 1 (hook) | reference recipe |
| [`cursor.md`](cursor.md) | Cursor | 2 (MCP + rules) | recipe |
| [`cline.md`](cline.md) | Cline (VS Code extension) | 2 (MCP + custom instructions) | recipe |
| [`continue.md`](continue.md) | Continue (VS Code / JetBrains) | 2 (MCP + systemMessage) | recipe |
| [`windsurf.md`](windsurf.md) | Windsurf (Codeium) | 2 (MCP + rules) | recipe |
| [`openclaw.md`](openclaw.md) | OpenClaw CLI | 2 (MCP + system message) | recipe |
| [`codex-cli.md`](codex-cli.md) | OpenAI Codex CLI | 3 (programmatic) | recipe |
| [`claude-agent-sdk.md`](claude-agent-sdk.md) | Claude Agent SDK | 3 (programmatic) | recipe (TS + Python) |
| [`openai-apps-sdk.md`](openai-apps-sdk.md) | OpenAI Apps SDK / Assistants / Responses | 3 (programmatic) | recipe |
| [`grok-and-xai.md`](grok-and-xai.md) | xAI Grok | 3 (programmatic) | recipe |
| [`local-models.md`](local-models.md) | Hermes, Llama, Mistral, etc. via LM Studio / Ollama / vLLM | 3 (programmatic) | recipe |
| [`platforms.md`](platforms.md) | macOS / Linux / Windows / WSL / Docker / BSD platform notes | n/a | reference |
| [`global-claude-md-template.md`](global-claude-md-template.md) | `~/.claude/CLAUDE.md` belt-and-suspenders snippet | 1 fallback | reference |

## Failure modes (any recipe)

- DB unreachable: pass `--quiet` to the boot call. Hook/wrapper exits 0,
  agent starts with no extra context (graceful degrade, no hang).
- Wrong namespace: `auto_namespace` falls back to cwd basename → "global".
  If still empty, boot returns the most-recently-accessed `tier=long`
  memories globally so a greenfield checkout still has cross-project
  context.
- Hook output too large: `--budget-tokens` (default 4096) clamps the row
  count cheaply (cumulative chars / 4 ≈ tokens).

## Verifying a recipe

After installing any recipe, prove it works with the cold-start test:

1. Quit the agent completely.
2. Open a fresh window in **a directory other than the ai-memory project root**
   (this catches recipes that depend on project-local config — they should
   work everywhere or not be billed as "100%").
3. Send the agent a single first message: `what do you remember?`
4. The agent should respond with concrete recalled context (titles,
   namespaces, ages) **without** you having to type "access your memories"
   first.

If step 4 fails on a recipe that claims category 1 or 3, that recipe has a
bug and the fix lands in this directory.

## Cross-org follow-ups

Category 2 agents (Cursor, Cline, Continue, Windsurf, OpenClaw) all need
native session-start hooks to reach 100% remediation. Cross-files tracking
those upstream requests live in #487's comments.

The MCP spec proposal at
`modelcontextprotocol/specification` for a `session/initialize` server
callback is the universal architectural fix. Once accepted, it closes
category 2 entirely without per-host work.
