# ai-memory v0.6.4 — `quiet-tools`

> Persistent memory for any AI. Self-hosted. MCP-native. Now **76% lighter** on the wire — without losing a single tool.

---

## What's actually new in v0.6.4 (read this first — the framing matters)

The headline number is **76.4% reduction in tool-schema prefix tokens** on every eager-loading harness (Codex CLI / Grok CLI / Gemini CLI / Claude Desktop). What's NOT changing is the AI's **capability surface** — every one of the 43 tools shipped in v0.6.3 is still in the server, still callable, still functional.

What changed: how the tool list is *advertised* on session start.

| | v0.6.3 | v0.6.4 |
|---|---|---|
| Tools the server actually runs | 43 | **43 (unchanged)** |
| Tools advertised in initial `tools/list` | 43 | 5 + always-on `memory_capabilities` |
| Tokens prepaid per request prefix | ~6,200 | ~1,500 (-4,700) |
| AI can still call `memory_kg_query`, `memory_consolidate`, etc.? | Yes | **Yes** — via runtime discovery OR `--profile <name>` |

Every tool the AI could reach in v0.6.3 is still reachable in v0.6.4. The change is *when* the AI sees the schemas, not *whether* it can call them.

---

## ⚠️ Breaking change — default tool advertising surface

Three opt-up paths if you want the v0.6.3 default behavior back:

```bash
# Option 1 — CLI flag
ai-memory mcp --profile full

# Option 2 — env var
export AI_MEMORY_PROFILE=full

# Option 3 — config.toml
[mcp]
profile = "full"
```

Resolution order: CLI > env > config > `core` (the new default). Full migration walkthrough at [`docs/MIGRATION_v0.6.4.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/MIGRATION_v0.6.4.md).

---

## Distribution channels — 5/5 published from this tag

| Channel | Install |
|---|---|
| **GitHub Release** | this page (11 binary assets + SHA256SUMS) |
| **Homebrew tap** | `brew install alphaonedev/tap/ai-memory` |
| **ghcr.io** | `docker pull ghcr.io/alphaonedev/ai-memory:0.6.4` |
| **Fedora COPR** | `sudo dnf copr enable alpha-one-ai/ai-memory && sudo dnf install ai-memory` |
| **crates.io** | `cargo install ai-memory --version 0.6.4` |

All five publishes auto-fired on the same `v0.6.4` tag through `.github/workflows/ci.yml`. SHA256 checksums of binary tarballs match this release page exactly.

---

## TL;DR by audience

### 👤 If you're a non-technical user

> **What changed:** every time your AI assistant (Claude / ChatGPT / Cursor / Codex CLI / Grok CLI / Gemini CLI / etc.) reaches for ai-memory, it used to spend ~6,200 input tokens just *describing the available memory tools* before it could even read your message. v0.6.4 cuts that to ~1,500. **Your AI still does everything it did before** — it just doesn't pre-pay for tools it doesn't need every turn.

> **What you need to do:** nothing. Run `brew upgrade ai-memory` (or `cargo install ai-memory --force`) and your existing setup keeps working. If you've been seeing slow first-message responses on Codex / Grok / Gemini, they should feel snappier now.

> **What you'll notice:** your AI bill on those harnesses drops automatically. Memory recall, store, and search all work exactly the same way they did before.

```bash
brew upgrade ai-memory && ai-memory doctor --tokens
```

That second command shows you exactly how much you're saving.

### 🏢 If you're a C-level decision maker

> **What v0.6.4 closes:** the *token-tax line item* in your AI subscription cost stack. Boris Cherny's published 90-day instrumentation data quantified that 73% of Claude Code tokens go to nine waste patterns. ai-memory was the #1 contributor to **Pattern 6** ("just-in-case tool definitions") on every eager-loading harness except Claude Code's own deferred-tools path. v0.6.4 fixes that one waste pattern in one release.

> **What's also new:**
> - **NHI guardrails phase 1** — opt-in per-agent capability allowlist (`[mcp.allowlist]` in `config.toml`), capability-expansion audit log (schema v20), deterministic discovery protocol that lets AI agents opt into restricted tool families at runtime
> - **Cross-harness coverage** — built-in installers for `claude-code`, `claude-desktop`, `codex`, `grok-cli`, `gemini-cli`, `openclaw`, `cursor`, `cline`, `continue`, `windsurf`. `ai-memory install <harness> --apply` writes the right config for every one of them
> - **Empirical validation** — companion repo [`ai-memory-discovery-gate`](https://github.com/alphaonedev/ai-memory-discovery-gate) runs a 4-tier test matrix (T1 awareness / T2 reactive / T3 proactive / T4 mesh recovery) against real LLMs to prove agents actually use the discovery mechanisms this release ships with. **First baseline run with xAI Grok 4.3 against the v0.6.4 release binary: 100% pass rate across all four tiers (6/6 cells)** — see [public verdict page](https://alphaonedev.github.io/ai-memory-discovery-gate/)

> **Cost math (concrete, measured):** at ~7,500 turns/year for a heavy single user on Sonnet 4.6 input pricing ($3/MTok), the prefix savings alone are ~$107/user/year on eager-loading harnesses. At fleet scale of 1,000 daily-active agent seats, that's ~$107K/year off the input-token line item, before any per-call latency improvements.

> **Backward compatibility:** zero data-migration risk. Existing v0.6.3.x SQLite DBs auto-migrate v18/v19 → v20 on first open (verified against a real production DB with 228 memories + 51 links — no row loss). The `audit_log` table is added; nothing else changes.

### 🛠️ If you're a maintainer / subject-matter-expert engineer

> **What landed in 18 issues:**
>
> - `--profile {core,graph,admin,power,full,custom}` flag (CLI + `AI_MEMORY_PROFILE` env + `[mcp].profile` config) with deterministic resolution order
> - Family-scoped `tools/list` filter at `mcp.rs::tool_definitions_for_profile`. `core` advertises 5 tools + always-on `memory_capabilities` bootstrap. Other 38 tools remain registered in the server; they're filtered from the initial advertising surface, not removed
> - `tools/call` for an unloaded tool returns JSON-RPC `-32601` with an actionable diagnostic naming the family + suggesting `--profile <name>` AND `memory_capabilities --include-schema family=<f>` recovery paths
> - `memory_capabilities` extended: optional `family=<name>` parameter returns just that family's tools; optional `include_schema=true` returns full MCP-style tool definitions inline for runtime registration
> - `[mcp.allowlist]` config table maps `agent_id` patterns to allowed family sets. Pattern resolution: exact > longest-prefix > `*` wildcard. Default disabled (Tier-1 single-process semantics — backward compat for existing deployments)
> - Schema migration **v19 → v20** adds `audit_log` table for capability-expansion observability. Idempotent (`CREATE TABLE IF NOT EXISTS` + 3 indexes). `cli::boot::MAX_SUPPORTED_SCHEMA` bumped to 20
> - HTTP webhook parity for the four lifecycle event types added in v0.6.3.1 P5 (`memory_delete`, `memory_promote`, `memory_link_created`, `memory_consolidated`). v0.6.3.1 wired them into the MCP path only; the HTTP handlers in `src/handlers.rs` were silent. **v0.6.4-017 closes that gap symmetrically** with 4 new integration tests in `tests/webhook_http_parity.rs`. This was a real carry-forward bug surfaced by source-code review during the sprint, not a new feature
> - SDK `requireProfile` helper in both TypeScript (`@alphaone/ai-memory`) and Python (`ai-memory`). Throws `ProfileNotLoaded` with a structured `hint` field if the daemon doesn't load every family the requested profile needs. Pre-v0.6.4 daemons get a permissive warn-and-continue fallback so SDK upgrades don't break old servers
> - Per-harness install (`ai-memory install --harness <name>`) for `claude-desktop` / `codex` / `grok-cli` / `gemini-cli` — writes the canonical `mcpServers.<name>.{command,args,env}` shape with `["mcp", "--profile", "core"]` baked in
> - `ai-memory doctor --tokens` reports per-family + per-profile token cost with `--json` and `--raw-table` modes. Output is deterministic — does not require a running daemon
> - CI gate `.github/workflows/token-budget.yml` enforces the per-tool ceiling (no individual tool may exceed 1,500 `cl100k_base` tokens) and the full-profile honest range (5K-8K tokens). Two-pronged regression protection: a single tool ballooning fires red, AND the full surface drifting fires red

> **Test surface — every gate green on the merged commit (`9494c72`):**
>
> ```
> cargo fmt --check                                      ✓
> cargo clippy -- -D warnings -D clippy::all -D pedantic ✓
> cargo test --lib                                  1,960 ✓
> cargo test --test integration                       211 ✓
> cargo test --test mcp_integration                    16 ✓
> cargo test --test webhook_http_parity                 4 ✓ (new)
> cargo test --test recipe_contract                    16 ✓
> + ~15 other binary test targets                  ~150+ ✓
> cargo audit                            (1 allowed warn) ✓
> ```
>
> Total full-surface ~2,400 tests. **Code coverage on net-new modules**: sizes.rs **100.00%**, profile.rs **99.50%**, cli/audit.rs **97.58%**, cli/doctor.rs **97.05%**, cli/install.rs 92.26%, handlers.rs 92.56% — all above the 92% project bar.

> **Truthfulness correction we made on ourselves before shipping:** the v0.6.4 RFC drafts originally claimed "~25,800 tokens / 87% reduction." Those numbers were measured against MiniLM (a sentence-embedder vocabulary that systematically over-counts JSON by ~4× vs. `cl100k_base`, the BPE Claude/GPT actually use for input accounting). Real measurement: **6,198 → 1,465 / 76.4%**. We corrected the public claim before the release; both numbers are documented in the CHANGELOG with the methodology gap explained explicitly. Methodology lives at [`benchmarks/v0.6.4-cross-harness.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/benchmarks/v0.6.4-cross-harness.md).

> **Empirical validation of the discovery dance:** the [`ai-memory-discovery-gate`](https://github.com/alphaonedev/ai-memory-discovery-gate) repo runs a 4-tier matrix against real xAI Grok 4.3 driving an OpenClaw-shape harness against the v0.6.4 release binary, with the v0.6.3.1 baseline DB fixture restored per cell.
>
> - **T1 awareness** (≥90% bar): does the agent know unloaded tools exist? — **PASS, 100%**
> - **T2 reactive recovery** (≥80% bar): does the agent recover from `tool_not_found`? — **PASS, 100%**
> - **T3 proactive expansion** (≥50% bar): does the agent reach for `--include-schema` before failing? — **PASS, 100%**
> - **T4 mesh recovery** (≥66% bar): can the mesh route around misconfigured peers? — **PASS, 100% (3/3 sub-cells)**
>
> Per-cell evidence (full LLM transcripts, MCP wire logs, verdict JSON) at [https://alphaonedev.github.io/ai-memory-discovery-gate/](https://alphaonedev.github.io/ai-memory-discovery-gate/).

---

## Install

### macOS / Linux

```bash
brew install alphaonedev/tap/ai-memory
ai-memory --version
```

Or via Cargo (any Rust 1.88+ host):

```bash
cargo install ai-memory --version 0.6.4
```

### Linux — Fedora / RHEL / Rocky

```bash
sudo dnf copr enable alpha-one-ai/ai-memory
sudo dnf install ai-memory
```

### Linux — Debian / Ubuntu

Download the `.deb` artifact from this page, then:

```bash
sudo dpkg -i ai-memory_0.6.4_amd64.deb   # or _arm64.deb
```

### Windows

Download `ai-memory.exe` or `ai-memory-x86_64-pc-windows-msvc.zip` from this page. Add to `%PATH%`.

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
ai-memory install codex --apply --config <path-to-codex-mcp-config>
ai-memory install grok-cli --apply --config <path-to-grok-mcp-config>
ai-memory install gemini-cli --apply --config <path-to-gemini-mcp-config>
```

Each writes the canonical `mcpServers.ai-memory.{command, args, env}` JSON shape with `["mcp", "--profile", "core"]` baked in. Drop the [v0.6.4 system-prompt snippet](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/integrations/v0.6.4-system-prompt-snippet.md) into your harness's system prompt to prime the AI with the discovery dance convention — the discovery gate confirmed 100% T1 awareness with that snippet present.

---

## What you can verify in 60 seconds

```bash
ai-memory --version                          # 0.6.4
ai-memory doctor --tokens                    # 1,465 / 6,198 / 76.4% saved
ai-memory mcp --profile full --version       # confirms full-profile opt-out works
```

For the brave (probes the actual MCP surface over stdio):

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | ai-memory mcp --profile core --tier keyword 2>/dev/null \
  | jq '.result.tools | length'
# -> 6  (5 core + always-on memory_capabilities)

echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | ai-memory mcp --profile full --tier keyword 2>/dev/null \
  | jq '.result.tools | length'
# -> 43  (full v0.6.3 surface, 1:1)
```

---

## Migration notes (zero-downtime)

**Schema migration v19 → v20** runs automatically on first open. Adds the `audit_log` table for capability-expansion observability. Idempotent (`CREATE TABLE IF NOT EXISTS`); preserves every existing row. We tested it against a real populated v0.6.3.1 deployment with 228 memories + 51 memory_links — zero data loss, all rows queryable post-migration, `audit_log` created with 3 indexes.

**Default profile flip** — see "Breaking change" section above. Three opt-out paths.

**Token-cost claim** — the released numbers (**6,198 → 1,465 tokens / 76.4%**) are honest `cl100k_base` measurement. The earlier "~25,800 / 87%" RFC claim was a methodology error (MiniLM tokenizer over-counts JSON ~4× vs. cl100k_base). Both numbers are documented in the CHANGELOG with the discrepancy explained explicitly. The savings *percentage* and *user-facing impact* are real and material; the absolute number is just smaller than the early RFC framing.

---

## Documentation index

- [`docs/MIGRATION_v0.6.4.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/MIGRATION_v0.6.4.md) — operator's three-path walkthrough
- [`docs/v0.6.4/V0.6.4-EPIC.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/v0.6.4/V0.6.4-EPIC.md) — sprint framework + source-anchored ground truth
- [`docs/v0.6.4/rfc-default-tool-surface-collapse.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/v0.6.4/rfc-default-tool-surface-collapse.md) — design RFC
- [`docs/integrations/v0.6.4-system-prompt-snippet.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/integrations/v0.6.4-system-prompt-snippet.md) — discovery-aware NHI bootstrap (drop into any harness's system prompt)
- [`benchmarks/v0.6.4-cross-harness.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/benchmarks/v0.6.4-cross-harness.md) — token-cost measurement methodology
- [`CHANGELOG.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/CHANGELOG.md) — full v0.6.4 entry

### Cert verdict

**v0.6.4 cell: CERT GREEN.** All 8 new A2A scenarios S25–S32 pass live; v0.6.3 backward-compat surface preserved 1:1 under `--profile full`; 1,960 substrate tests green throughout the sprint. Full evidence: [`alphaonedev/ai-memory-test-hub` campaigns/v0.6.4.md](https://github.com/alphaonedev/ai-memory-test-hub/blob/main/campaigns/v0.6.4.md).

### NHI Discovery Gate

[`alphaonedev/ai-memory-discovery-gate`](https://github.com/alphaonedev/ai-memory-discovery-gate) — empirical 4-tier acceptance matrix against real xAI Grok 4.3 + OpenClaw. **First certifying baseline run: 6/6 cells PASS, 100% per tier, GATE GREEN.** Public Pages site: [https://alphaonedev.github.io/ai-memory-discovery-gate/](https://alphaonedev.github.io/ai-memory-discovery-gate/).

---

## All 18 issues closed

Track A — Mechanism:
- [#521](https://github.com/alphaonedev/ai-memory-mcp/issues/521) v0.6.4-001 `--profile` flag (CLI + env + config resolution)
- [#522](https://github.com/alphaonedev/ai-memory-mcp/issues/522) v0.6.4-002 family-scoped tool registration filter
- [#523](https://github.com/alphaonedev/ai-memory-mcp/issues/523) v0.6.4-003 `core` default flip + backward-compat note

Track B — Observability:
- [#524](https://github.com/alphaonedev/ai-memory-mcp/issues/524) v0.6.4-004 `ai-memory doctor --tokens`
- [#525](https://github.com/alphaonedev/ai-memory-mcp/issues/525) v0.6.4-005 static schema-size table (build-time)
- [#526](https://github.com/alphaonedev/ai-memory-mcp/issues/526) **v0.6.4-017 G9 HTTP webhook parity** (source-anchored fold-in surfaced 2026-05-04 — closes a v0.6.3.1 charter promise that only shipped on the MCP path)

Track C — Discovery:
- [#527](https://github.com/alphaonedev/ai-memory-mcp/issues/527) v0.6.4-006 `memory_capabilities` family enum + `--include-schema`
- [#528](https://github.com/alphaonedev/ai-memory-mcp/issues/528) v0.6.4-007 SDK `requireProfile` (TypeScript + Python)

Track D — NHI guardrails phase 1:
- [#529](https://github.com/alphaonedev/ai-memory-mcp/issues/529) v0.6.4-008 per-agent capability allowlist
- [#530](https://github.com/alphaonedev/ai-memory-mcp/issues/530) v0.6.4-009 capability-expansion audit log + schema v20

Track E — Cross-harness install:
- [#531](https://github.com/alphaonedev/ai-memory-mcp/issues/531) v0.6.4-010 per-harness install profiles for 4 new MCP harnesses

Track F — Cert + benchmarks:
- [#532](https://github.com/alphaonedev/ai-memory-mcp/issues/532) v0.6.4-011 cross-harness token-cost benchmark + CI gate
- [#533](https://github.com/alphaonedev/ai-memory-mcp/issues/533) v0.6.4-012 A2A cert scenarios S25–S32
- [#534](https://github.com/alphaonedev/ai-memory-mcp/issues/534) v0.6.4-013 backward-compat verification (`--profile full` 1:1)

Track G — Docs + release:
- [#535](https://github.com/alphaonedev/ai-memory-mcp/issues/535) v0.6.4-014 README + ADMIN_GUIDE updates
- [#536](https://github.com/alphaonedev/ai-memory-mcp/issues/536) v0.6.4-015 migration guide + release notes
- [#537](https://github.com/alphaonedev/ai-memory-mcp/issues/537) v0.6.4-016 CHANGELOG + version bumps + tag + CI release

Stretch (added late-sprint after source review):
- [#539](https://github.com/alphaonedev/ai-memory-mcp/issues/539) v0.6.4-018 NHI Discovery Gate (`alphaonedev/ai-memory-discovery-gate`)

---

## Credits

This sprint was executed end-to-end by Claude Opus 4.7 (1M context) acting as `ai:claude-opus-4-7@v0.6.4-lead-coordinator`, under continuous human direction. Every commit on `feat/v0.6.4` is SSH-signed under the `alphaonedev` GitHub identity.

Boris Cherny's published 90-day instrumentation data (May 2026) — quantifying that 73% of Claude Code tokens go to nine waste patterns and that ai-memory was a top contributor to Pattern 6 ("just-in-case" tool definitions) on naïve-loading harnesses — was the impetus for this release. v0.6.4 is the response.

Per-tier methodology and per-cell empirical evidence at [`alphaonedev.github.io/ai-memory-discovery-gate`](https://alphaonedev.github.io/ai-memory-discovery-gate/).

🤖 Built with [Claude Code](https://claude.com/claude-code)
