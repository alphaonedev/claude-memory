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

### End-user diagnostic — always-visible status header

Every invocation emits exactly one of these four headers (per [#487
addendum](https://github.com/alphaonedev/ai-memory-mcp/issues/487)):

| Header | Meaning |
|---|---|
| `# ai-memory boot: ok — loaded N memories from ns=X` | Normal happy path. |
| `# ai-memory boot: info — namespace empty; loaded N memories from global Long tier fallback` | Requested namespace empty; surfacing cross-project context instead. |
| `# ai-memory boot: info — namespace 'X' is empty and no global Long-tier fallback found — nothing to load (this is normal on a fresh install)` | First-run / greenfield. |
| `# ai-memory boot: warn — db unavailable at /path — proceeding without memory context. Run \`ai-memory doctor\` to diagnose.` | DB couldn't be opened. The hook fired but found no DB. |

**If no status header appears at all**, the integration recipe didn't
fire the hook — the agent host either skipped the hook, the binary isn't
on `$PATH`, or the recipe is misconfigured. This absence is itself a
diagnostic: silent vs. "warn" vs. "ok" tell the user three different
failure modes apart.

`--no-header` is supported but should NOT be used in production hooks —
suppressing the header makes silent failure indistinguishable from "no
memories yet."

The output body (after the header, when memories are loaded) is one of
three formats:

- `text` (default) — human-readable bulleted list. Works in any agent's
  system message. Easiest to scan.
- `json` — single object containing `status`, `namespace`, `count`,
  `memories`, `note` for programmatic ingest (Claude Agent SDK, OpenAI
  Apps SDK, Codex CLI prepend).
- `toon` — the canonical token-efficient memory format, byte-identical
  to a `memory_recall` MCP response.

## Three integration categories

| Category | Agent host has… | How memory gets loaded on first turn | Example agents |
|---|---|---|---|
| **1. Hook-capable** | A documented session-start hook the user can configure | Hook runs `ai-memory boot`; stdout is injected as additional context. **100% reliable.** | Claude Code |
| **2. MCP-capable, no hook** | An MCP client and a project-rules / system-prompt file but no session-start hook | `ai-memory-mcp` registered as an MCP server **plus** a one-line directive in the agent's rules file telling the model to call `memory_session_start` first. **Best-effort** (text-directive subject to model compliance). | Cursor, Cline, Continue, Windsurf, OpenClaw |
| **3. Programmatic only** | An SDK or raw API where the developer assembles each request | Application code uses the SDK pattern (prepends `ai-memory boot` output to the system message). For the launcher case (just spawn a CLI), `ai-memory wrap <agent>` is the cross-platform Rust replacement for the bash / PowerShell wrappers earlier PRs shipped — it runs the same code path on macOS / Linux / Windows / Docker / Kubernetes. **100% reliable when implemented.** | Codex CLI, Claude Agent SDK, OpenAI Apps SDK / Assistants API / Responses API, Grok via xAI API, Hermes / local models via LM Studio / Ollama / vLLM |

The bar for "100% remediated" is: every supported agent has a recipe that
loads memory on the first turn without user prompting. Categories 1 and 3
hit that bar today; category 2 is best-effort until upstream agents grow a
proper session-start hook (see issue #487 cross-files).

### Category 3 — `ai-memory wrap` (PR-6)

PR-6 of issue #487 ships `ai-memory wrap <agent>`: a built-in
cross-platform Rust subcommand that replaces the per-recipe bash and
PowerShell wrappers earlier PRs shipped. The same binary runs on
macOS / Linux / Windows / Docker / Kubernetes — no shell required.

`ai-memory wrap`:

1. Calls `ai-memory boot` in-process (no subprocess).
2. Builds a system message of the form
   `<preamble>\n\n<boot output>`.
3. Spawns the named agent CLI with the system message delivered via
   the strategy chosen by `default_strategy(<agent>)`:

   | Agent | Strategy | Argv shape |
   |---|---|---|
   | `codex` / `codex-cli` | `SystemFlag` | `codex --system "<msg>" <args>` |
   | `gemini` | `SystemFlag` | `gemini --system "<msg>" <args>` |
   | `aider` | `MessageFile` | `aider --message-file <tempfile> <args>` |
   | `ollama` | `SystemEnv` | `OLLAMA_SYSTEM=<msg> ollama <args>` |
   | (anything else) | `SystemFlag` (`--system`) | fall-through default |

4. Propagates the agent's exit code.

Override the strategy with `--system-flag <flag>`, `--system-env <name>`,
or `--message-file-flag <flag>` if your agent uses a different
contract. See `ai-memory wrap --help` for the full surface.

The category-3 recipes ([`codex-cli.md`](codex-cli.md),
[`claude-agent-sdk.md`](claude-agent-sdk.md),
[`openai-apps-sdk.md`](openai-apps-sdk.md),
[`grok-and-xai.md`](grok-and-xai.md),
[`local-models.md`](local-models.md)) all link to `ai-memory wrap` for
the launcher case and keep the SDK code patterns for in-process
integrations.

## Per-agent recipes

The **Installer** column tracks `ai-memory install <agent>` support
(issue #487 PR-2). `yes` means a one-line `ai-memory install <agent>
--apply` writes the recipe's MCP / hook config block directly. Where
the column reads `yes (--config)`, the agent's canonical config path
isn't auto-discoverable yet — pass `--config <path>` explicitly. `no`
means the agent isn't yet a target for the installer (PR-2 follow-up
will extend); use the manual recipe in the meantime. `n/a` is
programmatic SDK / API code that the installer cannot wire — see the
recipe's snippets and `ai-memory wrap <agent>` (PR-6).

| File | Agent | Category | Installer | Status |
|---|---|---|---|---|
| [`claude-code.md`](claude-code.md) | Claude Code (CLI, Mac/Win desktop, IDE) | 1 (hook) | yes | reference recipe |
| [`cursor.md`](cursor.md) | Cursor | 2 (MCP + rules) | yes | recipe |
| [`cline.md`](cline.md) | Cline (VS Code extension) | 2 (MCP + custom instructions) | yes (--config) | recipe |
| [`roo-code.md`](roo-code.md) | Roo Code (Cline fork) | 2 (MCP + custom instructions) | no (PR-2 follow-up) | recipe |
| [`continue.md`](continue.md) | Continue (VS Code / JetBrains) | 2 (MCP + systemMessage) | yes | recipe |
| [`windsurf.md`](windsurf.md) | Windsurf (Codeium) | 2 (MCP + rules) | yes | recipe |
| [`zed.md`](zed.md) | Zed assistant | 2 (MCP + assistant directive) | no (PR-2 follow-up) | recipe |
| [`goose.md`](goose.md) | Block Goose | 2 (MCP + system instructions) | no (PR-2 follow-up) | recipe |
| [`openclaw.md`](openclaw.md) | OpenClaw CLI | 2 (MCP + system message) | yes (--config) | recipe |
| [`codex-cli.md`](codex-cli.md) | OpenAI Codex CLI | 3 (programmatic) | n/a (programmatic) | recipe |
| [`gemini.md`](gemini.md) | Google Gemini CLI / Gemini Code Assist | 3 (programmatic) | n/a (programmatic) | recipe |
| [`aider.md`](aider.md) | Aider | 3 (programmatic via `--message-file`) | n/a (programmatic) | recipe |
| [`cody.md`](cody.md) | Sourcegraph Cody | 3 (programmatic) | n/a (programmatic) | recipe |
| [`claude-agent-sdk.md`](claude-agent-sdk.md) | Claude Agent SDK | 3 (programmatic) | n/a (programmatic) | recipe (TS + Python) |
| [`openai-apps-sdk.md`](openai-apps-sdk.md) | OpenAI Apps SDK / Assistants / Responses | 3 (programmatic) | n/a (programmatic) | recipe |
| [`grok-and-xai.md`](grok-and-xai.md) | xAI Grok | 3 (programmatic) | n/a (programmatic) | recipe |
| [`local-models.md`](local-models.md) | Hermes, Llama, Mistral, etc. via LM Studio / Ollama / vLLM | 3 (programmatic) | n/a (programmatic) | recipe |
| [`platforms.md`](platforms.md) | macOS / Linux / Windows / WSL / Docker / Kubernetes / ARM Linux / commercial Unix / embedded Linux / BSD platform notes | n/a | n/a | reference |
| [`global-claude-md-template.md`](global-claude-md-template.md) | `~/.claude/CLAUDE.md` belt-and-suspenders snippet | 1 fallback | n/a | reference |

## Failure modes (any recipe)

- DB unreachable: pass `--quiet` to the boot call. Hook/wrapper exits 0,
  agent starts with no extra context (graceful degrade, no hang).
- Wrong namespace: `auto_namespace` falls back to cwd basename → "global".
  If still empty, boot returns the most-recently-accessed `tier=long`
  memories globally so a greenfield checkout still has cross-project
  context.
- Hook output too large: `--budget-tokens` (default 4096) clamps the row
  count cheaply (cumulative chars / 4 ≈ tokens).
- Platform mismatch: a recipe written for `bash` doesn't run on native
  Windows, embedded BusyBox `ash`, or inside a Kubernetes sidecar with
  no shell. See
  [`platforms.md`](platforms.md) for per-platform notes —
  including the [Kubernetes HTTP boot equivalent](platforms.md#boot-hook-in-kubernetes)
  for clusters where stdio recipes don't apply, and
  [ARM Linux / embedded](platforms.md#arm-linux-raspberry-pi-aws-graviton-others)
  resource budgets for low-memory devices.
- CI gap: not every supported platform is in the GitHub Actions matrix.
  See the [Lifetime test matrix](platforms.md#lifetime-test-matrix-pr-3)
  in `platforms.md` for what CI actually exercises vs. what's
  documented best-effort.

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

Category 2 agents (Cursor, Cline, Roo Code, Continue, Windsurf, Zed,
Goose, OpenClaw) all need native session-start hooks to reach 100%
remediation. Cross-files tracking those upstream requests live in
#487's comments.

The MCP spec proposal at
`modelcontextprotocol/specification` for a `session/initialize` server
callback is the universal architectural fix. Once accepted, it closes
category 2 entirely without per-host work.
