---
sidebar_position: 1
title: Architecture
description: Three interfaces over a shared SQLite database. Module map.
---

# Architecture

ai-memory is a Rust-based persistent memory system exposing **three interfaces** over a **shared SQLite database**.

```
┌─────────────────────────────────────────────────────┐
│                  Three interfaces                   │
├──────────────┬─────────────────┬────────────────────┤
│  MCP Server  │   HTTP API      │       CLI          │
│ (src/mcp.rs) │(src/handlers.rs)│   (src/main.rs)    │
│ stdio JSON-  │ Axum REST,      │ clap, 26 commands  │
│ RPC 2.0,     │ port 9077,      │ + `--json` output  │
│ 23 tools     │ 24 endpoints    │                    │
├──────────────┴─────────────────┴────────────────────┤
│              Shared validation (src/validate.rs)    │
│              Shared database  (src/db.rs)           │
│              SQLite + WAL + FTS5                    │
└─────────────────────────────────────────────────────┘
```

Shared state: `Arc<Mutex<(Connection, PathBuf, ResolvedTtl, bool)>>` — a single SQLite connection protected by a mutex. **Lock contention is the bottleneck** under concurrent HTTP + MCP load.

## Module map

| Module | Role |
|---|---|
| `main.rs` | CLI parsing, daemon setup (Axum + GC scheduler), command dispatch |
| `mcp.rs` | MCP server: stdin/stdout JSON-RPC loop, tool definitions |
| `db.rs` | All SQLite operations: CRUD, FTS5, recall scoring, GC, schema migrations |
| `handlers.rs` | HTTP request handlers (Axum extractors), error sanitization |
| `models.rs` | Core data structures (Memory has 15 fields, MemoryLink, request/response) |
| `validate.rs` | Input validation for all write paths |
| `config.rs` | Feature tier system (keyword/semantic/smart/autonomous), TTL config |
| `reranker.rs` | Hybrid recall: blends semantic (cosine) + keyword (FTS5) scores |
| `embeddings.rs` | HuggingFace Candle, vector generation, cosine similarity |
| `hnsw.rs` | In-memory HNSW vector index for ANN search |
| `llm.rs` | Ollama integration: query expansion, auto-tagging, contradiction detection |
| `toon.rs` | TOON: token-efficient JSON alternative (40-60% smaller) |
| `mine.rs` | Conversation import (Claude / ChatGPT / Slack exports) |
| `errors.rs` | `ApiError`, `MemoryError` enum, HTTP status mapping |
| `identity.rs` | NHI agent_id resolution + immutability |

## Sync architecture (v0.6.0+)

```
┌─────────────────────────────────────────────────────┐
│              SYNC PROTOCOL LAYERS                   │
├─────────────────────────────────────────────────────┤
│ L4: Federation    org ↔ org, open knowledge commons │  ← v1.x+
│ L3: Hub           PostgreSQL shared state for teams │  ← v0.9.5
│ L2: Mesh          agent ↔ agent, peer-to-peer sync  │  ← v0.6.0 ✓
│ L1: Transport     HTTP API + native TLS             │  ← v0.6.0 ✓
│ L0: Merge         CRDT-lite (timestamp-aware today) │  ← v0.6.0 partial
├─────────────────────────────────────────────────────┤
│ FOUNDATION                                          │
│ • SQLite WAL (concurrent readers)                   │
│ • Vector clocks per agent (sync_state table)        │
│ • Namespace isolation + 3-level rule propagation    │
│ • Contradiction detection + confidence scoring      │
│ • Memory linking (related_to, supersedes, ...)      │
└─────────────────────────────────────────────────────┘
```
