# ai-memory v0.6.4 — `quiet-tools`

**Tagged:** 2026-05-08
**Theme:** cross-harness token economics + AI NHI capability-discovery protocol phase 1
**One-line summary:** ai-memory v0.6.4 ships 5 tools by default, not 43. Saves ~4,700 input tokens per request on Codex / Grok / Gemini / Claude-Desktop. Run `ai-memory mcp --profile full` to keep the v0.6.3 behavior.

---

## Highlights

### 🪶 5-tool default surface (was 43)

The MCP server now ships with a **5-tool default surface** plus the always-on `memory_capabilities` bootstrap = 6 visible tools. The other 38 tools remain reachable via:

- `--profile graph|admin|power|full` for static profile selection
- `memory_capabilities --include-schema family=<name>` for runtime expansion (the canonical NHI discovery dance)
- Custom comma-list (`--profile core,graph,archive`) for hand-tuned surfaces

Eager-loading harnesses see a **76.4% reduction** in tool-schema prefix cost — measured against `cl100k_base`, the BPE Claude / GPT use for input accounting (6,198 → 1,465 tokens, 4,733 saved per request).

### 🔬 Truthful measurement

Earlier RFC drafts claimed ~25,800 tokens / 87% reduction, derived from MiniLM (a sentence-embedder vocabulary that systematically over-counts JSON by ~4×). The shipped numbers are the honest `cl100k_base` measurement. See `benchmarks/v0.6.4-cross-harness.md` for methodology + per-family rollup.

### 🛠️ `ai-memory doctor --tokens`

New observability surface. Reports per-family, per-profile, and per-tool token cost. Used by ops to audit before-and-after impact and CI to enforce the regression budget.

```bash
ai-memory doctor --tokens                # human table
ai-memory doctor --tokens --json         # structured output
ai-memory doctor --tokens --raw-table    # per-tool dump
ai-memory doctor --tokens --profile full # hypothetical profile
```

### 🔌 `memory_capabilities` runtime discovery

`memory_capabilities` is always loaded (regardless of profile) and now accepts:

- No params → v2 capabilities document + new `families` block (taxonomy + `loaded` flags)
- `family=<name>` → tool-name list for that family
- `family=<name>` + `include_schema=true` → full MCP-style tool definitions inline (callable by hosts that support runtime registration)

### 🔒 NHI guardrails phase 1

Opt-in per-agent capability allowlist via `[mcp.allowlist]` in `config.toml`. Default disabled (Tier-1 single-process semantics — backward compat for existing deployments). Every `memory_capabilities --include-schema` call (grant + deny) is recorded in the new `audit_log` SQLite table (schema v20).

### 📦 SDK `requireProfile`

Both TypeScript and Python SDKs export a `requireProfile(client, "graph")` helper that fails fast with a structured `ProfileNotLoaded` error if the daemon hasn't loaded all the families the agent needs. Pre-v0.6.4 daemons get a permissive warn-and-continue fallback.

### 🪝 Per-harness install

`ai-memory install` now supports `claude-desktop`, `codex`, `grok-cli`, `gemini-cli` in addition to the existing `claude-code`, `openclaw`, `cursor`, `cline`, `continue`, `windsurf`. All write the canonical `mcpServers.ai-memory.{command,args,env}` JSON shape with `["mcp", "--profile", "core"]` baked in for self-documenting clarity.

---

## Breaking changes

**Default profile flipped from `full` to `core`.** Power users who depend on tools outside the 5 core (`store/recall/list/get/search`) must opt up via `--profile <name>` / `AI_MEMORY_PROFILE=<name>` / `[mcp].profile = "<name>"`. Full migration walkthrough in `docs/MIGRATION_v0.6.4.md`.

To preserve v0.6.3 behavior 1:1:

```bash
ai-memory mcp --profile full
```

---

## Fixed

### G9 HTTP webhook parity (#526, source-anchored fold-in)

v0.6.3.1 P5 wired four lifecycle webhooks (`memory_delete`, `memory_promote`, `memory_link_created`, `memory_consolidated`) into the **MCP path** but left the HTTP handlers silent. Source review on 2026-05-04 found `grep "dispatch_event" src/handlers.rs` returned zero matches. v0.6.4-017 closes the gap symmetrically: HTTP DELETE / promote / link / consolidate now fire the same events with the same payloads. New integration tests in `tests/webhook_http_parity.rs` pin the contract.

---

## Schema migration

**v19 → v20** — adds `audit_log` table for capability-expansion observability. Idempotent (`CREATE TABLE IF NOT EXISTS`). Run by the binary on first startup; no manual action needed. Existing v18/v19 DBs upgrade cleanly.

`cli::boot::MAX_SUPPORTED_SCHEMA` bumped to 20 so the boot manifest reports OK status against v0.6.4 DBs.

---

## Substrate test count

1,955 lib tests pass (up from 1,886 in v0.6.3.1 baseline). +69 new tests across the v0.6.4 issue surface:

- `sizes::tests::*` (6) — token cost table + CI gate
- `profile::tests::*` (28) — Profile/Family parsing, comma-list edges, tool-name → family mapping
- `config::tests::effective_profile_*` (4) — CLI/env/config resolution
- `config::tests::allowlist_*` (8) — per-agent allowlist precedence
- `cli::doctor::tests::run_tokens_*` (4) — doctor --tokens human + JSON + raw-table
- `mcp::tests::tool_definitions_for_profile_*` (4) — family filter
- `mcp::tests::handle_capabilities_family_*` + `families_overview` (5) — capabilities extension
- `db::tests::audit_log_*` (4) — schema v20 + record/list semantics
- `cli::install::tests::*_apply_writes_mcp_standard_with_profile_core` (5) — per-harness install
- `tests/webhook_http_parity.rs` (4) — G9 HTTP parity integration tests
- SDK: 13 jest (TS) + 14 pytest (Py) for `requireProfile`

---

## Cert verdict

**v0.6.4 cell: CERT.** All 8 new scenarios S25–S32 GREEN. Backward-compat preserved (43 tools under `--profile full`). 1,955 substrate tests green throughout sprint. Full evidence: [`alphaonedev/ai-memory-test-hub`](https://github.com/alphaonedev/ai-memory-test-hub/blob/main/campaigns/v0.6.4.md).

---

## Acknowledgments

Boris Cherny's published 90-day instrumentation data (May 2026) quantifying that 73% of Claude Code tokens go to nine waste patterns — and the observation that ai-memory was a top contributor to Pattern 6 ("just-in-case" tool definitions) on naïve-loading harnesses — was the impetus for this release. v0.6.4 is the response.

---

## Upgrade path

```bash
# Homebrew
brew update && brew upgrade ai-memory

# Cargo
cargo install ai-memory --force

# npm
npm install -g @alphaone/ai-memory@0.6.4

# pip
pip install --upgrade ai-memory==0.6.4
```

After upgrade:

1. Run `ai-memory doctor --tokens` to see your new surface cost.
2. If you hit a `tool_not_found` error from your agent, the error message names the profile/family you need to add. Pick one of the three opt-up paths in `docs/MIGRATION_v0.6.4.md`.
3. Re-run `ai-memory install --harness <name>` to refresh your harness config with the v0.6.4 defaults baked in.
