# ai-memory v0.6.4 — `quiet-tools`

> Persistent memory for any AI. Self-hosted. MCP-native. Now **76% lighter** on the wire.

---

## ⚠️ Breaking change — default tool surface

The MCP server now ships with a **5-tool default surface** (`memory_store`, `memory_recall`, `memory_list`, `memory_get`, `memory_search`) plus the always-on `memory_capabilities` bootstrap. The other 38 tools are gated behind `--profile graph|admin|power|full` or runtime expansion via `memory_capabilities --include-schema family=<name>`.

**To preserve v0.6.3 behavior 1:1:**

```bash
ai-memory mcp --profile full
```

Or via env / config:

```bash
export AI_MEMORY_PROFILE=full
# or in ~/.config/ai-memory/config.toml:
[mcp]
profile = "full"
```

Resolution order: CLI flag > `AI_MEMORY_PROFILE` env > `[mcp].profile` config > `core` default.

Full migration walkthrough: [`docs/MIGRATION_v0.6.4.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/MIGRATION_v0.6.4.md).

---

## Distribution channels (5/5 — auto-published by CI on tag push)

| Channel | Install |
|---|---|
| **GitHub Release** | this page (11 binary assets + SHA256SUMS) |
| **Homebrew tap** | `brew install alphaonedev/tap/ai-memory` |
| **ghcr.io** | `docker pull ghcr.io/alphaonedev/ai-memory:0.6.4` |
| **Fedora COPR** | `sudo dnf copr enable alpha-one-ai/ai-memory && sudo dnf install ai-memory` |
| **crates.io** | `cargo install ai-memory --version 0.6.4` |

Every channel publishes from the same `v0.6.4` tag through `.github/workflows/ci.yml`. SHA256 checksums of binary tarballs match the GitHub Release page exactly.

---

## TL;DR by audience

### 👤 If you're a non-technical user

You probably already use ai-memory — that thing that makes Claude / Cursor / your AI remember what you talked about yesterday. **v0.6.4 makes it 76% lighter on every request.**

Every time your AI sends a message, it sends a list of "tools it knows about" along with it. Up through v0.6.3, ai-memory shipped 43 tools by default. v0.6.4 ships 5. The other 38 are still there — your AI can ask for them when it needs them — but for the 95% of conversations that just need "remember this" and "what did we say about X", the default is now lean.

What changes for you: nothing. Run `brew upgrade ai-memory` and your existing setup keeps working. The token bill on Codex / Grok / Gemini / Claude Desktop drops automatically.

**One command to upgrade:**

```bash
brew upgrade ai-memory && ai-memory doctor --tokens
```

The second command shows you exactly how much you're saving.

### 🏢 If you're a C-level decision maker

ai-memory v0.6.4 closes the **token-tax line item** in your AI subscription cost stack. Every eager-loading agent harness (OpenAI Codex CLI, xAI Grok CLI, Google Gemini CLI, Claude Desktop) was paying ~6,200 tokens of tool-schema prefix per request just for ai-memory. v0.6.4 cuts that to ~1,500. Across an org running ~10K agent-turns a day, the savings on input pricing is material.

What's also new in v0.6.4:

- **NHI guardrails phase 1** — opt-in per-agent capability allowlist (`[mcp.allowlist]` in `config.toml`), capability-expansion audit log (schema v20), and a deterministic discovery dance for AI agents to opt into restricted tool families at runtime
- **Cross-harness coverage** — built-in installers for `claude-code`, `claude-desktop`, `codex`, `grok-cli`, `gemini-cli`, `openclaw`, `cursor`, `cline`, `continue`, `windsurf`. `ai-memory install <harness> --apply` writes the right config for every one.
- **Empirical validation gate** — companion repo [`ai-memory-discovery-gate`](https://github.com/alphaonedev/ai-memory-discovery-gate) runs a 4-tier test matrix (T1 awareness / T2 reactive / T3 proactive / T4 mesh recovery) against real LLMs (Claude / GPT / Grok / Gemini) to prove agents actually use the discovery mechanism this release ships with. First baseline run included in this release window.

### 🛠️ If you're a maintainer / operator

```bash
brew upgrade ai-memory
ai-memory doctor --tokens
```

That gives you the cost breakdown. Then look at `docs/MIGRATION_v0.6.4.md` for the three opt-up paths (`--profile full`, narrower profile, runtime `memory_capabilities --include-schema`).

Schema migration **v19 → v20** runs on first start. Idempotent. Adds the `audit_log` table; preserves every existing memory and link. We tested it against a real production DB with 228 memories + 51 links — no data loss.

If you fan out this release to a multi-agent mesh, the new `[mcp.allowlist]` table lets you carve different family permissions per agent_id pattern. Default is wide open (Tier-1 single-process semantics); operators opt into the gate by writing the table.

### 🤖 If you're an AI agent reading this

You now run on the `core` profile by default. If you need a tool outside `[memory_store, memory_recall, memory_list, memory_get, memory_search]`:

1. Call `memory_capabilities` (always loaded) to see all 8 families with their `loaded` flags
2. If a family you need is not loaded, call `memory_capabilities` again with `family=<name> include_schema=true` to get the full schemas
3. If you call an unloaded tool directly, you'll get a `-32601` error with a hint naming the family and the `--profile` flag that would load it

The discovery dance is the canonical pattern. Don't silently give up. Don't fabricate. There's a system-prompt template at [`docs/integrations/v0.6.4-system-prompt-snippet.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/integrations/v0.6.4-system-prompt-snippet.md) you can ask the operator to pre-load.

---

## Install

### macOS / Linux

```bash
brew install alphaonedev/tap/ai-memory
ai-memory --version
```

Or via Cargo:

```bash
cargo install ai-memory --version 0.6.4
```

### Linux (Fedora / RHEL / Rocky)

```bash
sudo dnf copr enable alpha-one-ai/ai-memory
sudo dnf install ai-memory
```

### Linux (Debian / Ubuntu)

Download the `.deb` artifact from this release page, then:

```bash
sudo dpkg -i ai-memory_0.6.4_amd64.deb
```

(arm64 .deb also available)

### Windows

Download `ai-memory.exe` or the `.zip` from this release page. Add it to your `%PATH%`.

### Docker

```bash
docker pull ghcr.io/alphaonedev/ai-memory:0.6.4
docker run --rm -it -v $HOME/.ai-memory:/data ghcr.io/alphaonedev/ai-memory:0.6.4 mcp
```

---

## One-command setup for Claude Code

```bash
brew install alphaonedev/tap/ai-memory && ai-memory install claude-code --apply
```

For other harnesses:

```bash
ai-memory install claude-desktop --apply
ai-memory install codex --apply --config <path>           # codex config path varies
ai-memory install grok-cli --apply --config <path>        # grok config path varies
ai-memory install gemini-cli --apply --config <path>      # gemini config path varies
```

Each writes the canonical `mcpServers.ai-memory.{command, args, env}` JSON shape with `["mcp", "--profile", "core"]` baked in.

---

## What you can verify in 60 seconds

```bash
ai-memory --version                       # 0.6.4
ai-memory doctor --tokens                 # 1,465 / 6,198 / 76.4% saved
ai-memory mcp --profile core --version    # confirms core profile loads
ai-memory mcp --profile full --version    # confirms full opt-up works
```

For the brave:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | ai-memory mcp --profile core --tier keyword 2>/dev/null \
  | jq '.result.tools | length'
# -> 6  (5 core + always-on memory_capabilities)
```

---

## Migration notes

**Schema migration v19 → v20** runs automatically on first open. Adds the `audit_log` table for capability-expansion observability. Idempotent (`CREATE TABLE IF NOT EXISTS`); preserves every existing row.

Tested against a real populated v0.6.3.1 deployment (228 memories, 51 memory_links) — no data loss, all rows queryable post-migration.

**Default profile flip** — see the breaking-change section above. `--profile full` reproduces v0.6.3 behavior 1:1.

**Token-cost claim** — earlier RFC drafts said "~25,800 tokens / 87% reduction". Those numbers were measured against MiniLM (a sentence-embedder vocabulary that systematically over-counts JSON by ~4× vs. `cl100k_base`). The shipped numbers — **6,198 → 1,465 tokens / 76.4%** — are the honest `cl100k_base` measurement (the BPE Claude / GPT actually use for input accounting). Both numbers are material; we corrected our own claim before shipping.

---

## Documentation

- [`docs/MIGRATION_v0.6.4.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/MIGRATION_v0.6.4.md) — operator's walkthrough
- [`docs/v0.6.4/V0.6.4-EPIC.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/v0.6.4/V0.6.4-EPIC.md) — sprint framework + source-anchored ground truth
- [`docs/v0.6.4/rfc-default-tool-surface-collapse.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/v0.6.4/rfc-default-tool-surface-collapse.md) — design RFC
- [`docs/integrations/v0.6.4-system-prompt-snippet.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/integrations/v0.6.4-system-prompt-snippet.md) — discovery-aware NHI bootstrap
- [`benchmarks/v0.6.4-cross-harness.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/benchmarks/v0.6.4-cross-harness.md) — token-cost measurement methodology
- [`CHANGELOG.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/CHANGELOG.md) — full v0.6.4 entry

### Cert verdict

**v0.6.4 cell: CERT GREEN.** All 8 new scenarios S25–S32 pass live. Backward-compat preserved (43 tools under `--profile full`). 1,960 substrate tests green throughout sprint. Full evidence: [`alphaonedev/ai-memory-test-hub` campaigns/v0.6.4.md](https://github.com/alphaonedev/ai-memory-test-hub/blob/main/campaigns/v0.6.4.md).

### NHI Discovery Gate

New companion repo: [`alphaonedev/ai-memory-discovery-gate`](https://github.com/alphaonedev/ai-memory-discovery-gate) — empirical 4-tier acceptance matrix (T1 awareness / T2 reactive / T3 proactive / T4 mesh recovery) × 4 LLMs × 3 harnesses. First baseline run captured in this release window: harness pipeline ✅ GREEN. Real LLM-driven cells run during the post-tag soak window.

---

## All 18 issues closed

Track A — Mechanism:
- #521 v0.6.4-001 `--profile` flag (CLI + env + config resolution)
- #522 v0.6.4-002 family-scoped tool registration filter
- #523 v0.6.4-003 `core` default flip + backward-compat note

Track B — Observability:
- #524 v0.6.4-004 `ai-memory doctor --tokens`
- #525 v0.6.4-005 static schema-size table (build-time)
- **#526 v0.6.4-017 G9 HTTP webhook parity** (source-anchored fold-in surfaced 2026-05-04 — closes a v0.6.3.1 charter promise that only shipped on the MCP path)

Track C — Discovery:
- #527 v0.6.4-006 `memory_capabilities` family enum + `--include-schema`
- #528 v0.6.4-007 SDK `requireProfile` (TypeScript + Python)

Track D — NHI guardrails phase 1:
- #529 v0.6.4-008 per-agent capability allowlist
- #530 v0.6.4-009 capability-expansion audit log + schema v20

Track E — Cross-harness install:
- #531 v0.6.4-010 per-harness install profiles for 4 new MCP harnesses (claude-desktop, codex, grok-cli, gemini-cli)

Track F — Cert + benchmarks:
- #532 v0.6.4-011 cross-harness token-cost benchmark + CI gate
- #533 v0.6.4-012 A2A cert scenarios S25–S32
- #534 v0.6.4-013 backward-compat verification (`--profile full` 1:1)

Track G — Docs + release:
- #535 v0.6.4-014 README + ADMIN_GUIDE updates
- #536 v0.6.4-015 migration guide + release notes
- #537 v0.6.4-016 CHANGELOG + version bumps + tag + CI release flow

Stretch (added late-sprint):
- #539 v0.6.4-018 NHI Discovery Gate (`alphaonedev/ai-memory-discovery-gate`)

---

## Credits

This sprint was executed end-to-end by Claude Opus 4.7 (1M context) acting as `ai:claude-opus-4-7@v0.6.4-lead-coordinator`, under continuous human direction. 17 SSH-signed commits on `feat/v0.6.4` + 4 commits on the discovery-gate repo + 2 commits on the test-hub campaign.

Boris Cherny's published 90-day instrumentation data (May 2026) — quantifying that 73% of Claude Code tokens go to nine waste patterns and that ai-memory was a top contributor to Pattern 6 ("just-in-case" tool definitions) on naïve-loading harnesses — was the impetus for this release. v0.6.4 is the response.

Per-tier methodology and acceptance criteria are public at [`alphaonedev.github.io/ai-memory-discovery-gate`](https://alphaonedev.github.io/ai-memory-discovery-gate/).

🤖 Built with [Claude Code](https://claude.com/claude-code)
