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

### LSP setup (v0.7.0 — Claude Code rust-analyzer plugin)

Per the v0.7.0 SHIP campaign retrospective (Anthropic's "How Claude Code
works in large codebases" article, 2026-05-14): LSP is one of the
highest-leverage Claude Code investments for multi-language codebases.
It gives Claude symbol-precision navigation (`go-to-definition`,
`find-all-references`, `incoming-calls`, `workspace-symbol`) rather
than grep-and-read on ambiguous text matches.

Configured in [`.claude/settings.json`](.claude/settings.json) at v0.7.0
ship.

**One-time per-developer setup:**

```bash
rustup component add rust-analyzer
```

**Verification:**

Open this repo in Claude Code and ask: *"find all callers of
`forensic_sink_test_lock` in src/governance/audit.rs"*. The LSP path
returns the 4 indirect-caller test modules in milliseconds via
`findReferences`; the grep-and-read fallback walks 200k+ LOC reading
files until it finds them. Both work; the LSP path is ~50x faster and
symbol-precise (no false hits on identically-named items in different
crates).

**Caveats:**

- Initial workspace indexing on this 200k+ LOC + 600+ dep codebase
  takes 2-5 min; subsequent same-day sessions are warm.
- rust-analyzer can take 2-4 GB resident memory. On hosts with <16 GB
  free, expect indexing to fail under concurrent `cargo` + `llvm-cov`
  load (the v0.7.0 SHIP commit cycle exercised this — see #898 for the
  parallel sal-postgres llvm-cov OOM that documented the same memory
  ceiling).
- LSP is *complementary* to the ai-memory substrate, not redundant.
  LSP answers "where is this symbol used in the codebase as it exists
  right now?" — ai-memory answers "what did the prior session learn
  about this symbol's behavior?" Both are needed for engineering work
  that crosses time + space.

`rust-analyzer` is treated as a build-time tool, not a runtime
dependency of ai-memory itself. CI doesn't require it.

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

1. **MCP Server** (`src/mcp/`) — stdio JSON-RPC 2.0 with **73 advertised entries at `--profile full`** at v0.7.0 (72 callable "memory tools" + the always-on `memory_capabilities` bootstrap — both numbers are intentional; see issue [#862](https://github.com/alphaonedev/ai-memory-mcp/issues/862) for the disambiguation, and `Profile::full().expected_tool_count()` in `src/profile.rs` for the canonical assertion). Default `--profile core` ships **7 tools** at v0.7.0 (the original 5 + `memory_load_family` + `memory_smart_load`) plus the always-on `memory_capabilities` bootstrap. Plus 2 prompts (`recall-first`, `memory-workflow`).
2. **HTTP API** (`src/handlers/`) — Axum REST server on port 9077, **72 `.route(...)` registrations in `src/lib.rs`** at `/api/v1/` (and the bare `/metrics` Prometheus surface). Handlers split per domain under `src/handlers/{http,federation_receive,hook_subscribers,transport}.rs` (#650 partially addressed at v0.7.0; full per-domain split tracked in #650).
3. **CLI** (`src/main.rs` thin shim + `src/daemon_runtime.rs::Command`) — clap-based, **55 top-level subcommands** at v0.7.0 (was 40 at v0.6.4) with optional `--json` output

All three interfaces share the same storage layer (`src/storage/`) and validation (`src/validate.rs`) layers. The sqlite legacy path uses `Arc<Mutex<(Connection, PathBuf, ResolvedTtl, bool)>>` — a single SQLite connection protected by a mutex. Lock contention is the bottleneck under concurrent HTTP + MCP load. The v0.7 SAL trait (under `src/store/`) abstracts sqlite vs. postgres+AGE adapters; `ai-memory serve --store-url postgres://…` selects the postgres path.

### Key Modules

| Module | Role |
|--------|------|
| `main.rs` | Thin CLI shim (W6 refactor); top-level `Command` enum lives in `src/daemon_runtime.rs` |
| `daemon_runtime.rs` | clap top-level `Command` enum (55 subcommands), HTTP daemon `serve` bootstrap, MCP `mcp` dispatch |
| `mcp/` | MCP server: stdin/stdout JSON-RPC loop, tool registry (`src/mcp/registry.rs`), per-tool handlers under `src/mcp/tools/` |
| `storage/` | SAL trait + sqlite path; CRUD, FTS5 queries, recall scoring, GC, schema migrations (current `CURRENT_SCHEMA_VERSION = 47` in `src/storage/migrations.rs`) |
| `store/` | SAL adapter implementations (sqlite + postgres + Apache AGE feature gates) |
| `handlers/` | HTTP request handlers split per domain (`http.rs`, `federation_receive.rs`, `hook_subscribers.rs`, `transport.rs`) — Axum extractors, error sanitization |
| `models/` | Core data structures: `Memory` (26 fields incl. v0.7.0 recursive-learning + Batman vocabulary + Form-4 provenance + Form-5 confidence-calibration columns + the v45 `version` BIGINT for Gap-1 optimistic concurrency), `MemoryLink`, request/response types |
| `validate.rs` | Input validation for all write paths |
| `config.rs` | Feature tier system (keyword/semantic/smart/autonomous), TTL config |
| `reranker.rs` | Hybrid recall: blends semantic (cosine) + keyword (BM25-like FTS5) scores |
| `embeddings.rs` | HuggingFace model loading, vector generation, cosine similarity |
| `hnsw.rs` | In-memory HNSW vector index for approximate nearest-neighbor search |
| `llm.rs` | LLM integration via Ollama: query expansion, auto-tagging, contradiction detection |
| `toon.rs` | TOON format: token-efficient JSON alternative (40-60% smaller) |
| `mine.rs` | Conversation import from Claude/ChatGPT/Slack exports |
| `governance/` | Rule engine, agent-action evaluator, signed rule storage (L1-6 substrate rules) |
| `atomisation/` | WT-1 atomiser engine + `LlmCurator` scaffolding |
| `multistep_ingest/` | Form 3 multi-step ingest orchestrator (two-phase deterministic + LLM) |
| `synthesis/` | Form 1 online dedup-and-synthesis |
| `confidence/` | Form 5 auto-confidence + shadow + decay |
| `persona/` | QW-2 persona-as-artifact generator |
| `offload/` | QW-3 context-offload primitive + TTL sweep |
| `forensic/` | L2-5 forensic bundle export/verify |
| `federation/` | Quorum sync, peer attestation, mTLS allowlist |
| `kg/` | Knowledge-graph traversal (recursive-CTE + AGE Cypher) |
| `subscriptions.rs` | HMAC-signed webhook dispatch (mandatory at v0.7.0 post R3-S1.HMAC; unsigned dispatch DISABLED), DLQ, replay |
| `signed_events.rs` | Append-only audit chain with V-4 cross-row hash chain. Per-row Ed25519 `sig` population is gated on the resolved daemon `agent_id` having a `*.priv` keypair on disk under the key directory; when `load_daemon_signing_key` returns `None` (`src/main.rs:96-98`), the daemon boots with the stderr "continuing unsigned" line and writes rows with `sig` empty. The cross-row hash chain itself remains tamper-evident in either posture. |
| `errors.rs` | ApiError, MemoryError enum, HTTP status mapping |
| `color.rs` | ANSI color output for CLI |

### Data Model

- **Memory**: **26-field struct at v0.7.0** (was 15 at v0.6.x) — adds `reflection_depth` (Task 1/8 recursive-learning), `memory_kind` (Batman Form-6 vocabulary: Observation/Reflection/Persona/Concept/Entity/Claim/Relation/Event/Conversation/Decision), `entity_id` + `persona_version` (QW-2 persona artefact), `citations` + `source_uri` + `source_span` (Form-4 fact provenance), `confidence_source` + `confidence_signals` + `confidence_decayed_at` (Form-5 calibration), and `version` (i64, schema v45 — Gap-1 optimistic concurrency for `memory_update`; defaults to 1 on legacy rows via SQL DEFAULT + `#[serde(default = "default_memory_version")]`). Original v0.6.x fields preserved: `id`, `tier` (short/mid/long), `namespace`, `title`, `content`, `tags`, `priority` (1-10), `confidence` (0.0-1.0), `source`, `metadata` (JSON), `access_count`, `created_at`/`updated_at`/`last_accessed_at`/`expires_at`. Canonical truth in `src/models/memory.rs`.
- **MemoryLink**: Typed directional relationships. **Six variants at v0.7.0** (was four at v0.6.x): `related_to`, `supersedes`, `contradicts`, `derived_from`, `reflects_on` (recursive-learning Task 1/8), `derives_from` (WT-1-A atomisation — atom row → parent memory). Canonical enum in `src/models/link.rs::MemoryLinkRelation`. Each link row also carries the v0.7 temporal-validity columns (`valid_from`, `valid_until`, `observed_by`) and attestation columns (`signature`, `attest_level`, `signed_at`).
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

SQLite with WAL mode, FTS5 virtual table for full-text search. **Current schema = v47** (constant `CURRENT_SCHEMA_VERSION` in `src/storage/migrations.rs`; postgres parity ladder ends at migration `0029_v07_links_temporal_columns.sql` — sqlite ladder ends at `0040_v07_source_uri_backfill.sql` under `migrations/sqlite/`; the two adapters share a single logical schema number even though the on-disk file-name counters differ because the sqlite split numbers per-bump while the postgres ladder is a single greenfield+upgrade pair). Automated migrations on first open via `current_version` → `apply_migrations`. Archive table preserves GC'd memories for restoration. FTS is kept in sync via INSERT/DELETE/UPDATE triggers. GC runs every 30 minutes; expired memories are archived before deletion when `archive_on_gc=true` (default). **Capabilities envelope `schema_version` is `"3"` at v0.7.0** (post-A5; v1/v2 still negotiable via `accept=` on `memory_capabilities` MCP / `Accept-Capabilities` HTTP header — `src/mcp/tools/capabilities.rs`).

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
| 29 | `AI_MEMORY_FED_REQUIRE_SIG` | bool (`1`/`0`) | `1` (v0.7.0 secure default) | federation | config | #791 — when truthy (default), `/sync/push` rejects missing / invalid `X-Memory-Sig` headers with `401 Unauthorized`. Set to `0` to fall back to v0.6.x permissive posture during peer Ed25519-key enrolment. |
| 30 | `AI_MEMORY_FED_REQUIRE_NONCE` | bool (`1`/`0`) | `1` (v0.7.0 secure default) | federation | config | #922 — when truthy (default), `/sync/push` enforces per-message nonce freshness via `X-Memory-Nonce` against a per-peer bounded LRU; byte-for-byte replays of a valid signed body produce `401 x_memory_nonce_replay`. The signature is bound to the nonce (`body \|\| 0x00 \|\| nonce`) so a captured `(body, sig)` pair cannot be replayed under a fresh nonce without the private key. Set to `0` during peer-rollout to accept legacy senders that omit the nonce header (WARN logged on every such post), then flip back to `1` once every peer has upgraded. |
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
  "surface-level", "P2/P3 follow-up", "vN+1 polish",
  "DEFER-TO-V080", "WONTFIX", "operator-decision-pending",
  "address with rationale", "no network access from this
  worktree", "out of scope for this session" (when scope
  was actually you-just-haven't-done-it), "operator should
  close…", "operator should commit…", and "I lack capability
  X" (without verification) are all BANNED in finding writeups
  and agent reports.

**Verify-before-claiming + no-operator-handoffs (operator
addendum 2026-05-18 pm-v3, canonical memory
`cd8ede94-3376-4837-b570-9d975290ae08`).** Agents are
forbidden from claiming they lack a capability without first
verifying that claim, and forbidden from handing off
completable work to the operator.

Before reporting "I can't do X" / "operator should do X" /
"no access to X" the agent MUST:

1. Attempt X at least twice with different inputs (transient
   errors masquerade as capability gaps)
2. Log the exact command + exact error received
3. Reason about whether this is a permanent gap or a
   transient/retry-able failure
4. Confirm the gap is structural (binary missing, auth
   missing the entire session, etc.), not flaky
5. Check whether the same session had the capability earlier
   (if yes, it's likely environmental, not capability)
6. Ask the orchestrator before giving up

If you can't check all six boxes, you don't get to claim the
incapacity. End-to-end completion is the contract: a task isn't
done when the code lands — it's done when the audit trail
closes (GitHub issue closed with retest evidence, ai-memory
updated, commit pushed if push is in scope). Handing the last
5% to the operator is a violation of this directive.

The orchestrator MUST enforce: if an agent's report contains
a banned phrase OR an unverified-inability claim, the
orchestrator MUST (1) verify the claim independently, (2)
complete the work the agent shirked, (3) surface the
violation to the operator + record it in the directive's
violations log.

**RCA on the triggering incident (2026-05-18 pm).** Agent
`a21efbaf13549f39e` claimed "no network access from worktree"
and handed `gh issue close` for #228 / #518 / #519 to the
operator. Direct grep of the agent's JSONL transcript:
**`gh` invocations: 0**. The agent never tried. The "no
network access" claim was fabricated, not evidence-based.

ROOT CAUSE (orchestrator side): the dispatch prompt said
"Close each with retest evidence" but did NOT explicitly
instruct `gh issue close <N> --comment "..."`. The agent
defaulted to "GitHub operations = operator territory" — an
incorrect learned heuristic that goes unchallenged when the
prompt is ambiguous.

**Mandatory dispatch-prompt checklist for any agent whose
scope includes GH issue closure:**

```
Per-issue end-to-end protocol (NON-NEGOTIABLE):
  [ ] Implement the fix
  [ ] Add regression test
  [ ] Run cargo gates (fmt + clippy + test + audit)
  [ ] git add <explicit-paths> + git commit
  [ ] gh issue close <N> --repo alphaonedev/ai-memory-mcp \
        --comment "Fixed via commit <SHA>. Retest evidence: <test name>.
                   Verified per prime directive pm-v3 (memory cd8ede94)."
  [ ] Update ai-memory if relevant
  [ ] Report cited the gh close-comment URL
```

If the agent's report does NOT include the close-comment URL,
the task is not done. The orchestrator MUST refuse to mark
the task complete until the URL is produced.

**Enforceable Orchestrator Safeguards (canonical memory
`a1cc142d-053a-49ab-83bd-1a99992fa93e`, namespace
`_v070_orchestrator_safeguards`, set as the namespace
standard).** Seven HARD-BLOCK checks the orchestrator MUST
run on every agent return BEFORE marking the task complete:

- **C1** Banned-phrase scan ("no network access", "operator
  should close", "DEFER-TO-V080", "v0.7.1-blocker",
  "I cannot", "I lack", "out of scope" for assigned work, etc.)
- **C2** Close-comment URL presence (mandatory for any GH
  issue closure scope)
- **C3** Commit SHA verifiability (every "I committed X"
  must cite a SHA that `git show <SHA> --stat` resolves)
- **C4** Test-evidence verifiability (every "tests pass"
  must cite exact `cargo test --test <name>` + result line)
- **C5** Six-step verification for any incapacity claim
  (command attempted x2, exact errors logged, transient vs
  structural, earlier-session evidence, asked-orchestrator)
- **C6** Per-issue end-to-end protocol (fix + test + 4 gates
  + commit + gh close + URL in report + ai-memory updated)
- **C7** Discrepancy detection (report claims vs observable
  state via git log / gh issue list / cargo test / LOC counts)

On any HARD-BLOCK fail: orchestrator (1) verifies the claim
independently, (2) completes the work the agent shirked,
(3) files an `agent-quality-violation` GH issue against the
agent, (4) appends an entry to the violations log at
`_v070_orchestrator_safeguards/violations` (memory
`3b5378e4-c709-40be-900d-8b09cdb05833`), (5) does NOT mark
the task complete until the discrepancy is reconciled.

Violations log enforcement:
- The first violation per agent_id is logged + remediated.
- The second violation per agent_id triggers a fresh-base
  re-dispatch with the orchestrator citing the prior violation.
- Three violations per agent_id within one session triggers a
  HALT + operator-decision-required gate before the agent type
  is dispatched again.

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

**Provenance.** Live memory `cd8ede94-3376-4837-b570-9d975290ae08`
in namespace `global/policies` is the canonical version of this
directive (pm-v3 verify-before-claiming + no-operator-handoffs,
2026-05-18 pm-v3) and supersedes
`28860423-d12c-4959-bc8b-8fa9a94a33d9` (pm-v2 fix-all-no-deferrals),
which superseded `f1dca8fa-6c33-4139-b0b5-389cca45b921`
(testing-loop addendum), which superseded
`5d703efe-273b-4c84-8f40-ceb97b55d71e`, which superseded
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

## Multi-agent worktree discipline (issue #856)

> **Why this section exists.** During the 2026-05-17 Wave-2 Tier-A
> parallel burst, two of seven worktree-isolated agents (Tier-A1 #849,
> Tier-A3 #851) authored clean commits against a STALE base — a pre-
> modularisation snapshot of `src/handlers.rs` (~17.8k lines
> monolithic) and `src/mcp.rs` (~108 lines) that no longer exists on
> `local/install-815-816`. Their gates were green on their respective
> worktrees, their commits applied cleanly to their stale base — and
> the diffs were structurally un-cherry-pickable against the current
> modular `src/handlers/{mod,http,transport,federation_receive,
> hook_subscribers}.rs` + `src/mcp/{mod,tools/}` layout.
>
> The harness itself is out-of-repo (Claude Code SDK); the in-repo
> half is this discipline section, applied by every agent that
> dispatches sub-agents via `isolation=worktree` or that operates
> inside a worktree spawned by a parent agent. Issue #856 tracks the
> harness-side fix (worktree-base pinning at spawn time).

### Discipline (every parent agent that spawns worktree-isolated children)

**1. Fresh-base sync at worktree creation.** Before spawning a
worktree-isolated agent, the parent agent MUST:

- Resolve the parent-repo HEAD SHA: `git rev-parse HEAD`
- Pass the SHA explicitly to the sub-agent prompt (e.g. "you are
  operating on base SHA `<sha>` against `local/install-815-816`")
- Verify the sub-agent's worktree is at that SHA before it begins
  work: `git -C <worktree> rev-parse HEAD` MUST match the resolved
  SHA at spawn time, NOT an older fetched-remote SHA, NOT a stale
  default-branch HEAD

**2. File-layout pre-flight at worktree boot.** The sub-agent MUST,
as its first substantive action, check the file-layout invariants
that anchor its working scope. For Wave-2 Tier-A class work:

```bash
# Must be modular at v0.7.0:
test -d src/handlers && test -d src/handlers/http.rs -o -f src/handlers/http.rs
test -d src/mcp && test -d src/mcp/tools
# Must NOT be monolithic:
test ! -f src/handlers.rs || (echo "STALE BASE — abort" >&2 && exit 64)
test ! -f src/mcp.rs || (echo "STALE BASE — abort" >&2 && exit 64)
```

The sub-agent halts with exit code 64 (sysexits.h `EX_USAGE`) on
stale base and reports back to the parent so the parent can re-dispatch
against the correct base.

**3. Diff statement at commit time.** Every worktree commit message
MUST include the base SHA the work was authored against:

```
fix(#NNN): <summary>

Base: <full SHA from parent at spawn time>
```

This makes the eventual cherry-pick or merge trivially auditable.

**4. Cherry-pick verification before re-dispatch.** The parent agent,
on receiving a worktree's commits, MUST verify cherry-pickability
before claiming the work is integrated:

```bash
git cherry-pick --no-commit <worktree-sha>
git status   # look for structural conflicts
git cherry-pick --abort   # if conflicts surfaced, the work is a SPEC, not a patch
```

If the cherry-pick fails on file-layout grounds, the original commits
remain valuable as a SPEC for re-execution against the current layout
(preserve the `worktree-agent-*` branch for reference); the work is
re-dispatched as a fresh agent against the current HEAD.

**5. Serial dispatch on file-layout transitions.** During refactor
waves that move large amounts of code (e.g. Wave 1's `src/handlers.rs`
→ `src/handlers/` split, the `src/mcp.rs` → `src/mcp/` split), the
parent agent MUST serialize child dispatch until the refactor lands.
Parallel dispatch during file-layout drift is the single highest-
probability failure mode for worktree isolation.

### Discipline (every sub-agent operating in a worktree)

**1. Read CLAUDE.md and this section first.** Before any substantive
action, the worktree-isolated sub-agent confirms it's operating
against the expected file layout. Pre-flight at boot, not after the
gates pass.

**2. Emit the base SHA in every commit and every handoff memory.**
The base SHA at worktree spawn becomes part of the audit trail. If
the parent agent dispatched against the wrong base, the handoff memory
preserves enough context for a forensic re-dispatch.

**3. Refuse to cherry-pick yourself.** A worktree-isolated sub-agent
does NOT push its commits to the parent branch. It commits to its own
worktree branch and reports the SHA + base SHA back to the parent.
The parent owns the cherry-pick (or re-dispatch) decision because the
parent has the full view of concurrent worktrees.

### Out-of-repo half (harness fix tracked under #856)

The Claude Code SDK harness's `isolation=worktree` mode currently
forks worktrees from an undocumented base (likely a stale remote-
tracking branch). The harness-side fix is: when the parent agent
calls Task/Agent with `isolation=worktree`, the harness MUST pin the
worktree base to the EXPLICIT parent-repo HEAD at spawn time, NOT to
any other reference. The resolved SHA SHOULD be exposed to the
spawned sub-agent via environment variable (e.g.
`CLAUDE_WORKTREE_BASE_SHA`) so step 2 of the in-repo discipline
above can verify mechanically.

Until the harness-side fix ships, this in-repo discipline is the
load-bearing mitigation. Every agent that touches worktree-isolated
dispatch in this repository follows this section.

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
