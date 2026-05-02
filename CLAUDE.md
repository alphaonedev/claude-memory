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

## Architecture

**ai-memory** is a Rust-based persistent memory system exposing three interfaces over a shared SQLite database layer:

1. **MCP Server** (`src/mcp.rs`) — stdio JSON-RPC 2.0 with 43 tools + 2 prompts
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
- **Tiers**: short (6h TTL), mid (7d TTL), long (permanent)
- **Feature tiers**: keyword (FTS5 only) → semantic (MiniLM embeddings) → smart (Ollama) → autonomous (cross-encoder reranking)

### Recall Pipeline

Recall is multi-stage and **never read-only** — every recall mutates the database:

1. **FTS5 keyword search** — fuzzy OR query, scored by `fts.rank + priority*0.5 + access_count*0.1 + confidence*2.0 + tier_bonus + recency_factor`
2. **Semantic search** — cosine similarity via HNSW index (or linear scan fallback), threshold >0.2 (relaxed from 0.3 in v0.6.2 Patch 2 after scenario-18 caught a miss at 0.25-0.29 cosine for legitimately-related content)
3. **Adaptive blending** — `final = semantic_weight * cosine + (1 - semantic_weight) * norm_fts`. Semantic weight varies 0.50 (short content ≤500 chars) → 0.15 (long content ≥5000 chars) because embeddings lose information on long text
4. **Touch operations** (atomic) — increment `access_count`, extend TTL (1h short / 1d mid), auto-promote mid→long at 5 accesses, increment priority every 10 accesses

### Upsert Behavior

Storing a memory with the same `(title, namespace)` updates the existing one. Tier is never downgraded (takes max). Expiry is only cleared if the new memory is `long`-tier.

### Database

SQLite with WAL mode, FTS5 virtual table for full-text search, schema version v7 with automated migrations. Archive table preserves GC'd memories for restoration. FTS is kept in sync via INSERT/DELETE/UPDATE triggers. GC runs every 30 minutes; expired memories are archived before deletion when `archive_on_gc=true` (default).

### Environment Variables

- `AI_MEMORY_DB` — database path override
- `AI_MEMORY_NO_CONFIG=1` — skip loading `~/.config/ai-memory/config.toml`
- `AI_MEMORY_AGENT_ID` — default `agent_id` for memories this process writes (see §Agent Identity below)
- `RUST_LOG` — tracing filter (e.g. `RUST_LOG=ai_memory=debug`)

Config precedence: CLI flags > config file > compiled defaults.

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
