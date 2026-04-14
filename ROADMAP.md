# ai-memory Roadmap

> Single repo: `alphaonedev/ai-memory-mcp`
> Current production version: **v0.5.4.4** (2026-04-13)
> License: Apache-2.0 | Trademark: ai-memory(TM) USPTO Serial No. 99761257
> Execution model: **AI-built, human-vetted** — Claude Code 24x7, orchestrated by AlphaOne

---

## Design Philosophy

**Zero-cost memory for AI agents.**

- Zero tokens until recall
- Zero infrastructure (single SQLite file)
- Zero latency (local-first, no network)
- Zero lock-in (works with any MCP-compatible AI)

SQLite is the backend. Period. The local-first, zero-infrastructure model is the competitive moat. Every feature must preserve this.

**What we killed:** StorageBackend trait, 10 database backends, CRDT sync, mobile SDKs, E2E encryption, federated memory. Zero user value, high complexity, undermines the local-first advantage. If users need PostgreSQL, that's Phase 5 — optional, not default.

---

## Execution Model

All implementation is AI-executed via Claude Code agents operating 24x7x365. The human role is:
- **Gatekeeper** — `@alphaonedev` approves all merges to `main` (CODEOWNERS enforced)
- **Architect** — approve design decisions
- **Quality gate** — vet all code against [Engineering Standards](docs/ENGINEERING_STANDARDS.md)

**LOE unit** = 1 session (one focused Claude Code interaction producing reviewable output).
**Throughput** = 4-8 sessions/day with agent orchestration.

---

## Branch Strategy

| Branch | Purpose |
|--------|---------|
| `develop` | Active development — all PRs merge here first |
| `main` | Production releases — protected, owner approval required |
| `feature/*` | Individual feature branches off `develop` |

Promotion: `feature/*` -> PR to `develop` -> stabilize -> PR to `main` -> tag -> release

---

## Phase 0 — Foundation

**Target: v0.5.4 | Status: COMPLETE**

Delivered:
- Archive system (GC archives before delete, 4 archive MCP tools)
- 23 MCP tools (up from 17)
- Schema v3 -> v4 (memory_archive table)
- Configurable TTL per tier
- Namespace standards with auto-prepend (three-level rule layering)
- 3 rounds of red-team security audits (~100+ findings, all resolved)
- Apache-2.0 license migration, CLA, OIN membership, USPTO trademark filing
- Pedantic clippy (zero warnings under `-D clippy::all -D clippy::pedantic`)
- Single-repo consolidation (ai-memory-mcp-dev archived)
- All package repos live: Homebrew, Ubuntu PPA, Fedora COPR/EPEL

---

## Phase 1 — Semantic Intelligence

**Target: v0.6.0 | LOE: 6-8 sessions**

The recall engine gets smarter. Every feature here makes memories more findable.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Embedding model upgrade | 1-2 | Evaluate BGE-small-en-v1.5 vs current MiniLM-L6-v2, benchmark R@5 delta |
| Context-budget-aware recall | 1-2 | `budget_tokens` parameter — return as many memories as fit in N tokens. **No competitor has this.** |
| Graph-aware recall | 1 | 1-hop linked memories included in recall results (related_to, derived_from) |
| Hybrid reranker v2 | 1-2 | Improved cross-encoder reranking on top-K candidates |
| Decay scoring tuning | 0.5 | Configurable half-life parameter for recency decay |

**Why this matters:** Context-budget-aware recall is the killer feature. LLMs have finite context windows. Being able to say "give me the most relevant memories that fit in 4K tokens" is something no other memory system offers.

---

## Phase 2 — Automatic Memory Extraction

**Target: v0.7.0 | LOE: 8-10 sessions**

The "iPhone moment" — ai-memory watches conversations and auto-stores facts, decisions, and corrections without being asked.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Conversation observer | 2-3 | Hook into MCP message stream, detect storable facts/decisions |
| Fact extraction | 2 | LLM-based extraction of key facts from conversation turns |
| Dedup-aware auto-store | 1-2 | Auto-store only if memory doesn't already exist (title+namespace dedup) |
| Confidence scoring | 1 | Auto-stored memories get confidence < 1.0, user-stored get 1.0 |
| User override | 1 | "Don't remember this" / "Remember this differently" MCP tools |

**Why this matters:** Current workflow requires the AI to explicitly call `memory_store`. Most AIs forget to do this. Automatic extraction means nothing important is lost — ever.

---

## Phase 3 — Knowledge Graph & Reasoning

**Target: v0.8.0 | LOE: 6-8 sessions**

Memories become a connected graph, not a flat list.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Graph traversal in recall | 1-2 | Multi-hop traversal: "what do I know about X and everything related to X?" |
| Auto link inference | 1-2 | LLM-based automatic detection of related_to and contradicts relationships |
| Temporal reasoning | 1 | "What did I know about X as of date Y?" point-in-time queries |
| Web UI visualization | 2-3 | Browser-based memory graph explorer (local, no cloud) |

---

## Phase 4 — Performance & Scale

**Target: v0.9.0 | LOE: 4-6 sessions**

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Parallel recall pipeline | 1-2 | Concurrent FTS + semantic + graph traversal |
| HNSW index tuning | 1 | Optimize for 10K+ memory stores |
| Batch operations | 1 | Bulk store/recall/delete for import/migration |
| Memory compaction | 1 | Automatic deduplication and consolidation of stale memories |
| Benchmark validation | 0.5 | Target: 500+ q/s keyword tier, 99%+ R@5 |

---

## Phase 5 — Optional PostgreSQL Backend

**Target: v0.9.5 | LOE: 3-4 sessions**

For teams that want shared server-side memory. SQLite remains the default.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| PostgreSQL + pgvector backend | 2-3 | `tsquery` for FTS, pgvector for embeddings, `--backend postgres` flag |
| Migration CLI | 1 | `ai-memory migrate --from sqlite --to postgres` |

**Not building:** Qdrant, Pinecone, Weaviate, Milvus, Neo4j, SurrealDB, TiDB, Redis, MongoDB. If someone needs these, they can contribute a backend implementation against the trait.

---

## Phase 6 — Production GA

**Target: v1.0.0 | LOE: 6-8 sessions**

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| API stability guarantee | 1 | Freeze all MCP tools, HTTP endpoints, CLI commands — semver contract |
| Plugin SDK (TypeScript) | 1-2 | `@alphaone/ai-memory` npm package |
| Plugin SDK (Python) | 1-2 | `ai-memory-sdk` PyPI package |
| Security audit | 1 | Full code review, fuzzing, dependency audit |
| TOON v2 | 1 | Schema inference compression, target 85%+ token reduction |

---

## Program Summary

| Phase | Milestone | Sessions | Status |
|:-----:|-----------|:--------:|:------:|
| 0 | Foundation (v0.5.4) | 5-7 | **COMPLETE** |
| 1 | Semantic Intelligence (v0.6.0) | 6-8 | Next |
| 2 | Automatic Memory Extraction (v0.7.0) | 8-10 | |
| 3 | Knowledge Graph & Reasoning (v0.8.0) | 6-8 | |
| 4 | Performance & Scale (v0.9.0) | 4-6 | |
| 5 | Optional PostgreSQL (v0.9.5) | 3-4 | |
| 6 | Production GA (v1.0.0) | 6-8 | |
| | **TOTAL** | **39-51** | |

---

## Quality Gates

Every session must pass before merge to `develop`:

```bash
cargo fmt --check
cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test
cargo audit
```

Plus maintainer review: functional test, security review, documentation sync.
See [ENGINEERING_STANDARDS.md](docs/ENGINEERING_STANDARDS.md) for full details.
