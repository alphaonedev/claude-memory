# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Required Reading at Session Start (AI agents)

Before proposing any change to this repository, load the following into context:

- [`docs/AI_DEVELOPER_WORKFLOW.md`](docs/AI_DEVELOPER_WORKFLOW.md) — the eight-phase
  workflow every AI session must follow (recall → plan → branch → implement → gates →
  self-review → PR → handoff).
- [`docs/AI_DEVELOPER_GOVERNANCE.md`](docs/AI_DEVELOPER_GOVERNANCE.md) — authority
  classes (Trivial / Standard / Sensitive / Restricted), attribution rules, security
  policy, memory governance, and the hard prohibitions you must never violate.
- [`docs/ENGINEERING_STANDARDS.md`](docs/ENGINEERING_STANDARDS.md) — code, test,
  security, and release standards.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contributor procedures.

### Loading project memory at session start

The mechanical guarantee is the SessionStart hook documented in
[`docs/integrations/claude-code.md`](docs/integrations/claude-code.md).
Install it once; every fresh Claude Code session boots with relevant
memory context already in the system prompt — no model proactivity
required. See the full agent matrix in
[`docs/integrations/README.md`](docs/integrations/README.md).

If the hook is not installed (cold-start fallback), call
`memory_session_start` followed by `memory_recall <task topic>` before
responding. Text directives are best-effort; the hook is the load-bearing
mechanism. See [issue #487](https://github.com/alphaonedev/ai-memory-mcp/issues/487)
for the RCA.

Default namespace for this repo is `ai-memory-mcp`.

Every commit you author must end with a `Co-Authored-By:` trailer naming the model.
Every PR you open must include the **AI involvement** section described in
[`AI_DEVELOPER_WORKFLOW.md` §8.2](docs/AI_DEVELOPER_WORKFLOW.md).

## Build & Test Commands

```bash
cargo build                    # Debug build
cargo build --release          # Release build (thin LTO, stripped)

# All four gates must pass before PR submission:
cargo fmt --check
cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test
cargo audit

# Run a single test
AI_MEMORY_NO_CONFIG=1 cargo test test_name

# Benchmarks
cargo bench --bench recall
```

`AI_MEMORY_NO_CONFIG=1` prevents loading user config which may trigger embedder/LLM initialization during tests.

## Dogfooding release branches

Every `release/v0.6.x.y` branch should be dogfooded by the maintainer for at least 24h before tag-cut so any migration / capability / wire-format regression surfaces in real use, not just CI. The script that does this on this node:

```bash
scripts/dogfood-rebuild.sh
```

What it does (idempotent — safe to re-run after every commit):
1. `cargo build --release`
2. Backs up the live MCP DB to `/tmp/ai-memory-dogfood-test-<ts>.db`
3. Dry-runs migrations against the backup (proves v17→v18→v19 etc. round-trip cleanly on real data)
4. Re-points `/opt/homebrew/bin/ai-memory` → `target/release/ai-memory` (via `brew unlink` + symlink)
5. Lists running MCP processes that need a Claude Code restart to pick up the new binary

What it does NOT do:
- Touch the live DB (migrations only run when an actual ai-memory process opens it on the next MCP restart)
- Kill the running MCP (would self-DOS the in-flight Claude Code session)
- Bump `Cargo.toml` version (that's a tag-cut concern)

Reverting to the brew-managed binary: `brew link --overwrite ai-memory`.

## Reproducing the v0.7.0 recursive-learning primitive

`scripts/reproduce-recursive-learning.sh` is the self-contained end-to-end
demo for the v0.7.0 recursive-learning add-on (issue #655, Tasks 1-4
landed; Tasks 5-8 in flight on `feat/v0.7.0-recursive-learning`). It
builds the release binary, creates a fresh sqlite DB under
`.local-runs/repro-recursive-learning-<timestamp>/` (honoring the
project no-`/tmp` HARD RULE), inserts 3 sample memories, drives
`memory_reflect` over MCP stdio JSON-RPC up to the default depth cap
(3), and demonstrates the refusal at depth=4 with a clearly-formatted
`REFLECTION_DEPTH_EXCEEDED` verdict block. Idempotent (each run uses
a fresh timestamped subdir).

```bash
scripts/reproduce-recursive-learning.sh
# Set REPRO_KEEP_DB=1 to retain the demo DB for inspection after the run.
```

The full conceptual primer lives at `docs/RECURSIVE_LEARNING.md`; the
release-notes intro lives under `docs/v0.7.0/release-notes.md`
§"Substrate-native recursive refinement".

## Architecture

**ai-memory** is a Rust-based persistent memory system exposing three interfaces over a shared SQLite database layer:

1. **MCP Server** (`src/mcp/`) — stdio JSON-RPC 2.0 with 63 tools at full profile (5 default per v0.6.4 `--profile core`) + 2 prompts
2. **HTTP API** (`src/handlers.rs`) — Axum REST server on port 9077, 50 endpoints at `/api/v1/`
3. **CLI** (`src/main.rs`) — clap-based, 40 subcommands with optional `--json` output

All three interfaces share the same database (`src/db.rs`) and validation (`src/validate.rs`) layers. Shared state is `Arc<Mutex<(Connection, PathBuf, ResolvedTtl, bool)>>` — a single SQLite connection protected by a mutex. Lock contention is the bottleneck under concurrent HTTP + MCP load.

### Key Modules

| Module | Role |
|--------|------|
| `main.rs` | CLI parsing, daemon setup (Axum + GC scheduler), command dispatch |
| `mcp.rs` | MCP server: stdin/stdout JSON-RPC loop, tool definitions |
| `db.rs` | All SQLite operations: CRUD, FTS5 queries, recall scoring, GC, schema migrations |
| `handlers.rs` | HTTP request handlers (Axum extractors), error sanitization |
| `models.rs` | Core data structures: Memory (15 fields), MemoryLink, request/response types |
| `validate.rs` | Input validation for all write paths |
| `config.rs` | Feature tier system (keyword/semantic/smart/autonomous), TTL config |
| `reranker.rs` | Hybrid recall: blends semantic (cosine) + keyword (BM25-like FTS5) scores |
| `embeddings.rs` | HuggingFace model loading, vector generation, cosine similarity |
| `hnsw.rs` | In-memory HNSW vector index for approximate nearest-neighbor search |
| `llm.rs` | LLM integration via Ollama: query expansion, auto-tagging, contradiction detection |
| `toon.rs` | TOON format: token-efficient JSON alternative (40-60% smaller) |
| `mine.rs` | Conversation import from Claude/ChatGPT/Slack exports |
| `errors.rs` | ApiError, MemoryError enum, HTTP status mapping |
| `color.rs` | ANSI color output for CLI |

### Data Model

- **Memory**: 15-field struct with id, tier (short/mid/long), namespace, title, content, tags, priority (1-10), confidence (0.0-1.0), source, metadata (JSON), timestamps
- **MemoryLink**: Typed directional relationships (related_to, supersedes, contradicts, derived_from)
- **Tiers**: short (6h TTL), mid (7d TTL), long (permanent). Tier transitions: automatic mid→long via touch at 5 accesses (`PROMOTION_THRESHOLD`); explicit `memory_promote` jumps to long in a single call by default (short→long or mid→long, NOT short→mid→long stepwise). The MCP tool now accepts an optional `target_tier` parameter (`"mid"` or `"long"`) for callers that want to stop at an intermediate tier; omitting it preserves the historical highest-reachable-tier behavior. Downgrades (e.g. mid→short) are never honored — `db::update` enforces tier monotonicity.
- **Feature tiers**: keyword (FTS5 only) → semantic (MiniLM embeddings) → smart (Ollama) → autonomous (cross-encoder reranking)

### Recall Pipeline

Recall is multi-stage and **never read-only** — every recall mutates the database:

1. **FTS5 keyword search** — fuzzy OR query, scored by `fts.rank + priority*0.5 + access_count*0.1 + confidence*2.0 + tier_bonus + recency_factor`
2. **Semantic search** — cosine similarity via HNSW index (or linear scan fallback), threshold >0.2 (relaxed from 0.3 in v0.6.2 Patch 2 after scenario-18 caught a miss at 0.25-0.29 cosine for legitimately-related content)
3. **Adaptive blending** — `final = semantic_weight * cosine + (1 - semantic_weight) * norm_fts`. Semantic weight varies 0.50 (short content ≤500 chars) → 0.15 (long content ≥5000 chars) because embeddings lose information on long text
4. **Touch operations** (atomic) — increment `access_count`, **set `expires_at = now + per-tier-TTL`** (1h short / 1d mid; this is a sliding-window **REPLACEMENT**, not a max-of-old-and-new extend — the create-time 6h short / 7d mid backstop applies only until first access, after which the per-access window takes over), auto-promote mid→long at 5 accesses, increment priority every 10 accesses. `memory_promote` jumps a memory to the highest reachable tier (long) in a single call by default; a future revision may add an optional `target_tier` parameter for stepwise control (mid as an intermediate landing zone).

### Upsert Behavior

Storing a memory with the same `(title, namespace)` updates the existing one. Tier is never downgraded (takes max). Expiry is only cleared if the new memory is `long`-tier.

### Database

SQLite with WAL mode, FTS5 virtual table for full-text search, schema version v7 with automated migrations. Archive table preserves GC'd memories for restoration. FTS is kept in sync via INSERT/DELETE/UPDATE triggers. GC runs every 30 minutes; expired memories are archived before deletion when `archive_on_gc=true` (default).

### Environment Variables

**Precedence (universal).** Every knob in the table below resolves
through the same ladder when more than one source is present:

```
CLI flag  >  AI_MEMORY_* env var  >  config.toml field  >  compiled default
```

CLI flags are clap-parsed; for flags declared with `#[arg(env = "...")]`
clap reads the env var ONLY when the CLI flag is absent. Vars not bound
to a clap flag (most of the table) are read directly from the
appropriate `effective_*` accessor at the point of use. Test-only
vars (`AI_MEMORY_TEST_*`, `AI_MEMORY_AUTO_EXPORT_INJECT_PANIC`) are
inert under production builds.

**Classification.** `secret` = leaks credentials or override authority
if logged or echoed; MUST NOT appear in capabilities, banners, audit
records, or `tracing` output. `config` = operational knob, safe to
echo. `test-only` = honored in test builds; never set in production.

**Surfaces.** `CLI` = `ai-memory <subcommand>`. `daemon` = `ai-memory
serve` (HTTP). `MCP` = `ai-memory mcp` (stdio JSON-RPC). `federation`
= peer-to-peer sync paths. `entrypoint` = `entrypoint.plan-c.sh` boot
script (Docker / Plan C deployments).

| # | Variable | Type | Default | Surface | Class | Notes |
|--|---|---|---|---|---|---|
| 1 | `AI_MEMORY_DB` | path | `ai-memory.db` | CLI/daemon/MCP | config | clap `env=`; `--db` flag wins. Resolved by `effective_db`. |
| 2 | `AI_MEMORY_DB_PASSPHRASE` | string | unset | CLI/daemon/MCP (sqlcipher build) | **secret** | Set by the CLI from `--db-passphrase-file` (mode 0400). Direct caller use leaks via `ps -E`. Never echoed. |
| 3 | `AI_MEMORY_API_KEY` | string | unset | entrypoint (`entrypoint.plan-c.sh`, #845) | **secret** | Injected into the rendered `config.toml` top-level `api_key` field at container boot. Never read from Rust env directly. |
| 4 | `AI_MEMORY_NO_CONFIG` | bool (`1`) | unset | all | config | Skip loading `~/.config/ai-memory/config.toml`. Required for integration tests that bring up isolated state. |
| 5 | `AI_MEMORY_AGENT_ID` | string | synthesized | CLI/MCP (NOT daemon) | config | clap `env=`; `--agent-id` flag wins. See §Agent Identity for full resolution ladder. |
| 6 | `AI_MEMORY_PROFILE` | string | `core` | MCP only | config | clap `env=` on `ai-memory mcp`; `--profile` flag wins. One of `core`/`graph`/`admin`/`power`/`full`/comma list. |
| 7 | `AI_MEMORY_ANONYMIZE` | bool (`1`/`0`) | `false` | CLI/daemon/MCP | config | Overrides `[identity].anonymize_default`. Truthy = synthesize `anonymous:pid-…` fallback instead of `host:…`. |
| 8 | `AI_MEMORY_AUTONOMOUS_HOOKS` | bool (`1`/`0`) | `false` | CLI/daemon/MCP | config | Truthy = fire `auto_tag`+`detect_contradiction` synchronously after every `memory_store`. |
| 9 | `AI_MEMORY_BOOT_ENABLED` | bool (`1`/`0`) | `true` | CLI/daemon/MCP | config | Boot lifecycle primitive (#bootloader). Falsy disables boot-time inventory + index warm-up. |
| 10 | `AI_MEMORY_PERMISSIONS_MODE` | enum (`enforce`/`advisory`/`off`) | `enforce` (v0.7.0 secure default) | CLI/daemon/MCP | config | K3/K9 governance gate. Overrides `[permissions].mode`. Unparseable values warn + fall through. |
| 11 | `AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS` | bool (`1`/`true`/`yes`/`on`) | `false` | daemon | config | H11/#628 SSRF gate. Truthy permits `127.0.0.1` webhook URLs for integration tests. |
| 12 | `AI_MEMORY_OPERATOR_PUBKEY` | base64 ed25519 | falls back to on-disk `operator.key.pub` | CLI/daemon/MCP/governance | **secret-adjacent** (override authority) | Treated as override-authority — anyone who sets it controls rule signing. Lock down host. |
| 13 | `AI_MEMORY_KEY_DIR` | path | platform config dir + `/ai-memory/keys` | CLI/daemon/MCP | config | Override for ed25519 key storage location. Used by H4 `memory_verify` tests. |
| 14 | `AI_MEMORY_LOG_DIR` | path | platform-default (XDG / `/var/log/ai-memory/logs`) | CLI/daemon/MCP | config | Operational log dir override; mirrors `--log-dir` flag. World-writable directories are rejected. |
| 15 | `AI_MEMORY_AUDIT_DIR` | path | platform-default (`audit/` subdir) | CLI/daemon/MCP | config | Audit log dir override; mirrors `--audit-dir` flag. |
| 16 | `AI_MEMORY_SYSTEM_PROMPT_DIR` | path | bundled | `ai-memory install` (CLI) | config | Override for the installed SystemPrompt template directory (`ai-memory install` writes hooks here). |
| 17 | `AI_MEMORY_PRECOMPUTE_FAMILY_EMBEDDINGS` | bool (`1`) | unset | daemon | config | B3 daemon hot-start: truthy precomputes family-prototype embeddings during `serve` startup. |
| 18 | `AI_MEMORY_TOOLS_VERBOSE` | bool (`1`) | unset | MCP | config | Force-on `verbose` for every `memory_capabilities` invocation (operator debug). |
| 19 | `AI_MEMORY_AUTO_CONFIDENCE` | bool (`1`) | `false` | CLI/daemon/MCP | config | Enable auto-confidence calibration on store/touch. |
| 20 | `AI_MEMORY_CONFIDENCE_DECAY` | bool (`1`) | `false` | CLI/daemon/MCP | config | Enable confidence decay sweep. |
| 21 | `AI_MEMORY_CONFIDENCE_SHADOW` | bool (`1`) | `false` | CLI/daemon/MCP | config | PERF-9 shadow-mode confidence pipeline (sample-rate gated, latency observed). |
| 22 | `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE` | float 0.0-1.0 | `0.0` | CLI/daemon/MCP | config | Fraction of touches that pay the shadow calibration cost. |
| 23 | `AI_MEMORY_FED_PEER_ATTESTATION` | string (peer pubkey allowlist marker) | unset | federation | config | Peer-attestation enforcement marker on federated sync. |
| 24 | `AI_MEMORY_FED_TRUST_BODY_AGENT_ID` | bool (`1`) | `false` | federation | config | Trust `body.agent_id` instead of envelope-attributed sender on federated writes. **Loosens** identity gating — set only for fully trusted peers. |
| 25 | `AI_MEMORY_FED_SYNC_TRUST_PEER` | bool (`1`) | `false` | federation | config | Trust peer-supplied sync metadata (counter, timestamps). **Loosens** anti-replay — set only for fully trusted peers. |
| 26 | `AI_MEMORY_AUTO_EXPORT_INJECT_PANIC` | bool (`1`) | unset | hooks (post-reflect) | **test-only** | Forces a panic inside `auto_export` to exercise the recovery path. Production deployments MUST leave unset. |
| 27 | `AI_MEMORY_TEST_POSTGRES_URL` | conn-string | unset | tests (CLI `schema-init`, postgres store) | **test-only** | Points the Postgres backend tests at a live instance. Carries credentials — treat as **secret** when set. |
| 28 | `AI_MEMORY_TEST_AGE_URL` | conn-string | unset | tests (Apache AGE store) | **test-only** | Same shape as `AI_MEMORY_TEST_POSTGRES_URL` for the AGE-backed graph tests. |
| — | `RUST_LOG` | tracing filter | unset (= `info`) | all | config | Standard `tracing-subscriber` filter (e.g. `RUST_LOG=ai_memory=debug`). Not an `AI_MEMORY_*` var — listed for completeness. |

**Regression tests.** Precedence + secret-classification invariants
are pinned by `tests/config_precedence.rs`:

- `test_cli_flag_overrides_env` — `--db /a.db` with `AI_MEMORY_DB=/b.db`
  in env must resolve to `/a.db`. Tests `Cli::parse_from` directly so
  the clap binding is verified end-to-end.
- `test_env_overrides_config` — `AI_MEMORY_DB=/x.db` with
  `config.toml` `db = "/y.db"`; env wins because clap merges env
  into the same flag slot, and `effective_db` treats any non-default
  CLI/env value as explicit operator intent.
- `test_secret_not_in_capabilities` — `AI_MEMORY_DB_PASSPHRASE=mysecret`
  must NOT appear anywhere in the serialised `memory_capabilities`
  JSON (v2 schema). Hardens the no-secret-in-overlay invariant against
  future capability-overlay refactors.

If you add a new env var, update the table above AND extend
`tests/config_precedence.rs` so the invariant is mechanically enforced.

### Agent Identity (NHI) — `metadata.agent_id`

Every stored memory carries `metadata.agent_id` — a best-effort Non-Human Identity
marker. See design discussion on issue #148. **agent_id is a *claimed* identity,
not an *attested* one** — do not use it for security decisions without pairing
with agent registration (Task 1.3, upcoming).

**Resolution precedence (CLI and MCP):**

1. Explicit value from caller (`--agent-id` flag, MCP `agent_id` tool param, or
   `metadata.agent_id` embedded in an MCP store request)
2. `AI_MEMORY_AGENT_ID` environment variable
3. (MCP only) Value captured from `initialize.clientInfo.name` →
   `ai:<client>@<hostname>:pid-<pid>`
4. `host:<hostname>:pid-<pid>-<uuid8>` (stable per-process)
5. `anonymous:pid-<pid>-<uuid8>` (fallback if hostname unavailable)

**HTTP daemon mode** is multi-tenant, so there is no process-level default:

1. `agent_id` field in `POST /api/v1/memories` body
2. `X-Agent-Id` request header
3. Per-request `anonymous:req-<uuid8>` (logged at WARN)

**Validation:** `^[A-Za-z0-9_\-:@./]{1,128}$` — permits prefixed forms
(`ai:`, `host:`, `anonymous:`), `@` scope separator, `/` for future SPIFFE-style
ids. Rejects whitespace, null bytes, control chars, shell metacharacters.

**Immutability:** Once a memory is stored, `metadata.agent_id` is preserved across
update, dedup (UPSERT), MCP `memory_update`, HTTP `PUT /memories/{id}`, import,
sync, and consolidate. Preservation is enforced at both caller layer
(`identity::preserve_agent_id`) and SQL layer (`json_set` CASE clauses in
`db::insert` and `db::insert_if_newer`).

**Filter by agent_id:** `list` and `search` accept `--agent-id <id>` (CLI), the
`agent_id` property (MCP tool), or `?agent_id=<id>` (HTTP query param).

**Special metadata keys produced by the system** (do not overwrite):

- `imported_from_agent_id` — original claim preserved when `ai-memory import`
  restamps agent_id with caller's id (absent when `--trust-source` is passed)
- `consolidated_from_agents` — array of source authors, preserved on
  `memory_consolidate` (the consolidator's id becomes `agent_id`)
- `mined_from` — source format tag (`claude` / `chatgpt` / `slack`) stamped by
  `ai-memory mine` alongside the caller's `agent_id`

**Defaults that leak:** The fallback `host:<hostname>:pid-…` exposes hostname and
PID. When writing memories to a shared or upstream database, set `--agent-id` or
`AI_MEMORY_AGENT_ID` to something scrubbed (an opaque identifier, `alice`, etc.).
Tracking issue: #198.

## Adding New Functionality

**New CLI command**: Add variant to `Command` enum → define `Args` struct → add dispatch case in `main()` → implement `cmd_*` handler taking `&Path` (db) + args.

**New MCP tool**: Add JSON definition in `tool_definitions()` → add match arm in the dispatch block → implement handler taking `&Connection` + params → return `Result<Value>`.

**New HTTP endpoint**: Add route in `main.rs` router → implement handler in `handlers.rs` using `Db` extractor.

## Code Style

- `cargo fmt` required. All code formatted with rustfmt.
- Zero warnings under `clippy::pedantic`.
- Copyright header on all source files: `// Copyright 2026 AlphaOne LLC` + `// SPDX-License-Identifier: Apache-2.0`
- PRs target `develop` branch, not `main`. `main` is production releases only.
- Commit format: `<type>: <summary>` (feat, fix, docs, style, refactor, test, chore, perf)

## Prime directive (operator-set, 2026-05-17)

> This is a **prime directive** — it overrides any general-purpose
> framing of "non-blocking", "trend-line", or "surface-level" issues.
> It applies to every agent that touches this repository.

**The rule.** If you find or identify an issue, OPEN AN ISSUE,
TRACK THE ISSUE, FIX THE ISSUE. Every issue gets fixed. That is
the standard.

**No surface-level dismissals.** There is no such thing as a
"surface-level" issue. Do not classify findings as "non-blocking",
"docs-drift", "trend-line", "MCP-coverage-gap", or any framing
that would let the issue rot in a queue. Every gap is a defect.
Every defect is fixed.

**World-class only.** We are driving toward perfection. The
ai-memory codebase is now substantial (70 MCP tools, 50 HTTP
endpoints, 40 CLI subcommands, tens of thousands of lines of
Rust); the architectural North Star is long-term code-base
manageability so the codebase lasts for a very long time.

**Mechanics.**
- Discovery → tracker entry → fix → close is one non-divisible
  workflow. The discoverer is responsible for all three steps
  OR for explicitly handing off each step to a named queue/PR
  with a tracker reference.
- Every `auto-filed-by-agent` issue MUST have a "Proposed fix"
  section with concrete file paths + line counts.
- For each test-campaign phase: a separate "findings" memory
  enumerating EVERY anomaly. All findings reach the issue
  tracker before the next phase starts.
- Documentation drift between code behavior and docstrings is a
  real defect. File AND fix the docs (or fix the behavior so it
  matches the docs).
- The phrases "non-blocking", "trend-line gap",
  "surface-level", "P2/P3 follow-up", and "vN+1 polish" are
  banned in finding writeups.

**Testing-loop discipline (operator addendum 2026-05-18 pm).**
During ANY testing session (NHI playbook, A2A campaigns,
integration tests, chaos probes, security audits, manual smoke
tests, anything that exercises the system):

1. EVERY issue surfaced during testing — even ones the test
   framework would call "informational", "expected drift",
   "warning", or "minor" — MUST be filed as a GitHub issue at
   the moment of discovery.
2. The issue must be documented with root cause one-liner,
   evidence (file:line or test output), reproduction, proposed
   fix size, related memory ids.
3. The issue must be tracked through fix → retest → re-check
   → close, in the CURRENT release (v0.7.0 in this campaign).
   No deferral to a future release is permitted unless the
   operator explicitly approves the defer in writing.
4. The fix must be retested against the same scenario that
   surfaced it.
5. The fix must be re-CHECKED via a fresh probe that didn't
   run the original test path, to confirm the fix doesn't
   merely make the test pass while leaving the underlying
   defect.
6. Iteration continues until 100% remediation. No "close as
   fixed" without the retest + re-check both green.
7. Audit trail is mandatory: GH issue body links to ai-memory
   evidence; ai-memory evidence links to GH issue id; commit
   messages reference both; campaign docs
   (`docs/v0.7.0/test-campaign-*/`) cite both.

Banned mid-testing behaviors:
- Deferring a found issue to "after the campaign" — file NOW.
- Closing the campaign with open findings unresolved — every
  found issue must be resolved (fixed + retested + closed)
  before the campaign verdict can mint as SHIP.
- Bundling many findings into one issue — each finding gets
  its own issue so each gets its own audit trail.
- Counting "blocked tests" or "out-of-scope" as resolution —
  if a test couldn't run, that's a test-infra defect. File +
  fix it.

Recompile + batch retest discipline (operator addendum 2026-05-18 pm):
- After a batch of fixes lands, recompile ONCE (`cargo build
  --release`), then run a BATCH retest of every issue the
  batch was meant to fix — not one-issue-at-a-time piecemeal
  retesting mid-stream.
- The MCP session running while you fix the binary keeps the
  OLD binary loaded in memory; retest the NEW binary via CLI
  (`ai-memory <cmd>`), via raw MCP probes (`printf JSONRPC |
  ai-memory mcp ...`), or by spawning fresh MCP sub-processes.
  Operator restart is only needed to UPGRADE their live
  session, not for AI NHI to validate the fix.

**Three-wave refactor mandate (pre-v0.7.0 release).** Three
sequenced waves of refactor + review work must complete BEFORE
v0.7.0 ships. None is skippable. All three are pre-release.
See tasks #16 → #17 → #18 → #19 (FINAL MISSION docs+pages
drift) for the current execution state.

**Six strategic high-level lanes (operator-corrected 2026-05-17 pm-v7).**
The canonical lane index lives in memory
`f970d6f6-7bde-4d6b-9a53-500734961e04` (namespace
`_v070_strategic_tracking`; supersedes `ab6aedf5-...`, `c413ac25-...`,
`afd38b34-...`, `b1109500-...`). Operator correction memory:
`338278f5-1d42-4e95-88c5-84d5fc3b1f53` (IP swap + Docker IronClaw +
E1/E2 withdrawal). Every session boot should load both.

| # | Lane | Task |
|---|------|------|
| 1 | Bugs/issues — fix everything | #22 |
| 2 | Code line coverage | #23 |
| 3 | Full-spectrum testing (NHI + A2A 100% regression + net-new + DO hive) | #24 |
| 4 | Code refactoring (3-wave mandate) | #25 |
| 5 | Documentation drift — 100% remediation | #26 |
| 6 | GitHub Pages website redesign (3 audiences + 3 AI-NHI brass tacks) | #27 → issue #832 |

Lane 3 testing tracks (corrected per operator 2026-05-17 pm-v7,
memory `338278f5-1d42-4e95-88c5-84d5fc3b1f53`):
- Track A: NHI playbook P0-P11 + verdict — #7 (P0-P2 done)
- Track B: A2A 4-domain IronClaw **in Docker** on this node (192.168.50.100), Grok 4.3 via xAI API, 100% regression + net-new — #8
- Track C: Postgres + Apache AGE on Linux node **192.168.1.50** (NOT .50.1 — that was earlier-session drift) — #9
- Track D: Cross-node integration (.100 ↔ **.1.50**) — #10
- **Track E1 (DO CPU agent hive) — WITHDRAWN from active scope.** Pursuit requires explicit human biologic operator approval. Issue #833 / task #28 frozen.
- **Track E2 (AWS GPU burst hive) — WITHDRAWN from active scope.** Same gating as E1. Issue #834 / task #29 frozen.

**Current blocker for Track C/D:** 192.168.50.100 cannot reach
192.168.1.50 (different subnets; ping + 22 + 5432 unreachable).
Operator action needed: route / VPN / bridge between subnets.

All 6 lanes pre-release. None skippable. Cross-lane discipline: Lane 1 is
the meta-lane (every other lane's findings land there); Lane 3
re-runs on the Wave-3 post-refactor binary; Lane 5 final sweep is
post-refactor; Lane 6 can run in parallel with Lane 4; Track E
captures feed Lane 6 case-study content.

**Provenance.** Live memory `f1dca8fa-6c33-4139-b0b5-389cca45b921`
in namespace `global/policies` is the canonical version of this
directive (testing-loop addendum 2026-05-18 pm) and supersedes
`5d703efe-273b-4c84-8f40-ceb97b55d71e` which superseded
`71ecce23-611b-4984-962d-d37c4309f261`.

## v0.7.0 release gate (operator-set 2026-05-17 pm-v5)

**AI NHI is 100% autonomous and makes ALL decisions EXCEPT the
v0.7.0 release tag cut.** The release gate is **100% GREEN TESTS**.
The full checklist lives in issue #836 (`v0.7.0 RELEASE GATE`) and
the lane-index memory. Tier summary:

1. Every CI workflow on `release/v0.7.0` HEAD passes.
2. Every queued `auto-filed-by-agent` issue resolved (no open
   blocker).
3. Lane 3 full-spectrum testing: Tracks A-E2 all PASS, final
   verdict memory minted with status = SHIP.
4. Lane 4 refactor Waves 1-3 complete with green re-validation
   on the refactored binary.
5. Lane 2 coverage floors met + raised on hot-path modules.
6. Lane 5 docs drift 100% remediated.
7. Lane 6 website redesign + 3 audience pages + 3 AI-NHI essays
   + #835 clean A2A test pages all live.
8. Final binary validation (24h dogfood, cargo audit clean, all
   four gates clean on fresh checkout, release-notes + CHANGELOG
   complete).

When all 6 tiers are green, the agent posts a SHIP-RECOMMENDED
comment on #836 + a high-priority memory in
`_v070_release_gate`, then **stops**. Operator reviews + cuts
the tag. Banned: surface-level exemptions, "close enough"
quoting, bypassing via --no-verify / force-push / out-of-band
merges, cutting the tag without explicit operator approval.

## Commit & push policy (project override of global default)

> This policy **overrides** Claude Code's global default ("NEVER commit unless
> the user explicitly asks"). Two days of uncommitted work is bad engineering;
> the loss of work on a local-only edit graph is a real failure mode. The
> override below distinguishes **committing** (local, recoverable, low blast
> radius) from **pushing** (shared-system write, higher blast radius) so each
> can have its own discipline.

**Commit autonomously when work crosses a logical checkpoint.** No need to
ask first. Specifically commit when ANY of these become true:

- A feature lands and all four gates (`cargo fmt --check`, clippy `-D warnings
  -D clippy::all -D clippy::pedantic`, `AI_MEMORY_NO_CONFIG=1 cargo test`,
  `cargo audit`) are green.
- A fix lands and the regression test that pins it passes.
- A patch series completes (e.g., L1-L15 patch batch, a 4-lane audit fix
  series, a multi-issue fold-in).
- A doc-only change is self-contained and the surrounding sections are not
  in mid-rewrite (`grep -n "TODO\|XXX\|TBD" <file>` in your scope is clean).
- An hour of focused work has accumulated and the working tree is at a clean
  point (gates pass).
- The agent is about to start a substantial in-flight task that could
  conflict with the current dirty state (commit-before-pivot).

**Group commits by intent.** Don't dump the whole working tree into one
commit. Reasonable groupings (in this repo's recent ship history):

- `feat(...)` per issue or per feature
- `fix(...)` per bug or per finding (#318 / #355 / L14 / G5 / etc.)
- `chore(deps)` for `Cargo.toml` + `Cargo.lock` together
- `chore(tests)` for test-scaffold updates that follow a struct-field
  addition
- `docs(...)` per doc surface (CHANGELOG separate from ROADMAP separate
  from release-notes when they touch different audiences)
- `infra(...)` for Dockerfile + entrypoint changes

**Stage explicit paths**, not `git add -A` or `git add .`. Prevents accidental
inclusion of `.env`, credentials, large binaries, or work-in-progress
sibling files the user didn't intend to land yet. The bash command this
file already documents (`git add <specific>` then `git commit`) holds.

**Use a HEREDOC for multi-line commit messages.** Every commit ends with
the `Co-Authored-By:` trailer naming the model (matches the discipline in
the existing AI Developer Workflow doc).

### When to ASK before committing

Ask the operator first when ANY of these apply:

- Mass-deletion (more than ~5 tracked files about to be `git rm`-ed) that
  isn't the result of an explicit "delete X" instruction.
- The diff touches a file the operator has been actively hand-editing
  in the same session (concurrent-edit risk; check `git diff` against
  the most recent system-reminder of the file).
- The commit would land secrets-looking content (anything matching
  `password|secret|key|token|cred` patterns in the diff that isn't a
  test fixture or doc).
- The commit would re-introduce reverted code (check `git log -p`
  against the relevant region).
- The cert/CI signal is currently RED and the commit doesn't itself
  close the failure.

### Pushing — separate, higher bar

**Pushing requires explicit operator authorization.** Each push to a shared
remote branch is a write to an external system that may trigger CI, sync
to a PR diff, or notify reviewers. Different blast radius from local
commits.

**Operator-set scope (2026-05-17 pm-v6, memory `eb44c467-a42e-4f37-8a80-34151fe20fc3`):**
The AI NHI agent is APPROVED to push directly to `release/v0.7.0`
as part of normal autonomous work — fixing auto-filed-by-agent
issues, persisting test-campaign results, docs updates, site
updates, refactor work. The release tag cut + release publish
remain operator-gated per the 8-tier release gate (issue #836).

Default discipline:

- Local commits accumulate freely under the rules above.
- Push to `origin/<topic-branch>` (e.g., `round-2-fixes`,
  `feat/...`) and to `origin/release/v0.7.0` are PRE-APPROVED for
  the current v0.7.0 campaign per the operator directive above.
- **Never force-push** without explicit operator authorization, ever.
- **Never push to `main` directly**, even with authorization to push to
  other branches. `main` is production-tag-only.
- **Never push to `develop`** without operator authorization specific to
  `develop`, since `develop` is the integration branch.
- **Cutting the v0.7.0 release tag, publishing to crates.io / GHCR /
  Homebrew / COPR, or merging `release/v0.7.0` → `main` remain
  operator-gated** (require explicit per-action authorization, fire only
  when the 8-tier release gate verifies 100% green).
- Cost-spending actions (DO provisioning #833, AWS GPU burst #834) stay
  operator-$-gated.

### Sync discipline (operator emphasis 2026-05-17 pm-v6: do not lose context)

Per operator: "keep everything in sync — do not lose context on keeping
everything in sync". This is a first-class discipline. Concretely:

- **Lane index ↔ CLAUDE.md ↔ live issues** must all agree. Every
  material state change supersedes the lane-index memory AND updates
  CLAUDE.md AND fires a task/issue update.
- **Memory supersession chains** retain `related_to` (or future
  `supersedes`) links so audit is reconstructable.
- **Commit messages reference issue numbers + memory ids** — the commit
  log itself becomes a navigable history.
- **Task list updates fire on every status change** — no stale
  "in_progress" rows.
- **PR descriptions point at the issues + memories** — the PR is also
  a navigable index, not a stub.

Trailing discipline on every round: update memory → update CLAUDE.md →
update tasks → commit → push → verify all four are aligned before the
next change.

### Rationale

This policy is the project's response to two empirical failure modes:

1. **The default-NEVER-commit rule** produced 80-file working trees with
   ~7,000 lines of uncommitted code after multi-day sessions, where a
   power loss or container crash would have lost the work. That is
   unacceptable engineering.
2. **A blanket "always push" policy** would be reckless — pushing kicks
   off CI, lands diffs on open PRs, and notifies reviewers. The separation
   above lets the agent be safe (commit often) while keeping high-blast-
   radius actions (push, force-push, push-to-main) under operator
   control.

The default-flexible-commit / explicit-push split is the cleaner discipline.

## No agent-created files under /tmp, /var/tmp, /private/tmp, or any tmpfs (project hard rule)

> This is a **project hard rule**, not a preference. It overrides any
> tool, shell, or library default that would land scratch files on a
> tmpfs path. It applies to every agent that touches this repository.

**The rule.** Agents working in this repository MUST NOT create files
under any of the following paths, ever:

- `/tmp/...`
- `/var/tmp/...`
- `/private/tmp/...` (the macOS realpath of `/tmp`)
- any other tmpfs-backed path the host exposes

This covers, at minimum: bash one-liner output redirects (`> /tmp/log`),
`heredoc` write-throughs, log captures, `script(1)` typescripts,
container-test artifacts, capability JSON dumps, ad-hoc fixtures,
benchmark output, dogfood-rebuild backup files, and any
`mktemp`/`mktempfile` call where the path is not explicitly overridden
to a project-local location. The rule applies to files that the agent
itself creates; it does NOT apply to files OS tooling creates beneath
the agent (e.g., compiler `/var/folders/...` scratch, the Claude Code
harness's own session cache).

**Allowed scratch location.** All agent-created scratch lives under:

```
/Users/fate/v07/v07-fixes/.local-runs/
```

This directory is gitignored (see `.gitignore`). It is the canonical
home for: log captures from background `cargo` runs, container-test
output dumps, ad-hoc verification scripts, throwaway fixture JSON,
benchmark roll-ups, and similar transient artifacts. Sub-organize
freely (`.local-runs/r8-cert/`, `.local-runs/2026-05-12/`, etc.) —
the directory has no enforced internal structure.

If a tool or third-party script defaults to `/tmp`, pass it an
explicit `--output-dir` / `TMPDIR=$PWD/.local-runs` / equivalent.
If it has no such override, write the output to a project-local
path first and post-process it instead.

**Why this is a hard rule.** During the v0.7.0 cert sequence
(2026-05-11/05-12), accumulated agent scratch on `/private/tmp`
across multiple agents (~30+ logs/scripts/typescripts) contributed to
a full-disk ENOSPC failure that halted in-flight work, forced a
`colima delete -f` to recover, and lost the Plan C container fleet.
The root cause was not any single file — it was the absence of an
enforced project-local scratch convention. This rule closes that
gap. Future agents inherit the convention by reading this file at
session start.

**Discipline.** Zero strikes from here forward. A single violation
is grounds for the agent to self-revert the offending command, move
the file under `.local-runs/`, and update its working memory with
the redirect so the mistake doesn't repeat in-session. The operator
will be informed if a violation occurs so the convention can be
hardened further (e.g., a pre-tool-use hook).

**Cleanup.** `.local-runs/` is intentionally NOT auto-cleaned. Each
agent is expected to delete its own scratch when a task finishes
green and the artifacts are no longer needed for the handoff memory.
A long-lived `.local-runs/` is a smell — flag it in the handoff.
