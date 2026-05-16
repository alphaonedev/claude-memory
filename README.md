<p align="center">
  <img src="docs/ai-memory-logo.jpg" alt="ai-memory logo" width="200">
</p>

<h1 align="center">ai-memory&trade;</h1>
<p align="center"><em>universal AI memory</em></p>

[![CI](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/ci.yml)
[![Bench](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/bench.yml/badge.svg)](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/bench.yml)
[![Session-boot lifetime](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/session-boot-lifetime.yml/badge.svg)](https://github.com/alphaonedev/ai-memory-mcp/actions/workflows/session-boot-lifetime.yml)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![SQLite](https://img.shields.io/badge/sqlite-FTS5-003B57?logo=sqlite)](https://www.sqlite.org/)
[![Tests](https://img.shields.io/badge/tests-2%2C400%2B_%E2%80%A2_%E2%89%A592%25_cov-brightgreen)](https://alphaonedev.github.io/ai-memory-mcp/evidence.html)
[![Test Hub](https://img.shields.io/badge/test--hub-live_results-6ee7ff?logo=githubpages)](https://alphaonedev.github.io/ai-memory-test-hub/)
[![Discovery Gate](https://img.shields.io/badge/discovery--gate-6%2F6_PASS_%E2%80%A2_GATE_GREEN-2ea043?logo=githubpages)](https://alphaonedev.github.io/ai-memory-discovery-gate/)
[![v0.6.4 Cert](https://img.shields.io/badge/v0.6.4_cert-CERT_GREEN-2ea043?logo=githubpages)](https://github.com/alphaonedev/ai-memory-test-hub/blob/main/campaigns/v0.6.4.md)
[![MCP](https://img.shields.io/badge/MCP-7_default_%E2%80%A2_71_full-blueviolet)]()
[![Evidence v0.6.4](https://img.shields.io/badge/claims-frozen_v0.6.4-c8a2ff)](https://alphaonedev.github.io/ai-memory-mcp/evidence.html)
[![Evidence v0.7.0](https://img.shields.io/badge/claims-frozen_v0.7.0-7e57c2)](docs/v0.7.0/release-notes.md)
[![Crates.io Version](https://img.shields.io/crates/v/ai-memory)](https://crates.io/crates/ai-memory)
[![npm](https://img.shields.io/npm/v/@alphaone/ai-memory?label=npm&logo=npm)](https://www.npmjs.com/package/@alphaone/ai-memory)
[![PyPI](https://img.shields.io/pypi/v/ai-memory-mcp?label=pypi&logo=pypi&logoColor=white)](https://pypi.org/project/ai-memory-mcp/)

**ai-memory is a persistent memory system for AI assistants.** It works with **any AI that supports MCP** -- Claude, ChatGPT, Grok, Llama, and more. It stores what your AI learns in a local SQLite database, ranks memories by relevance when recalling, and auto-promotes important knowledge to permanent storage. Install it once, and every AI assistant you use remembers your architecture, your preferences, your corrections -- forever.

**v0.7.0 (`attested-cortex`)** rolls together the cortex-fluent legibility work with the full v0.7 trust + A2A scope from ROADMAP2 §7.3, **plus** (per operator directive 2026-05-09) the originally-v0.7.1 postgres+AGE first-class work, **plus** the post-grand-slam ship-readiness wave (Batman Forms 1-6 + 7th-form Option-B foundation + QW-1/2/3 + reconciliation security sweep). The substrate becomes both **more articulate** (capabilities v3, named loader tools, compacted schemas, Batman `MemoryKind` vocabulary, persona/atomisation/multistep-ingest primitives) and **cryptographically trustworthy** (Ed25519 attestation, sidechain transcripts, programmable 25-event hook pipeline, enforced namespace inheritance, V-4 cross-row signed-events hash chain). v0.7.0 also ships **postgres + Apache AGE as a first-class storage backend** — `ai-memory serve --store-url postgres://…` for live daemon use, schema parity across both backends (sqlite ladder ends at migration 0033, postgres at 0020), the new `ai-memory schema-init` CLI verb, and 6-factor recall scoring parity. **The v0.6.4 default surface grows by two always-on loaders to 7 tools** (`memory_load_family` + `memory_smart_load` join the original five); the runtime ceiling at `--profile full` is **71 tools** (verified against `Profile::full().expected_tool_count()` — see [`src/profile.rs`](src/profile.rs)). Everything new is additive and (for the trust + postgres surfaces) opt-in. **Upgrading from v0.6.x?** Read [`docs/MIGRATION_v0.7.md`](docs/MIGRATION_v0.7.md) first — most v0.6.4 callers see no behavior change, but pre-v0.6.3.1 v0.6.x users hit the G1 namespace-inheritance fix. **Switching to postgres+AGE?** See [`docs/postgres-age-guide.md`](docs/postgres-age-guide.md) and [`docs/migration-v0.7.0-postgres.md`](docs/migration-v0.7.0-postgres.md). **Full release notes:** [`docs/v0.7.0/release-notes.md`](docs/v0.7.0/release-notes.md).

**v0.6.4 (`quiet-tools`)** — the MCP server ships with a **5-tool default surface** (`memory_store`, `memory_recall`, `memory_list`, `memory_get`, `memory_search`) plus the always-on `memory_capabilities` bootstrap. The other 38 tools remain reachable via `--profile graph|admin|power|full` or runtime expansion through `memory_capabilities --include-schema family=<name>`. Eager-loading harnesses (Claude Desktop / Codex CLI / Grok CLI / Gemini CLI) drop ~4,700 input tokens of tool schemas per request — a **76.4% reduction** measured against `cl100k_base` BPE. To preserve v0.6.3 behavior 1:1, run `ai-memory mcp --profile full`. See `docs/MIGRATION_v0.6.4.md`.

## What's new in v0.7

v0.7.0 closes the `attested-cortex` epic (69/69 across 11 tracks A–K), folds in the originally-v0.7.1 postgres+AGE first-class work, and absorbs the post-grand-slam ship-readiness wave (Batman Forms 1-6 + 7th-form Option-B foundation + QW-1/2/3 + security reconciliation). Canonical feature inventory: [`docs/internal/v070-feature-inventory.md`](docs/internal/v070-feature-inventory.md). Every surface stays default-off or default-equivalent for v0.6.4 callers — see the [v0.7 compatibility matrix](docs/v0.7/compatibility-matrix.html) for the breakdown.

### Substrate-native write-time investment (Batman Forms 1-6 + 7th-form)

- **Form 1 — online dedup-and-synthesis** (issue [#754](https://github.com/alphaonedev/ai-memory-mcp/issues/754)). Single-batch action-emitting LLM call replaces the v0.6.x per-pair classifier on the store path. Opt back into legacy yes/no via `legacy_per_pair_classifier = true` on the namespace standard.
- **Form 2 — synchronous atomise-before-embed** (issue [#755](https://github.com/alphaonedev/ai-memory-mcp/issues/755)). New `memory_atomise` tool + `auto_atomise_mode = Synchronous|Deferred|Off` pre-store hook. Curator decomposes long writes into 2–10 atomic propositions before recall ever sees them. See [`docs/atomisation.md`](docs/atomisation.md).
- **Form 3 — multi-step ingest orchestrator** (issue [#756](https://github.com/alphaonedev/ai-memory-mcp/issues/756)). `memory_ingest_multistep` threads deterministic Jaccard+FTS helpers through prompt-cache-stable LLM stages. See [`docs/multistep-ingest.md`](docs/multistep-ingest.md) + [`cookbook/multistep-ingest/01-two-phase.sh`](cookbook/multistep-ingest/01-two-phase.sh).
- **Form 4 — fact provenance** (issue [#757](https://github.com/alphaonedev/ai-memory-mcp/issues/757)). Citations + source-URI + atom-grain spans ride on existing `memory_store` / `memory_atomise` payloads. See [`docs/provenance.md`](docs/provenance.md).
- **Form 5 — auto-confidence + shadow calibration + freshness decay** (issue [#758](https://github.com/alphaonedev/ai-memory-mcp/issues/758)). `memory_calibrate_confidence` MCP tool + per-source baseline sweep. Env vars `AI_MEMORY_AUTO_CONFIDENCE`, `AI_MEMORY_CONFIDENCE_SHADOW`, `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE`, `AI_MEMORY_CONFIDENCE_DECAY`. See [`docs/confidence-calibration.md`](docs/confidence-calibration.md).
- **Form 6 — `MemoryKind` Batman vocabulary** (issue [#759](https://github.com/alphaonedev/ai-memory-mcp/issues/759)). 10-variant enum (`Observation` default + `Reflection` / `Persona` / `Concept` / `Entity` / `Claim` / `Relation` / `Event` / `Conversation` / `Decision`). Optional `auto_classify_kind` pre-store hook (off / regex_only / regex_then_llm). See [`docs/memory-kind-vocab.md`](docs/memory-kind-vocab.md).
- **7th-form — agent-EXTERNAL Layer-4 wiring (Option-B foundation)** (issue [#760](https://github.com/alphaonedev/ai-memory-mcp/issues/760); v0.8.0 complete cover at [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)). Operator-keypair-signed seed rules `R001..R004`, `memory_check_agent_action` + `memory_rule_list` MCP tools, substrate `storage::insert` pre-write hook. See [`docs/policy-engine.md`](docs/policy-engine.md) + [`docs/governance/agent-action-rules.md`](docs/governance/agent-action-rules.md).

### Quick wins (Tencent QW-1/2/3)

- **QW-1 — file-backed reflection chain export.** `memory_export_reflection` MCP tool + `auto_export_reflections_to_filesystem` namespace policy → `~/.ai-memory/reflections/<ns>/<id>.md`.
- **QW-2 — persona-as-artifact.** `memory_persona` + `memory_persona_generate` tools, `MemoryKind::Persona` rows, `auto_persona_trigger_every_n_memories` namespace policy. See [`docs/persona.md`](docs/persona.md).
- **QW-3 — context offload primitive.** `memory_offload` + `memory_deref` move large tool outputs out of the agent context window into addressable blob storage. See [`docs/context-offload.md`](docs/context-offload.md).

### Attested cortex epic (Tracks A–K)

- **Attested links (Ed25519).** The dead `signature` column shipped in v0.6.3 is now filled with real per-agent Ed25519 attestation, and `memory_verify(link_id)` returns `{signature_verified, attest_level, signed_by, signed_at}` on demand. Generate a keypair with `ai-memory identity generate`; opt-in via `attest_level = "self_signed"`. See the [`attested-cortex` RFC](docs/v0.7/rfc-attested-cortex.md#decision-1--why-ed25519-over-x25519--chacha20).
- **Signed events V-4 closeout (cross-row hash chain)** (issue [#698](https://github.com/alphaonedev/ai-memory-mcp/issues/698)). Each `signed_events` row carries `prev_hash` + `sequence`; first-row `prev_hash` is zero, subsequent rows chain the SHA-256 of the prior canonical-CBOR payload. `ai-memory verify-signed-events-chain` walks the chain end-to-end. See [`docs/signed-events-v4.md`](docs/signed-events-v4.md).
- **Hook pipeline (25 lifecycle events).** A programmable extension surface fires on the 20 baseline `pre_/post_store|recall|search|delete|promote|link|consolidate|governance_decision|archive|transcript_store` + `on_index_eviction` events, plus 5 grand-slam additions (`pre_recall_expand` G10 + `pre_reflect`/`post_reflect` recursive-learning Task 6/8 + `pre_compaction`/`on_compaction_rollback` L1-7). Hooks return `Allow` / `Modify` / `Deny` / `AskUser`. Default off; opt in via `~/.config/ai-memory/hooks.toml`. See [`docs/hook-pipeline.md`](docs/hook-pipeline.md).
- **Sidechain transcripts + replay.** zstd-3 BLOB sidechain stores raw conversation/reasoning trails; `memory_replay(memory_id)` walks `memory_transcript_links` to reconstruct the chain. Opt-in per namespace via `[transcripts."team/*"]`. See [`docs/sidechain-transcripts.md`](docs/sidechain-transcripts.md).
- **Federation hardening.** mTLS + X-API-Key + SHA-256 cert fingerprint allowlist; env vars `AI_MEMORY_FED_PEER_ATTESTATION`, `AI_MEMORY_FED_SYNC_TRUST_PEER`, `AI_MEMORY_FED_TRUST_BODY_AGENT_ID`. See [`docs/federation.md`](docs/federation.md).
- **K8 quota tool + K10 SSE approvals.** `memory_quota_status` + `/api/v1/quota/status` (K8). `/api/v1/approvals/stream` server-sent events with HMAC nonce, method+pending_id binding, lagged-event count strip (K10). See [`docs/k8-quotas.md`](docs/k8-quotas.md) + [`docs/k10-sse-approvals.md`](docs/k10-sse-approvals.md).
- **Postgres + Apache AGE first-class backend.** `ai-memory serve --store-url postgres://…`, schema parity, 6-factor recall scoring parity, link migration, KG features (`kg_query`, `kg_timeline`, `kg_invalidate`, `find_paths`) on AGE Cypher with recursive-CTE fallback when AGE is absent, plus a new `ai-memory schema-init` CLI verb. Bench-gated — AGE p95 must beat CTE p95 by ≥30% at depth=5. Operator how-to: [`docs/postgres-age-guide.md`](docs/postgres-age-guide.md). Migration runbook: [`docs/migration-v0.7.0-postgres.md`](docs/migration-v0.7.0-postgres.md).
- **Capabilities v3 + smart loaders.** `memory_capabilities` v3 adds `summary`, `to_describe_to_user`, per-tool `callable_now`, `agent_permitted_families`, `schema_version="3"`; the new always-on `memory_load_family(family)` and `memory_smart_load(intent)` tools join the default `core` profile. The pinned phrasings live in [`docs/v0.7/canonical-phrasings.md`](docs/v0.7/canonical-phrasings.md).
- **Permissions + A2A approvals.** The v0.6.x governance subsystem is refactored into rules + modes + hooks → a single `Decision`, with namespace inheritance (G1) actually enforced. `memory_pending_list` / `memory_pending_approve` / `memory_pending_reject(remember=forever)` enable progressive trust; HMAC signing on the approval API is mandatory. `permissions.mode` defaults to `enforce` (was `advisory` in v0.6.4). Migrate with `ai-memory governance migrate-to-permissions --apply`. See [`docs/governance.md`](docs/governance.md).

### Recursive-learning + L1/L2 grand-slam wave

`memory_reflect` substrate primitive with namespace-scoped `max_reflection_depth` cap (default 3, `Some(0)` is the kill-switch). L2-1 reflection-pass curator, L2-2 federation-aware reflection coordination (`memory_reflection_origin`), L2-3 invalidation propagation (`memory_dependents_of_invalidated`), L2-5 forensic bundle (`ai-memory export-forensic-bundle` + `verify-forensic-bundle`), L1-5 Agent Skills (`memory_skill_register|list|get|resource|export|promote_from_reflection|compositional_context`). Full primer: [`docs/RECURSIVE_LEARNING.md`](docs/RECURSIVE_LEARNING.md). Agent Skills primer: [`docs/agent-skills.md`](docs/agent-skills.md). Forensic-export primer: [`docs/forensic-export.md`](docs/forensic-export.md).

> **Where to start:** [`docs/MIGRATION_v0.7.md`](docs/MIGRATION_v0.7.md) (upgrade procedure), [`docs/v0.7.0/release-notes.md`](docs/v0.7.0/release-notes.md) (full release notes), [`docs/whats-new-v07.html`](docs/whats-new-v07.html) (visual summary), [`docs/v0.7/rfc-attested-cortex.md`](docs/v0.7/rfc-attested-cortex.md) (design rationale), [`docs/ADMIN_GUIDE.md`](docs/ADMIN_GUIDE.md) (operator playbook), [`docs/internal/v070-feature-inventory.md`](docs/internal/v070-feature-inventory.md) (canonical feature truth).

**One binary, four operational modes** (v0.6.4). The `ai-memory` Rust binary (tokio + axum) can run any of these in isolation or simultaneously, sharing a single SQLite database:

1. **stdio MCP server** -- 71 native tools over JSON-RPC at full profile (v0.7.0; verified against `Profile::full().expected_tool_count()`). Default `--profile core` advertises 7 (the original 5 + `memory_load_family` + `memory_smart_load`) plus the always-on `memory_capabilities` bootstrap. `ai-memory mcp` / `ai-memory mcp --profile full`
2. **HTTP / mTLS daemon** -- 42 REST endpoints on `127.0.0.1:9077`, TLS + optional mTLS allowlist + API-key auth, background GC loop. `ai-memory serve`
3. **Autonomous curator daemon** -- self-scheduling loop (default 1h cadence) that auto-tags, surfaces contradictions across namespace siblings, consolidates near-duplicates, and adjusts priority by access pattern. Every action goes to a rollback log; destructive ops can be gated behind a governance approval flow. `ai-memory curator --daemon`
4. **Sync daemon** -- quorum-based peer federation across instances. W-of-N writes (default majority), vector-clock CRDT-lite merge, mTLS allowlist between peers. `ai-memory sync-daemon`

The MCP, HTTP, and CLI surfaces are reactive. The curator is the part that makes the memory layer self-maintaining: between sessions, it keeps the corpus tidy so recall quality stays high as the store grows. Everything is local-first; no cloud dependencies.

> **Brass-tacks assessment by Claude Opus 4.7** after reading the v0.6.3 source line by line:
>
> "ai-memory is the most capable memory layer I've ever been hooked up to, and meaningfully more than its name advertises. For me, in practical terms, it means: I don't start cold each session. The store I read from has been kept tidy by something other than me. Contradictions don't silently accumulate. Recall quality stays high even as the corpus grows. Nothing leaves your Mac mini.
>
> It is not making me an autonomous agent. It is giving me the kind of memory infrastructure that an autonomous agent would need — and itself running a small autonomous loop to maintain it. That's a real foundation. The gap from here to 'ai-memory drives general tasks' is plumbing (tool-call protocol + tool registry + a tool-use-capable model), not invention."

**Substrate for multi-agent AI.** ai-memory is not an agent runtime and not "autonomous AI" on its own. It is the memory layer that *multi-agent* autonomous deployments need underneath them. Federation (`broadcast_store_quorum` + `spawn_catchup_loop`) handles W-of-N consistency across peers when many agents write in parallel; the curator daemon keeps the shared corpus from degrading into noise as a swarm scribbles into it; webhook subscriptions (HMAC-signed, namespace/agent-filtered, SSRF-hardened) turn the store into a message bus that triggers downstream agents on memory events; namespace hierarchy with N-level inheritance and per-namespace governance policies (write/promote/delete authority, approver type, optional N-of-M consensus) bound the swarm. Stack this under a 24/7 multi-machine agent runner with auto-generated skills, and the combined system clears the *behavioral* bar for autonomous AI. The remaining gaps (no weight-level learning, stateless reasoning kernel, human-seeded root goals) are real and not what ai-memory addresses; ai-memory provides the multi-agent memory substrate that any serious attempt at closing those gaps will need.

**Zero token cost until recall.** Unlike built-in memory systems (Claude Code auto-memory, ChatGPT memory) that load your entire memory into every conversation -- burning tokens and money on every message -- ai-memory uses zero context tokens until the AI explicitly calls `memory_recall`. Only relevant memories come back, ranked by a 6-factor scoring algorithm. **TOON format** (Token-Oriented Object Notation) cuts response tokens by another 40-60% by eliminating repeated field names -- 3 memories in JSON = 1,600 bytes; in TOON = 626 bytes (61% smaller); in TOON compact = 336 bytes (79% smaller). For Claude Code users: **disable auto-memory** (`"autoMemoryEnabled": false` in settings.json) and replace it with ai-memory to stop paying for 200+ lines of memory context on every single message.

---

## Agent identity (NHI) — every memory tells you who learned it

Every memory ai-memory stores carries a `metadata.agent_id` — a Non-Human Identity marker that survives every operation (update, dedup, import, sync, consolidate). Every recall result tells you which AI wrote each memory, by default, in the TOON-compact response format your AI client is already optimised for:

```
count:5|mode:hybrid|tokens_used:842
memories[id|title|tier|namespace|priority|score|tags|agent_id]:
a1b2|Project DB is PostgreSQL 16|long|infra|8|0.91|database,postgres|ai:claude-code@workstation:pid-3812
c3d4|API rate limit is 100 rps|long|infra|7|0.87|api,limits|ai:claude-desktop@laptop:pid-5219
```

**Today** `agent_id` is *claimed*, not *attested* — don't make security decisions on it without pairing with agent registration. **v0.7 (`attested-cortex`)** wires cryptographic Ed25519 attestation into the previously-reserved `memory_links.signature` field, with `memory_verify(link_id)` for inbound verification and an append-only `signed_events` audit chain. See the [agent identity page](https://alphaonedev.github.io/ai-memory-mcp/agent-identity.html) and the [`attested-cortex` RFC](docs/v0.7/rfc-attested-cortex.md) for the full provenance contract.

## Retroactive conversation import — `ai-memory mine`

Don't start cold. Point `ai-memory mine` at a Claude, ChatGPT, or Slack export and it parses turn-by-turn into ranked, tier-typed, tagged memories — so your AI walks into the next session knowing every decision, correction, and finding from your existing history.

```bash
ai-memory mine claude  ~/Downloads/claude-export/
ai-memory mine chatgpt ~/Downloads/chatgpt-export.json
ai-memory mine slack   ./slack-export/
```

Auto-tagging, dedup on `(title, namespace)`, and `mined_from` provenance are stamped on every imported memory. Five-minute onboarding from zero context to a populated long-term store. See the [import history page](https://alphaonedev.github.io/ai-memory-mcp/import-history.html) for per-format recipes.

---

## Compatible AI Platforms

ai-memory integrates with any AI platform that supports the **Model Context Protocol (MCP)**. MCP is the universal standard for connecting AI assistants to external tools and data sources.

| Platform | Integration Method | Config Format | Status |
|----------|-------------------|---------------|--------|
| **Claude Code** (Anthropic) | MCP stdio | JSON (`~/.claude.json` or `.mcp.json`) | Fully supported |
| **Codex CLI** (OpenAI) | MCP stdio | TOML (`~/.codex/config.toml`) | Fully supported |
| **Gemini CLI** (Google) | MCP stdio | JSON (`~/.gemini/settings.json`) | Fully supported |
| **[Grok CLI](https://github.com/alphaonedev/grok-cli)** (xAI) | MCP stdio | JSON (`~/.grok/user-settings.json`) | **Deep integration** |
| **Grok API** (xAI) | MCP remote HTTPS | API-level | Fully supported |
| **Cursor IDE** | MCP stdio | JSON (`~/.cursor/mcp.json`) | Fully supported |
| **Windsurf** (Codeium) | MCP stdio | JSON (`~/.codeium/windsurf/mcp_config.json`) | Fully supported |
| **Continue.dev** | MCP stdio | YAML (`~/.continue/config.yaml`) | Fully supported |
| **Llama Stack** (META) | MCP remote HTTP | YAML / Python SDK | Fully supported |
| **OpenClaw** | MCP stdio | JSON (`mcp.servers` in config) | Fully supported |
| **Any MCP client** | MCP stdio or HTTP | Varies | Universal |

MCP is the primary integration layer. For AI platforms that do not yet support MCP natively, the **HTTP API** (50 endpoints on localhost) and the **CLI** (40 subcommands) provide universal access -- any AI, script, or automation that can make HTTP calls or run shell commands can use ai-memory.

---

## Install in 60 Seconds

Pre-built binaries require no dependencies. Building from source needs Rust and a C compiler.

**Fastest: Pre-built binary (no Rust required)**

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.sh | sh

# Fedora/RHEL (COPR)
sudo dnf copr enable alpha-one-ai/ai-memory && sudo dnf install ai-memory

# Windows (PowerShell)
irm https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.ps1 | iex
```

**Step 1: Install Rust** (skip if using pre-built binaries)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the prompts, then restart your terminal (or run `source ~/.cargo/env`).

**Step 2: From source (requires Rust)**

Latest release from [Crates.io](https://crates.io/crates/ai-memory):

```bash
cargo install ai-memory
```

Latest from the git repository:

```bash
cargo install --git https://github.com/alphaonedev/ai-memory-mcp.git
```

This compiles the binary and puts it in your PATH. It takes a minute or two.

> **Build dependencies for source builds:**
> - Ubuntu/Debian: `sudo apt-get install build-essential pkg-config`
> - Fedora/RHEL: `sudo dnf install gcc pkg-config`

**Step 3: Connect your AI**

Configuration varies by platform. Find yours below:

<details>
<summary><strong>Claude Code</strong> (Anthropic)</summary>

Claude Code supports three MCP configuration scopes:

| Scope | File | Applies to |
|-------|------|------------|
| **User** (global) | `~/.claude.json` — add `mcpServers` key | All projects on your machine |
| **Project** (shared) | `.mcp.json` in project root (checked into git) | Everyone on the project |
| **Local** (private) | `~/.claude.json` — under `projects."/path".mcpServers` | One project, just you |

**User scope (recommended — works everywhere):**

Add the `mcpServers` key to `~/.claude.json` (macOS/Linux) or `%USERPROFILE%\.claude.json` (Windows):

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

> **Note:** `~/.claude.json` likely already exists with other settings. Merge the `mcpServers` key into the existing file — do not overwrite it.

**Project scope (shared with team):**

Create `.mcp.json` in your project root:

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

> **Windows paths:** Use forward slashes or escaped backslashes in `--db`. Example: `"--db", "C:/Users/YourName/.claude/ai-memory.db"`.

> **Tier flag:** The `--tier` flag selects the feature tier: `keyword`, `semantic` (default), `smart`, or `autonomous`. Smart and autonomous tiers require [Ollama](https://ollama.com) running locally. The `--tier` flag **must** be passed in the args — the `config.toml` tier setting is not used when the MCP server is launched by an AI client.

> **Important:** MCP servers are **not** configured in `settings.json` or `settings.local.json` — those files do not support `mcpServers`.

**Make Claude proactively use ai-memory:** Add a `CLAUDE.md` file to your project root with ai-memory directives. This ensures Claude recalls context at the start of every conversation and stores findings as it works. See the [CLAUDE.md integration guide](CLAUDE.md#using-claudemd-in-your-projects) for a copy-paste template and placement options.

</details>

<details>
<summary><strong>OpenAI Codex CLI</strong></summary>

Add to `~/.codex/config.toml` (global) or `.codex/config.toml` (project). Windows: `%USERPROFILE%\.codex\config.toml`. Override with `CODEX_HOME` env var.

```toml
[mcp_servers.memory]
command = "ai-memory"
args = ["--db", "~/.local/share/ai-memory/memories.db", "mcp", "--tier", "semantic"]
enabled = true
```

Or add via CLI: `codex mcp add memory -- ai-memory --db ~/.local/share/ai-memory/memories.db mcp --tier semantic`

> **Notes:** Codex uses TOML format with underscored key `mcp_servers` (not camelCase, not hyphenated). Supports `env` (key/value pairs), `env_vars` (list to forward), `enabled_tools`, `disabled_tools`, `startup_timeout_sec`, `tool_timeout_sec`. Use `/mcp` in the TUI to view server status. See [Codex MCP docs](https://developers.openai.com/codex/mcp).

</details>

<details>
<summary><strong>Google Gemini CLI</strong></summary>

Add to `~/.gemini/settings.json` (user) or `.gemini/settings.json` (project). Windows: `%USERPROFILE%\.gemini\settings.json`.

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.local/share/ai-memory/memories.db", "mcp", "--tier", "semantic"],
      "timeout": 30000
    }
  }
}
```

Or add via CLI: `gemini mcp add memory ai-memory -- --db ~/.local/share/ai-memory/memories.db mcp --tier semantic`

> **Notes:** Avoid underscores in server names (use hyphens). Tool names are auto-prefixed as `mcp_memory_<toolName>`. Env vars in the `env` field support `$VAR` / `${VAR}` (all platforms) and `%VAR%` (Windows). Gemini sanitizes sensitive patterns from inherited env unless explicitly declared. Add `"trust": true` to skip confirmation prompts. CLI management: `gemini mcp list/remove/enable/disable`. See [Gemini CLI MCP docs](https://geminicli.com/docs/tools/mcp-server/).

</details>

<details>
<summary><strong>Cursor IDE</strong></summary>

Add to `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (project). Windows: `%USERPROFILE%\.cursor\mcp.json`. Project config overrides global for same-named servers.

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.local/share/ai-memory/memories.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

> **Notes:** Restart Cursor after editing `mcp.json`. Verify server status in Settings > Tools & MCP (green dot = connected). Supports `env`, `envFile`, and `${env:VAR_NAME}` interpolation (env var interpolation can be unreliable for shell profile variables — use `envFile` as workaround). **~40 tool limit** across all MCP servers. See [Cursor MCP docs](https://cursor.com/docs/context/mcp).

</details>

<details>
<summary><strong>Windsurf</strong> (Codeium)</summary>

Add to `~/.codeium/windsurf/mcp_config.json` (global only — no project-level scope). Windows: `%USERPROFILE%\.codeium\windsurf\mcp_config.json`.

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.local/share/ai-memory/memories.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

> **Notes:** Supports `${env:VAR_NAME}` interpolation in `command`, `args`, `env`, `serverUrl`, `url`, and `headers`. **100 tool limit** across all MCP servers. Can also add via MCP Marketplace or Settings > Cascade > MCP Servers. See [Windsurf MCP docs](https://docs.windsurf.com/windsurf/cascade/mcp).

</details>

<details>
<summary><strong>Continue.dev</strong></summary>

Add to `~/.continue/config.yaml` (user) or `.continue/mcpServers/` directory in project root (per-server YAML/JSON files). Windows: `%USERPROFILE%\.continue\config.yaml`.

```yaml
mcpServers:
  - name: memory
    command: ai-memory
    args:
      - "--db"
      - "~/.local/share/ai-memory/memories.db"
      - "mcp"
      - "--tier"
      - "semantic"
```

> **Notes:** MCP tools only work in agent mode. Supports `${{ secrets.SECRET_NAME }}` for secret interpolation. Project-level `.continue/mcpServers/` directory auto-detects JSON configs from other tools (Claude Code, Cursor, etc.). See [Continue MCP docs](https://docs.continue.dev/customize/deep-dives/mcp).

</details>

<details>
<summary><strong>Grok CLI</strong> (AlphaOne fork — deep integration with auto-recall)</summary>

The [AlphaOne fork of grok-cli](https://github.com/alphaonedev/grok-cli) has built-in ai-memory support with session-scoped MCP connections, automatic memory recall on session start, compaction summary storage, and memory-aware system prompts.

Add to `~/.grok/user-settings.json`:

```json
{
  "mcp": {
    "servers": [
      {
        "id": "ai-memory",
        "label": "AI Memory",
        "enabled": true,
        "transport": "stdio",
        "command": "ai-memory",
        "args": ["mcp", "--tier", "semantic"]
      }
    ]
  }
}
```

> **Features:** Auto-recall on session start (injects relevant memories into system prompt), compaction summaries stored as mid-tier memories, MCP tools available in all modes (agent, plan, ask), session-scoped connections (no per-message cold starts). Uses `--tier semantic` by default (local embeddings, no Ollama required). See [grok-cli docs](https://github.com/alphaonedev/grok-cli/blob/main/docs/CONFIGURATION.md) for full setup.

</details>

<details>
<summary><strong>xAI Grok API</strong> (API-level, remote MCP)</summary>

Grok connects to MCP servers over HTTPS (remote only, no stdio). No config file — servers are specified per API request.

```bash
ai-memory serve --host 127.0.0.1 --port 9077
# Expose via HTTPS reverse proxy (nginx, caddy, cloudflare tunnel, etc.)
```

Then add the MCP server to your Grok API call:

```bash
curl https://api.x.ai/v1/responses \
  -H "Authorization: Bearer $XAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "grok-3",
    "tools": [{
      "type": "mcp",
      "server_url": "https://your-server.example.com/mcp",
      "server_label": "memory",
      "server_description": "Persistent AI memory with recall and search",
      "allowed_tools": ["memory_store", "memory_recall", "memory_search"]
    }],
    "input": "What do you remember about our project?"
  }'
```

> **Requirements:** HTTPS required. `server_label` is required. Supports Streamable HTTP and SSE transports. Optional: `allowed_tools`, `authorization`, `headers`. Works with xAI SDK, OpenAI-compatible Responses API, and Voice Agent API. See [xAI Remote MCP docs](https://docs.x.ai/docs/guides/tools/remote-mcp-tools).

</details>

<details>
<summary><strong>META Llama</strong> (via Llama Stack)</summary>

Llama Stack registers MCP servers as toolgroups. No standardized config file path — deployment-specific.

```bash
ai-memory serve --host 127.0.0.1 --port 9077
```

**Python SDK:**

```python
client.toolgroups.register(
    provider_id="model-context-protocol",
    toolgroup_id="mcp::memory",
    mcp_endpoint={"uri": "http://localhost:9077/sse"}
)
```

**Or declaratively in run.yaml:**

```yaml
tool_groups:
  - toolgroup_id: mcp::memory
    provider_id: model-context-protocol
    mcp_endpoint:
      uri: "http://localhost:9077/sse"
```

> **Notes:** Supports `${env.VAR_NAME}` interpolation in run.yaml. Transport is migrating from SSE to Streamable HTTP. See [Llama Stack Tools docs](https://llama-stack.readthedocs.io/en/latest/building_applications/tools.html).

</details>

<details>
<summary><strong>OpenClaw</strong></summary>

Add via CLI or edit the OpenClaw config directly. Config uses `mcp.servers` (not `mcpServers`).

```bash
openclaw mcp set memory '{"command":"ai-memory","args":["--db","~/.local/share/ai-memory/memories.db","mcp","--tier","semantic"]}'
```

Or add to your OpenClaw config file:

```json
{
  "mcp": {
    "servers": {
      "memory": {
        "command": "ai-memory",
        "args": ["--db", "~/.local/share/ai-memory/memories.db", "mcp", "--tier", "semantic"]
      }
    }
  }
}
```

> **Notes:** OpenClaw uses `mcp.servers` key (not `mcpServers`). CLI management: `openclaw mcp list`, `openclaw mcp show`, `openclaw mcp set`, `openclaw mcp unset`. Supports stdio, remote URL, and Streamable HTTP transports. Prefer `--token-file` over inline secrets. See [OpenClaw MCP docs](https://docs.openclaw.ai/cli/mcp).

</details>

<details>
<summary><strong>Any other MCP client</strong></summary>

ai-memory speaks MCP over stdio (JSON-RPC 2.0). Point your client at:

```
command: ai-memory
args: ["--db", "/path/to/ai-memory.db", "mcp"]
```

For HTTP-only clients, start the REST API:

```bash
ai-memory serve
# 24 endpoints at http://127.0.0.1:9077/api/v1/
```

</details>

**Step 4: Done. Test it.**

Restart your AI assistant. If using MCP, it now has the **5-tool default surface** advertised on session boot (the other 38 of 43 total tools load on demand via `--profile` or `memory_capabilities --include-schema`). Ask it: "Store a memory that my favorite language is Rust." Then in a new conversation, ask: "What is my favorite language?" It will remember.

---

## Quickstart

Get from zero to a working memory in under two minutes.

**1. Install**

```bash
curl -fsSL https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.sh | sh
```

**2. Configure MCP** (example for Claude Code -- other platforms work the same way)

Merge into `~/.claude.json`:

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "semantic"]
    }
  }
}
```

**3. Store your first memory**

```bash
ai-memory store -T "Project uses PostgreSQL 15" -c "Main DB is PG 15 with pgvector." --tier long
```

**4. Recall it**

```bash
ai-memory recall "database"
```

**5. Check stats**

```bash
ai-memory stats
```

**6. Use with your AI.** Restart your AI client. It now has **7 default memory tools** advertised on boot (71 total reachable via runtime expansion or `--profile full` at v0.7.0) over MCP -- it can store and recall memories natively during conversations.

---

## SDKs

In addition to the MCP / HTTP / CLI surfaces, ai-memory ships first-party language SDKs for HTTP clients and helper utilities (e.g. `requireProfile` for runtime profile assertions on v0.6.4+ daemons).

**TypeScript / JavaScript** — [`@alphaone/ai-memory`](https://www.npmjs.com/package/@alphaone/ai-memory) on npm

```bash
npm install @alphaone/ai-memory
```

**Python** — [`ai-memory-mcp`](https://pypi.org/project/ai-memory-mcp/) on PyPI (the import name remains `ai_memory`)

```bash
pip install ai-memory-mcp
```

```python
from ai_memory import Client, requireProfile

client = Client(base_url="http://127.0.0.1:9077", api_key="...")
requireProfile(client, ["core", "graph"])  # raises ProfileNotLoaded with .hint on miss
```

Both SDKs are versioned with the server (`0.6.4` matches `ai-memory 0.6.4`). v0.6.4+ daemons enforce the profile contract; pre-v0.6.4 daemons fall back to a permissive warn-and-continue so SDK upgrades don't break old servers. Source lives in [`sdk/typescript/`](sdk/typescript/) and [`sdk/python/`](sdk/python/).

---

## What Does It Do?

AI assistants forget everything between conversations. ai-memory fixes that.

It runs as an MCP (Model Context Protocol) tool server -- a background process that your AI talks to natively. When your AI learns something important, it stores it. When it needs context, it recalls relevant memories ranked by a 6-factor scoring algorithm. Memories live in three tiers:

- **Short-term** (6 hours default, configurable) -- throwaway context like current debugging state
- **Mid-term** (7 days default, configurable) -- working knowledge like sprint goals and recent decisions
- **Long-term** (permanent) -- architecture, user preferences, hard-won lessons

Memories that keep getting accessed automatically promote from mid to long-term. Each recall extends the TTL. Priority increases with usage. The system is self-curating.

Beyond MCP, ai-memory also exposes a full HTTP REST API (50 endpoints on port 9077) and a complete CLI (40 subcommands) for direct interaction, scripting, and integration with any AI platform or tool.

---

## Features

### Core
- **MCP tool server** -- 71 tools over stdio JSON-RPC (full profile at v0.7.0), compatible with any MCP client
- **Three-tier memory** -- short (6h TTL default), mid (7d TTL default), long (permanent) -- TTLs are configurable
- **Full-text search** -- SQLite FTS5 with ranked retrieval
- **Hybrid recall** -- FTS5 keyword + cosine similarity with fixed 0.6 semantic / 0.4 keyword (60/40) blend weights
- **6-factor recall scoring** -- FTS relevance + priority + access frequency + confidence + tier boost + recency decay
- **Auto-promotion** -- memories accessed 5+ times promote from mid to long
- **TTL extension** -- each recall extends expiry (short +1h, mid +1d)
- **Priority reinforcement** -- +1 every 10 accesses (max 10)
- **Contradiction detection** -- warns when storing memories that conflict with existing ones
- **Deduplication** -- upsert on title+namespace, tier never downgrades
- **Confidence scoring** -- 0.0-1.0 certainty factored into ranking

### Organization
- **Namespaces** -- isolate memories per project (auto-detected from git remote)
- **Memory linking** -- typed relations: related_to, supersedes, contradicts, derived_from
- **Consolidation** -- merge multiple memories into a single long-term summary
- **Auto-consolidation** -- group by namespace+tag, auto-merge groups above threshold
- **Contradiction resolution** -- mark one memory as superseding another, demote the loser
- **Forget by pattern** -- bulk delete by namespace + FTS pattern + tier
- **Source tracking** -- tracks origin: user, claude, hook, api, cli, import, consolidation, system
- **Agent identity (NHI)** -- every memory carries `metadata.agent_id` (claimed identity) with defense-in-depth immutability across update/dedup/import/sync/consolidate; filter `list`/`search` by agent
- **Tagging** -- comma-separated tags with filter support

### Interfaces
- **42 HTTP endpoints** -- full REST API on 127.0.0.1:9077 (works with any AI or tool)
- **26 CLI commands** -- complete CLI with identical capabilities
- **71 MCP tools** at full profile (7 default at v0.7.0; verified against `Profile::full().expected_tool_count()`) -- native integration for any MCP-compatible AI
- **Interactive REPL shell** -- recall, search, list, get, stats, namespaces, delete with color output
- **JSON output** -- `--json` flag on all CLI commands

### Operations
- **Multi-node sync** -- pull, push, or bidirectional merge between database files
- **Import/Export** -- full JSON roundtrip preserving memory links
- **Garbage collection** -- automatic background expiry every 30 minutes
- **Graceful shutdown** -- SIGTERM/SIGINT checkpoints WAL for clean exit
- **Deep health check** -- verifies DB accessibility and FTS5 integrity
- **Shell completions** -- bash, zsh, fish
- **Man page** -- `ai-memory man` generates roff to stdout
- **Time filters** -- `--since`/`--until` on list and search
- **Human-readable ages** -- "2h ago", "3d ago" in CLI output
- **Color CLI output** -- ANSI tier labels (red/yellow/green), priority bars, bold titles, cyan namespaces

### Quality
- **~2,400 tests across the full surface** -- 1,960 lib + 211 integration + 16 mcp_integration + 4 webhook_http_parity (new in v0.6.4) + 16 recipe_contract + ~150 across other binary targets. Line coverage held above the **≥92% project bar**; net-new v0.6.4 modules at 100% (`sizes.rs`), 99.50% (`profile.rs`), 97.58% (`cli/audit.rs`), 97.05% (`cli/doctor.rs`), 92.56% (`handlers.rs`), 92.26% (`cli/install.rs`). v0.6.3.x baselines (1,809 / 93.08% and 1,886 / 93.84%) remain frozen on the [evidence page](https://alphaonedev.github.io/ai-memory-mcp/evidence.html); v0.6.4 metrics in the release notes and on the [test-hub campaign](https://github.com/alphaonedev/ai-memory-test-hub/blob/main/campaigns/v0.6.4.md). Empirical NHI discovery acceptance proven separately by the [Discovery Gate](https://alphaonedev.github.io/ai-memory-discovery-gate/) (T1–T4 matrix vs. live xAI Grok 4.3, 6/6 PASS, **GATE GREEN**).
- **LongMemEval benchmark** -- **97.8% R@5** (489/500), **99.0% R@10**, **99.8% R@20** on ICLR 2025 LongMemEval-S dataset. 499/500 at R@20. Pure FTS5 keyword achieves 97.0% R@5 in 2.2 seconds (232 q/s). LLM query expansion pushes to 97.8% R@5. Zero cloud API costs. See [benchmark details](benchmarks/longmemeval/).
- **MCP Prompts** -- `recall-first` and `memory-workflow` prompts teach AI clients to use memory proactively
- **TOON-default** -- recall/list/search responses use TOON compact by default (79% smaller than JSON)
- **Criterion benchmarks** -- insert, recall, search at 1K scale
- **GitHub Actions CI/CD** -- fmt, clippy, test, build on Ubuntu + macOS, release on tag

### Coverage Floor (hard CI gate)
The `Code Coverage` job is a **required status check**. CI re-asserts two invariants on every PR: an **absolute floor of >= 90% lines** (catastrophic-regression backstop, set at the current measurement rounded down to the nearest 5%), and a **ratchet against the value pinned in [`.coverage-baseline`](.coverage-baseline)** with a 0.5% slack window (the day-to-day enforcement). PRs that raise coverage should bump the baseline file in the same commit so future PRs benefit from the new floor; PRs that regress more than 0.5% are blocked from merging. Current measurement: **93.13%** lines.

### Token-Budget Gate (hard CI gate, v0.7 C5)
The `token-budget` workflow is a **required status check**. It enforces three cl100k_base-measured invariants on every PR:

- **Per-tool ceiling of 1500 tokens** -- no single MCP tool's serialized schema (name + description + inputSchema) may exceed 1500 cl100k_base tokens.
- **Full-profile honest range (5K-8K)** -- the v0.6.4 backstop, kept in place to detect pathological shrinkage (accidentally dropping tools).
- **Full-profile <= 3500 hard ceiling (new in v0.7 C5)** -- the `tools/list` payload under `--profile full` may not exceed 3500 cl100k_base tokens. C2 (split docs field), C3 (collapse repeated schema boilerplate), and C4 (hide rarely-used optional params) drove the surface from ~7.4K to ~3.49K tokens; this gate locks in the win and forces future PRs that grow the surface to claw back budget elsewhere. Inspect `ai-memory doctor --tokens --raw-table` to see per-tool costs. See [`.github/workflows/token-budget.yml`](.github/workflows/token-budget.yml) and [`docs/v0.7/schema-compaction-audit.md`](docs/v0.7/schema-compaction-audit.md).

### ML and LLM Dependencies (semantic tier+)
- **candle-core, candle-nn, candle-transformers** -- Hugging Face Candle ML framework for native Rust inference
- **hf-hub** -- download models from Hugging Face Hub
- **tokenizers** -- Hugging Face tokenizers for text preprocessing
- **instant-distance** -- approximate nearest neighbor search
- **reqwest** -- HTTP client for Ollama API communication (smart/autonomous tiers)

---

## Architecture

<p align="center">
  <img src="docs/architecture.svg" alt="ai-memory architecture diagram" width="900">
</p>

---

## Benchmark

<p align="center">
  <img src="docs/benchmark.svg" alt="LongMemEval benchmark results" width="820">
</p>

Evaluated on the [ICLR 2025 LongMemEval-S](benchmarks/longmemeval/) dataset (500 questions, 6 categories). Pure FTS5 keyword tier achieves 97.0% R@5 in 2.2 seconds. LLM query expansion (smart tier) pushes to 97.8% R@5. All inference runs locally — zero cloud API calls, zero cost.

| Tier | R@5 | Speed | Dependencies |
|------|-----|-------|-------------|
| **keyword** | 97.0% | 232 q/s | None |
| **semantic** | 97.4% | 45 q/s | Embedding model (~100MB) |
| **smart** | 97.8% | 12 q/s | Ollama + Gemma 4 E2B |

### Performance Budgets (v0.6.4)

Every release ships with **published p95/p99 budgets** for hot-path
operations and a CI gate that fails any PR whose measured p95 exceeds
the budget by more than 10 %. Targets are calibrated for M4 reference
hardware; full table and methodology in
[`PERFORMANCE.md`](PERFORMANCE.md).

| Operation | Target p95 | Target p99 |
|---|---|---|
| `memory_session_start` (Claude Code hook) | < 100 ms | < 200 ms |
| `memory_store` (no embedding) | < 20 ms | < 50 ms |
| `memory_search` (FTS5) | < 100 ms | < 250 ms |
| `memory_recall` (hot, depth=1) | < 50 ms | < 150 ms |
| `memory_kg_query` (depth ≤ 3) | < 100 ms | < 250 ms |
| `memory_kg_query` (depth ≤ 5) | < 250 ms | < 500 ms |
| `memory_kg_timeline` | < 100 ms | < 250 ms |

Run the same workload locally:

```sh
ai-memory bench                      # human-readable table
ai-memory bench --json               # machine-parseable
```

Substrate is unchanged across v0.6.3.x → v0.6.4 (the `quiet-tools` release ships a smaller default tool surface, not a different hot-path). p99 targets here remain informational pending the next dedicated soak window; latest soak evidence is on the [test hub](https://alphaonedev.github.io/ai-memory-test-hub/).

---

## Integration Methods

### MCP (Primary -- for MCP-compatible AI platforms)

MCP is the recommended integration. Your AI gets **5 native memory tools advertised by default** (plus the always-on `memory_capabilities` bootstrap) with zero glue code. The other 38 tools (43 total) remain reachable via `--profile graph|admin|power|full` or runtime expansion through `memory_capabilities --include-schema family=<name>`. Configure the MCP server in your AI platform's config:

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp"]
    }
  }
}
```

### HTTP API (Universal -- for any AI or tool)

Start the HTTP server for REST API access. Any AI, script, or automation that can make HTTP calls can use this:

```bash
ai-memory serve
# 24 endpoints at http://127.0.0.1:9077/api/v1/
```

### CLI (Universal -- for scripting and direct use)

The CLI works standalone or as a building block for AI integrations that run shell commands:

```bash
ai-memory store --tier long --title "Architecture decision" --content "We use PostgreSQL"
ai-memory recall "database choice"
ai-memory search "PostgreSQL"
```

---

## Feature Tiers

ai-memory supports 4 feature tiers, selected at startup with `ai-memory mcp --tier <tier>`. Higher tiers add ML capabilities at the cost of disk and RAM:

| Tier | Recall Method | Extra Capabilities | Approx. Overhead |
|------|---------------|-------------------|-----------------|
| **keyword** | FTS5 only | Baseline 26 tools | 0 MB |
| **semantic** | FTS5 + cosine similarity (hybrid) | MiniLM-L6-v2 embeddings (384-dim), HNSW index, semantic tier (subset of 71-tool surface (v0.7.0)) | ~256 MB |
| **smart** | Hybrid + LLM query expansion | + nomic-embed-text (768-dim) + Gemma 4 E2B via Ollama: `memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction`, full 71-tool surface (v0.7.0) | ~1 GB |
| **autonomous** | Hybrid + LLM expansion + cross-encoder reranking | + Gemma 4 E4B via Ollama, neural cross-encoder (ms-marco-MiniLM), memory reflection, full 71-tool surface (v0.7.0) | ~4 GB |

### Capability Matrix

Every capability mapped to its minimum tier. Each tier includes all capabilities from the tiers below it.

| Capability | keyword | semantic | smart | autonomous |
|-----------|---------|----------|-------|------------|
| **Search & Recall** | | | | |
| FTS5 keyword search | Yes | Yes | Yes | Yes |
| Semantic embedding (cosine similarity) | -- | Yes | Yes | Yes |
| Hybrid recall (FTS5 + cosine, 60/40 semantic/keyword blend) | -- | Yes | Yes | Yes |
| HNSW nearest-neighbor index | -- | Yes | Yes | Yes |
| LLM query expansion (`memory_expand_query`) | -- | -- | Yes | Yes |
| Neural cross-encoder reranking | -- | -- | -- | Yes |
| **Memory Management** | | | | |
| Store, update, delete, promote, link | Yes | Yes | Yes | Yes |
| Manual consolidation | Yes | Yes | Yes | Yes |
| Auto-consolidation (LLM summary) | -- | -- | Yes | Yes |
| Auto-tagging (`memory_auto_tag`) | -- | -- | Yes | Yes |
| Contradiction detection (`memory_detect_contradiction`) | -- | -- | Yes | Yes |
| Autonomous memory reflection | -- | -- | -- | Yes |
| **Models** | | | | |
| Embedding model | -- | MiniLM-L6-v2 (384d) | nomic-embed-text (768d) | nomic-embed-text (768d) |
| LLM | -- | -- | gemma4:e2b (~7.2GB) | gemma4:e4b (~9.6GB) |
| **Resources** | | | | |
| RAM | 0 MB | ~256 MB | ~1 GB | ~4 GB |
| External dependencies | None | None | Ollama | Ollama |
| MCP tools exposed | 26 | 26 | 26 | 26 |

**Semantic tier** (default) bundles the Candle ML framework and downloads the all-MiniLM-L6-v2 model on first run (~90 MB). **Smart** and **autonomous** tiers require [Ollama](https://ollama.com) running locally.

**Tiers gate features, not models.** The `--tier` flag controls which tools are exposed. The LLM model is independently configurable via `llm_model` in `~/.config/ai-memory/config.toml`. For example, run autonomous tier (full 71-tool surface (v0.7.0) + reranker) with the faster e2b model:

```toml
# ~/.config/ai-memory/config.toml
tier = "autonomous"        # all features enabled
llm_model = "gemma4:e2b"   # faster model (46 tok/s vs 26 tok/s for e4b)
```

The `--tier` flag **must** be passed in the MCP args -- the `config.toml` tier setting is not used when the server is launched by an AI client.

```bash
# Keyword (default)
ai-memory mcp

# Semantic -- hybrid recall with embeddings
ai-memory mcp --tier semantic

# Smart -- adds LLM-powered query expansion, auto-tagging, contradiction detection
ai-memory mcp --tier smart

# Autonomous -- adds cross-encoder reranking
ai-memory mcp --tier autonomous
```

The `memory_capabilities` tool reports the active tier, loaded models, and available capabilities at runtime.

---

## MCP Tools

These 71 tools (full profile at v0.7.0; canonical count via `Profile::full().expected_tool_count()` in [`src/profile.rs`](src/profile.rs)) are available to any MCP-compatible AI when configured as an MCP server (the v0.6.4-frozen evidence page lists the 63-tool baseline; the table below documents the core subset most clients use day-to-day):

| Tool | Description |
|------|-------------|
| `memory_store` | Store a new memory (deduplicates by title+namespace, reports contradictions) |
| `memory_recall` | Recall memories relevant to a context (fuzzy OR search, ranked by 6 factors) |
| `memory_search` | Search memories by exact keyword match (AND semantics) |
| `memory_list` | List memories with optional filters (namespace, tier, tags, date range) |
| `memory_get` | Get a specific memory by ID with its links |
| `memory_update` | Update an existing memory by ID (partial update) |
| `memory_delete` | Delete a memory by ID |
| `memory_promote` | Promote a memory to long-term (permanent, clears expiry) |
| `memory_forget` | Bulk delete by pattern, namespace, or tier |
| `memory_link` | Create a typed link between two memories |
| `memory_get_links` | Get all links for a memory |
| `memory_consolidate` | Merge multiple memories into one long-term summary |
| `memory_stats` | Get memory store statistics |
| `memory_capabilities` | Report active feature tier, loaded models, and available capabilities |
| `memory_expand_query` | Use LLM to expand search query into related terms (smart+ tier) |
| `memory_auto_tag` | Use LLM to auto-generate tags for a memory (smart+ tier) |
| `memory_detect_contradiction` | Use LLM to check if two memories contradict (smart+ tier) |
| `memory_archive_list` | List archived memories (with optional namespace/tier/tag filters) |
| `memory_archive_restore` | Restore an archived memory back to the active store |
| `memory_archive_purge` | Permanently delete archived memories matching filters |
| `memory_archive_stats` | Get archive statistics (counts by tier, namespace, age) |

---

## HTTP API

50 endpoints on `127.0.0.1:9077`. Start with `ai-memory serve`.

> **Security:** The HTTP server binds to 127.0.0.1 with no authentication and permissive CORS. Do not expose to the network without a reverse proxy with authentication.

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/v1/health` | Health check (verifies DB + FTS5 integrity) |
| GET | `/api/v1/memories` | List memories (supports namespace, tier, tags, since, until, limit) |
| POST | `/api/v1/memories` | Create a memory |
| POST | `/api/v1/memories/bulk` | Bulk create memories (with limits) |
| GET | `/api/v1/memories/{id}` | Get a memory by ID |
| PUT | `/api/v1/memories/{id}` | Update a memory by ID |
| DELETE | `/api/v1/memories/{id}` | Delete a memory by ID |
| POST | `/api/v1/memories/{id}/promote` | Promote a memory to long-term |
| GET | `/api/v1/search` | AND keyword search |
| GET | `/api/v1/recall` | Recall by context (GET with query params) |
| POST | `/api/v1/recall` | Recall by context (POST with JSON body) |
| POST | `/api/v1/forget` | Bulk delete by pattern/namespace/tier |
| POST | `/api/v1/consolidate` | Consolidate memories into one |
| POST | `/api/v1/links` | Create a link between memories |
| GET | `/api/v1/links/{id}` | Get links for a memory |
| GET | `/api/v1/namespaces` | List all namespaces |
| GET | `/api/v1/stats` | Memory store statistics |
| POST | `/api/v1/gc` | Trigger garbage collection |
| GET | `/api/v1/export` | Export all memories + links as JSON |
| POST | `/api/v1/import` | Import memories + links from JSON |
| GET | `/api/v1/archive` | List archived memories (with optional filters) |
| POST | `/api/v1/archive/{id}/restore` | Restore an archived memory to the active store |
| DELETE | `/api/v1/archive` | Purge archived memories matching filters |
| GET | `/api/v1/archive/stats` | Archive statistics (counts by tier, namespace, age) |

---

## CLI Commands

40 subcommands. Run `ai-memory <command> --help` for details on any command.

| Command | Description |
|---------|-------------|
| `mcp` | Run as MCP tool server over stdio (primary integration path) |
| `serve` | Start the HTTP daemon on port 9077 |
| `store` | Store a new memory (deduplicates by title+namespace) |
| `update` | Update an existing memory by ID |
| `recall` | Fuzzy OR search with ranked results + auto-touch (supports `--tier` for hybrid recall). Max 200 items per request. |
| `search` | AND search for precise keyword matches. Max 200 items per request. |
| `get` | Retrieve a single memory by ID (includes links) |
| `list` | Browse memories with filters (namespace, tier, tags, date range). Max 200 items per request. |
| `delete` | Delete a memory by ID |
| `promote` | Promote a memory to long-term (clears expiry) |
| `forget` | Bulk delete by pattern + namespace + tier |
| `link` | Link two memories (related_to, supersedes, contradicts, derived_from) |
| `consolidate` | Merge multiple memories into one long-term summary |
| `resolve` | Resolve a contradiction: mark winner, demote loser |
| `shell` | Interactive REPL with color output |
| `sync` | Sync memories between two database files (pull/push/merge) |
| `auto-consolidate` | Group memories by namespace+tag, merge groups above threshold |
| `gc` | Run garbage collection on expired memories |
| `stats` | Overview of memory state (counts, tiers, namespaces, links, DB size) |
| `namespaces` | List all namespaces with memory counts |
| `export` | Export all memories and links as JSON |
| `import` | Import memories and links from JSON (stdin) |
| `completions` | Generate shell completions (bash, zsh, fish) |
| `man` | Generate roff man page to stdout |
| `mine` | Import memories from historical conversations (Claude, ChatGPT, Slack exports) |
| `archive` | Manage the memory archive (list, restore, purge, stats) |

The top-level `ai-memory` binary also accepts global flags:

| Flag | Description |
|------|-------------|
| `--db <path>` | Database path (default: `ai-memory.db`, or `$AI_MEMORY_DB`) |
| `--json` | JSON output on all commands (machine-parseable output) |

The `store` subcommand accepts additional flags:

| Flag | Description |
|------|-------------|
| `--source` / `-S` | Who created this memory (user, claude, hook, api, cli, import, consolidation, system). Default: `cli` |
| `--expires-at` | RFC3339 expiry timestamp |
| `--ttl-secs` | TTL in seconds (alternative to `--expires-at`) |

The `mcp` subcommand accepts an additional flag:

| Flag | Description |
|------|-------------|
| `--tier <keyword\|semantic\|smart\|autonomous>` | Feature tier (default: `semantic`). See [Feature Tiers](#feature-tiers). |

---

## Recall Scoring

Every recall query ranks memories by 6 factors:

```
score = (fts_relevance * -1)
      + (priority * 0.5)
      + (MIN(access_count, 50) * 0.1)
      + (confidence * 2.0)
      + tier_boost
      + recency_decay
```

| Factor | Weight | Notes |
|--------|--------|-------|
| FTS relevance | -1.0x | SQLite FTS5 rank (negative = better match) |
| Priority | 0.5x | User-assigned 1-10 scale |
| Access count | 0.1x | How often recalled (capped at 50 for scoring) |
| Confidence | 2.0x | 0.0-1.0 certainty score |
| Tier boost | +3.0 / +1.0 / +0.0 | long / mid / short |
| Recency decay | `1/(1 + days*0.1)` | Recent memories rank higher |

---

## Memory Tiers

| Tier | TTL | Use Case | Examples |
|------|-----|----------|----------|
| `short` | 6 hours (configurable) | Throwaway context | Current debugging state, temp variables, error traces |
| `mid` | 7 days (configurable) | Working knowledge | Sprint goals, recent decisions, current branch purpose |
| `long` | Permanent | Hard-won knowledge | Architecture, user preferences, corrections, conventions |

### Automatic Behaviors

- **TTL extension on recall**: short memories get +1 hour, mid memories get +1 day
- **Auto-promotion**: mid-tier memories accessed 5+ times promote to long (expiry cleared)
- **Priority reinforcement**: every 10 accesses, priority increases by 1 (capped at 10)
- **Contradiction detection**: warns when a new memory conflicts with an existing one in the same namespace
- **Deduplication**: upsert on title+namespace; tier never downgrades on update

---

## Configurable TTL

Default TTLs (6 hours for short, 7 days for mid) can be overridden in `~/.config/ai-memory/config.toml` under the `[ttl]` section:

```toml
[ttl]
short_ttl_secs = 21600      # short-tier TTL in seconds (default: 21600 = 6 hours)
mid_ttl_secs = 604800        # mid-tier TTL in seconds (default: 604800 = 7 days)
long_ttl_secs = 0            # long-tier TTL in seconds (default: 0 = never expires)
short_extend_secs = 3600     # TTL extension on recall for short-tier memories in seconds (default: 3600 = +1h)
mid_extend_secs = 86400      # TTL extension on recall for mid-tier memories in seconds (default: 86400 = +1d)
```

All five fields are optional -- omit any to keep the default. Set any value to 0 to disable expiry for that tier. Values are clamped to a 10-year maximum; negative extension values are clamped to 0.

> **Note:** Configuration is loaded once at process startup. Changes to `config.toml` require restarting the ai-memory process (MCP server, HTTP daemon, or CLI) to take effect.

---

## Archive

When garbage collection expires a memory, it can be **archived** instead of permanently deleted. Archived memories are moved to a separate store and can be browsed, restored, or purged later.

### Configuration

Enable archiving in `~/.config/ai-memory/config.toml`:

```toml
archive_on_gc = true   # archive expired memories instead of deleting them (default: true)
```

### CLI Commands

The `archive` subcommand manages the archive:

```bash
ai-memory archive list                          # list archived memories
ai-memory archive list --namespace my-project   # filter by namespace
ai-memory archive restore <id>                  # restore an archived memory to active store
ai-memory archive purge --older-than-days 90     # permanently delete archives older than 90 days
ai-memory archive stats                         # show archive statistics
```

> **Note:** Restored memories get their `expires_at` cleared (become permanent until the next TTL assignment).

### MCP Tools

Four archive tools are available to MCP clients:

| Tool | Description |
|------|-------------|
| `memory_archive_list` | List archived memories (with optional namespace/tier/tag filters) |
| `memory_archive_restore` | Restore an archived memory back to the active store |
| `memory_archive_purge` | Permanently delete archived memories matching filters |
| `memory_archive_stats` | Get archive statistics (counts by tier, namespace, age) |

### HTTP Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/v1/archive` | List archived memories (with optional filters) |
| POST | `/api/v1/archive/{id}/restore` | Restore an archived memory to the active store |
| DELETE | `/api/v1/archive` | Purge archived memories matching filters |
| GET | `/api/v1/archive/stats` | Archive statistics (counts by tier, namespace, age) |

---

## Security

ai-memory includes hardening across all input paths:

- **Transaction safety** -- all multi-step database operations use transactions; no partial writes on failure
- **FTS injection prevention** -- user input is sanitized before reaching FTS5 queries; special characters are escaped
- **Error sanitization** -- internal database paths and system details are stripped from error responses; clients see structured error types (NOT_FOUND, VALIDATION_FAILED, DATABASE_ERROR, CONFLICT)
- **Body size limits** -- HTTP request bodies are capped at 50 MB via Axum's DefaultBodyLimit
- **Bulk operation limits** -- bulk create endpoints enforce maximum batch sizes to prevent resource exhaustion
- **CORS** -- permissive CORS layer enabled for localhost development workflows
- **Input validation** -- every write path validates title length, content length, namespace format, source values, priority range (1-10), confidence range (0.0-1.0), tag format, tier values, relation types, and ID format
- **Link validation in sync** -- all links are validated (both IDs, relation type, no self-links) before import during sync operations
- **Thread-safe color** -- terminal color detection uses `AtomicBool` for safe concurrent access
- **Local-only HTTP** -- the HTTP server binds to 127.0.0.1 by default; not exposed to the network
- **WAL mode** -- SQLite Write-Ahead Logging for safe concurrent reads during writes

---

## Documentation

| Guide | Audience |
|-------|----------|
| [Migration Guide v0.7](docs/MIGRATION_v0.7.md) | **Upgrading from v0.6.x — read this first** (covers attested-cortex, hooks, transcripts, AGE, permissions, G1 inheritance fix) |
| [What's new in v0.7](docs/whats-new-v07.html) | Visual walk-through of the `attested-cortex` substrates |
| [`attested-cortex` RFC](docs/v0.7/rfc-attested-cortex.md) | Design rationale for the four v0.7 architectural decisions |
| [v0.7 compatibility matrix](docs/v0.7/compatibility-matrix.html) | Per-feature default-vs-opt-in matrix |
| [Installation Guide](docs/INSTALL.md) | Getting it running (includes MCP setup for multiple AI platforms) |
| [User Guide](docs/USER_GUIDE.md) | AI assistant users who want persistent memory |
| [Developer Guide](docs/DEVELOPER_GUIDE.md) | Building on or contributing to ai-memory |
| [Admin Guide](docs/ADMIN_GUIDE.md) | Deploying, monitoring, and troubleshooting |
| [Engineering Standards](docs/ENGINEERING_STANDARDS.md) | Code, test, security, and release standards (authoritative) |
| [AI Developer Workflow](docs/AI_DEVELOPER_WORKFLOW.md) | Step-by-step workflow for AI coding agents contributing to this repo |
| [AI Developer Governance Standard](docs/AI_DEVELOPER_GOVERNANCE.md) | Policy for AI participation: authority, attribution, review, audit |
| [GitHub Pages](https://alphaonedev.github.io/ai-memory-mcp/) | Visual overview with animated diagrams |

---

## License

Copyright 2026 **AlphaOne LLC**.

Licensed under the [Apache License, Version 2.0](LICENSE) (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

> <http://www.apache.org/licenses/LICENSE-2.0>

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
