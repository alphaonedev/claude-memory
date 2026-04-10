# ai-memory Roadmap

> Development fork: `alphaonedev/ai-memory-mcp-dev`
> Production repo: `alphaonedev/ai-memory-mcp`
> Current production version: **v0.5.2** (2026-04-08)
> Execution model: **AI-built, human-vetted** — Claude Code 24×7, orchestrated by AlphaOne

---

## Execution Model

All implementation is AI-executed via Claude Code agents operating 24×7×365. The human role is:
- **Architect** — approve design decisions and trait contracts
- **Quality gate** — vet all code against professional engineering standards
- **Orchestrator** — prioritize, sequence, and unblock

**LOE unit** = 1 session (one focused Claude Code interaction producing reviewable output).
**Throughput** = 4–8 sessions/day with agent orchestration (parallel where dependencies allow).

---

## Branch Strategy

| Branch | Purpose |
|--------|---------|
| `main` | Mirrors production — sync from `upstream/main` |
| `develop` | Active development — all features merge here first |
| `feature/*` | Individual feature branches off `develop` |

Promotion path: `feature/*` → `develop` → `main` → upstream release

---

## Phase 0 — Foundation (REQUIRED FIRST)

**Target: v0.5.3 | LOE: 5–7 sessions | ETA: Week 1**

The entire roadmap depends on decoupling from raw SQLite. This unlocks every milestone below.

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| Design `StorageBackend` trait | 1–2 | None | Trait definition with async CRUD, recall, search, FTS, transaction interfaces |
| Refactor `db.rs` behind trait | 1–2 | Trait approved | `SqliteBackend` impl, all 33 public functions wrapped, 161 tests green |
| Extract scoring to Rust layer | 1 | Trait approved | Portable 6-factor scoring (remove `julianday`, `CASE`, `json_each` from SQL) |
| FTS abstraction layer | 1 | Trait approved | `TextSearch` trait, `SqliteFts5` impl, pluggable full-text search |
| Backend registry + CLI flag | 0.5 | All above | `--backend <name>` flag, config-driven backend selection |

---

## Phase 1 — Semantic Intelligence

**Target: v0.6.0 | LOE: 6–8 sessions | ETA: Week 2**

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| Upgraded embedding model | 1–2 | None | Evaluate BGE-M3 / GTE-large, swap candle-core model, benchmark R@5 delta |
| Hybrid reranker v2 | 1–2 | Embedding model | Cross-encoder reranking on top-K candidates, precision boost measured |
| Contextual recall | 1 | None | Auto-infer recall context from conversation history |
| Memory clustering | 1–2 | Embedding model | Auto-group semantically similar memories, surface as topics |
| Decay scoring | 0.5 | None | Time-weighted relevance, configurable half-life parameter |

---

## Phase 2 — Multi-Agent & Collaboration

**Target: v0.7.0 | LOE: 6–8 sessions | ETA: Week 3**

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| Shared namespaces | 1–2 | Phase 0 trait | Multiple agents read/write common namespace with conflict resolution |
| Agent identity | 1 | Shared namespaces | Track which agent stored/modified each memory, `agent_id` field |
| Access control | 1–2 | Agent identity | Read-only vs read-write per namespace per agent, ACL model |
| Memory subscriptions | 1 | Shared namespaces | Agent A notified when Agent B stores in shared namespace (webhook/callback) |
| Merge strategies | 1–2 | Shared namespaces | Last-write-wins, manual, LLM-mediated conflict resolution |

---

## Phase 3 — Cloud Sync & Remote Storage

**Target: v0.8.0 | LOE: 6–8 sessions | ETA: Week 4**

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| Remote storage adapter | 1–2 | Phase 0 trait | Pluggable backend interface (SQLite local, PostgreSQL remote, S3 blob) |
| Sync protocol | 2 | Remote adapter | CRDT-based or vector-clock merge, offline-first with eventual consistency |
| End-to-end encryption | 1 | Remote adapter | AES-256-GCM client-side before sync, zero-knowledge server |
| Self-hosted server | 1–2 | Sync protocol | Docker image for teams running their own sync server |
| DigitalOcean deployment | 0.5 | Self-hosted server | One-click deploy, Terraform/Pulumi IaC for AlphaOne infrastructure |

---

## Phase 4 — Performance & Scale

**Target: v0.9.0 | LOE: 4–6 sessions | ETA: Week 5**

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| Parallel recall pipeline | 1–2 | Phase 0 trait | Concurrent FTS + semantic + graph traversal (currently sequential) |
| HNSW index tuning | 1 | None | Optimize ef_construction and M for 10K+ memory stores |
| Batch operations | 1 | Phase 0 trait | Bulk store/recall/delete for import/migration workflows |
| Memory compaction | 1 | None | Automatic deduplication and consolidation of stale memories |
| Benchmark validation | 0.5 | All above | Target: 500+ q/s keyword tier, 99%+ R@5 LLM-expanded tier |

---

## Phase 5 — Data Tier Integrations

**Target: v0.9.5 | LOE: 15–19 sessions | ETA: Weeks 5–7**

Requires Phase 0 `StorageBackend` trait. Backends can be built in parallel by multiple agents.

### Tier 1 — Ship First (prove the trait)

| Backend | Sessions | Difficulty | Notes |
|---------|:--------:|:----------:|-------|
| **PostgreSQL + pgvector** | 2 | Medium | `tsquery` for FTS, native JSON, pgvector for embeddings — closest SQLite analog |

### Tier 2 — Vector-Native (quick wins)

| Backend | Sessions | Difficulty | Notes |
|---------|:--------:|:----------:|-------|
| **Qdrant** | 1 | Low–Medium | Purpose-built vector DB, payload filtering, simple REST API |
| **Pinecone** | 1 | Low–Medium | Managed vector search, clean SDK, metadata filtering |
| **Weaviate** | 1–2 | Medium | Hybrid BM25 + vector built in, GraphQL query layer |
| **Milvus / Zilliz** | 1–2 | Medium | Strong vector search, scalar filtering |

### Tier 3 — Multi-Model & Graph

| Backend | Sessions | Difficulty | Notes |
|---------|:--------:|:----------:|-------|
| **Neo4j + Aura Agent** | 2–3 | High | Graph model for memory links, Cypher queries, vector index (5.x+) |
| **SurrealDB 3.0** | 2 | Medium–High | Multi-model (doc + graph + vector), SurrealQL |

### Tier 4 — Relational & Cache

| Backend | Sessions | Difficulty | Notes |
|---------|:--------:|:----------:|-------|
| **TiDB / TiDB Cloud** | 1–2 | Medium | MySQL-compatible + vector search, distributed transactions |
| **Redis (RediSearch)** | 1–2 | Medium | In-memory FTS + vector, no ACID — eventual consistency design |
| **MongoDB Atlas Vector** | 1–2 | Medium | Document model maps to Memory struct, Atlas Vector Search |

### Infrastructure

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| `ai-memory migrate` CLI | 1 | Any 2 backends | `--from sqlite --to <backend>` zero-downtime migration |
| Hybrid mode | 1 | Any remote backend | Local SQLite cache + remote backend, offline-first with sync |
| Integration test matrix | 1 | All backends | CI pipeline testing all backends against 161-test suite |

---

## Phase 6 — Production GA

**Target: v1.0.0 | LOE: 8–10 sessions | ETA: Week 8**

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| API stability guarantee | 1 | All phases | Freeze all MCP tools, HTTP endpoints, CLI commands — semver contract |
| Migration tooling | 1–2 | Phase 5 | Import from AI vendor memory exports (Claude, ChatGPT, Cursor, etc.) |
| Plugin SDK (TypeScript) | 1–2 | API freeze | `@alphaone/ai-memory` npm package for building on top of ai-memory |
| Plugin SDK (Python) | 1–2 | API freeze | `ai-memory-sdk` PyPI package |
| Mobile SDK | 1–2 | API freeze | Lightweight client for iOS (Swift) and Android (Kotlin) |
| Security audit | 1 | All phases | AI-driven code review, fuzzing, dependency audit, CVE scan |
| TOON v2 | 1 | None | Schema inference compression, target 85%+ token reduction |

---

## Phase 7 — Future / Exploratory

**Target: v1.x+ | LOE: 8–12 sessions | ETA: Weeks 9–10+**

| Task | Sessions | Dependencies | Deliverable |
|------|:--------:|:------------:|-------------|
| Federated memory | 2–3 | Phase 2 + Phase 3 | Memory graphs spanning organizations with privacy boundaries |
| Temporal reasoning | 1–2 | Phase 1 | "What did I know about X as of date Y" point-in-time queries |
| Memory visualization | 2 | Phase 2 | Web UI for exploring memory graphs, clusters, and links |
| Voice interface | 1–2 | Mobile SDK | Recall/store via speech-to-text for mobile and wearable |
| LLM-as-judge evaluation | 1 | Phase 1 | Automated quality scoring of stored memories |
| Agentic memory management | 1–2 | Phase 1 + Phase 2 | Autonomous tier self-organizes, promotes, and garbage-collects |

---

## Program Summary

| Phase | Milestone | Sessions | Calendar (24×7 agents) |
|:-----:|-----------|:--------:|:----------------------:|
| 0 | Foundation (v0.5.3) | 5–7 | **Week 1** |
| 1 | Semantic Intelligence (v0.6.0) | 6–8 | **Week 2** |
| 2 | Multi-Agent (v0.7.0) | 6–8 | **Week 3** |
| 3 | Cloud Sync (v0.8.0) | 6–8 | **Week 4** |
| 4 | Performance (v0.9.0) | 4–6 | **Week 5** |
| 5 | Data Tiers (v0.9.5) | 15–19 | **Weeks 5–7** |
| 6 | Production GA (v1.0.0) | 8–10 | **Week 8** |
| 7 | Future (v1.x+) | 8–12 | **Weeks 9–10+** |
| | **TOTAL** | **58–78** | **~10 weeks** |

**v0.5.2 → v1.0.0 GA in ~8 weeks.** Full exploratory roadmap complete in ~10 weeks.

At 4–8 sessions/day with parallel agent orchestration, phases with no cross-dependencies (e.g., Phase 4 + Phase 5 Tier 2 backends) can overlap, compressing the calendar further.

---

## Parallelization Opportunities

Sessions can run concurrently where dependencies allow:

```
Week 1:   [Phase 0: Foundation]
Week 2:   [Phase 1: Semantic Intelligence]
Week 3:   [Phase 2: Multi-Agent]
Week 4:   [Phase 3: Cloud Sync]
Week 5:   [Phase 4: Perf] + [Phase 5 Tier 1: Postgres]
Week 6:   [Phase 5 Tier 2: Qdrant, Pinecone, Weaviate, Milvus] ← 4 agents parallel
Week 7:   [Phase 5 Tier 3: Neo4j, SurrealDB] + [Phase 5 Tier 4: TiDB, Redis, MongoDB]
Week 8:   [Phase 6: Production GA]
Weeks 9+: [Phase 7: Exploratory]
```

---

## Quality Gates

Every session must pass before merge to `develop`:
1. **161 existing tests green** — zero regression
2. **New tests for new code** — minimum 80% coverage on new modules
3. **`cargo clippy` clean** — zero warnings
4. **Human code review** — architecture, correctness, edge cases
5. **Benchmark validation** — no performance regression on recall pipeline

---

## How to Contribute

1. Pick an item from any milestone
2. Create `feature/short-description` off `develop`
3. Implement + test
4. PR to `develop`
5. After stabilization, `develop` merges to `main` and gets tagged for upstream release

## Sync from Production

```bash
git fetch upstream
git checkout main
git merge upstream/main
git push origin main
git checkout develop
git merge main
```
