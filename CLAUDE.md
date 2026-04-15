# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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

## Architecture

**ai-memory** is a Rust-based persistent memory system exposing three interfaces over a shared SQLite database layer:

1. **MCP Server** (`src/mcp.rs`) — stdio JSON-RPC 2.0 with 23 tools + 2 prompts
2. **HTTP API** (`src/handlers.rs`) — Axum REST server on port 9077, 24 endpoints at `/api/v1/`
3. **CLI** (`src/main.rs`) — clap-based, 26 commands with optional `--json` output

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
2. **Semantic search** — cosine similarity via HNSW index (or linear scan fallback), threshold >0.3
3. **Adaptive blending** — `final = semantic_weight * cosine + (1 - semantic_weight) * norm_fts`. Semantic weight varies 0.50 (short content ≤500 chars) → 0.15 (long content ≥5000 chars) because embeddings lose information on long text
4. **Touch operations** (atomic) — increment `access_count`, extend TTL (1h short / 1d mid), auto-promote mid→long at 5 accesses, increment priority every 10 accesses

### Upsert Behavior

Storing a memory with the same `(title, namespace)` updates the existing one. Tier is never downgraded (takes max). Expiry is only cleared if the new memory is `long`-tier.

### Database

SQLite with WAL mode, FTS5 virtual table for full-text search, schema version v7 with automated migrations. Archive table preserves GC'd memories for restoration. FTS is kept in sync via INSERT/DELETE/UPDATE triggers. GC runs every 30 minutes; expired memories are archived before deletion when `archive_on_gc=true` (default).

### Environment Variables

- `AI_MEMORY_DB` — database path override
- `AI_MEMORY_NO_CONFIG=1` — skip loading `~/.config/ai-memory/config.toml`
- `RUST_LOG` — tracing filter (e.g. `RUST_LOG=ai_memory=debug`)

Config precedence: CLI flags > config file > compiled defaults.

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
