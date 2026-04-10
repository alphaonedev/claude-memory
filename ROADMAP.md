# ai-memory Roadmap

> Development fork: `alphaonedev/ai-memory-mcp-dev`
> Production repo: `alphaonedev/ai-memory-mcp`
> Current production version: **v0.5.2** (2026-04-08)

## Branch Strategy

| Branch | Purpose |
|--------|---------|
| `main` | Mirrors production — sync from `upstream/main` |
| `develop` | Active development — all features merge here first |
| `feature/*` | Individual feature branches off `develop` |

Promotion path: `feature/*` → `develop` → `main` → upstream release

---

## v0.6.0 — Semantic Intelligence

**Focus:** Enhance the embedding and recall pipeline.

- [ ] **Upgraded embedding model** — evaluate replacing candle-core's current model with a larger/more accurate embedding (e.g., BGE-M3, GTE-large)
- [ ] **Hybrid reranker v2** — cross-encoder reranking on top-K candidates for precision boost
- [ ] **Contextual recall** — auto-infer recall context from conversation history (no explicit query needed)
- [ ] **Memory clustering** — auto-group semantically similar memories, surface clusters as topics
- [ ] **Decay scoring** — time-weighted relevance (recent memories ranked higher when recency matters)

## v0.7.0 — Multi-Agent & Collaboration

**Focus:** Shared memory across agents and teams.

- [ ] **Shared namespaces** — multiple agents read/write to a common namespace with conflict resolution
- [ ] **Agent identity** — track which agent stored/modified each memory
- [ ] **Access control** — read-only vs read-write per namespace per agent
- [ ] **Memory subscriptions** — agent A gets notified when agent B stores in a shared namespace
- [ ] **Merge strategies** — configurable conflict resolution (last-write-wins, manual, LLM-mediated)

## v0.8.0 — Cloud Sync & Remote Storage

**Focus:** Optional cloud backend for cross-device and team sync.

- [ ] **Remote storage adapter** — pluggable backend (SQLite local, PostgreSQL remote, S3 blob)
- [ ] **Sync protocol** — CRDT-based or vector-clock merge for offline-first with eventual consistency
- [ ] **End-to-end encryption** — AES-256-GCM on client side before sync (zero-knowledge server)
- [ ] **Self-hosted server** — Docker image for teams running their own sync server
- [ ] **DigitalOcean deployment** — one-click deploy for AlphaOne infrastructure

## v0.9.0 — Performance & Scale

**Focus:** Push beyond current benchmarks.

- [ ] **Parallel recall pipeline** — concurrent FTS5 + semantic + graph traversal (current: sequential)
- [ ] **HNSW index tuning** — optimize ef_construction and M parameters for 10K+ memory stores
- [ ] **Batch operations** — bulk store/recall/delete for import/migration workflows
- [ ] **Memory compaction** — automatic deduplication and consolidation of stale memories
- [ ] **Benchmark target:** 500+ q/s keyword tier, 99%+ R@5 LLM-expanded tier

## v0.9.5 — Data Tier Integrations

**Focus:** Pluggable storage backends beyond local SQLite — vector databases, graph databases, and cloud-native stores.

### Vector Databases
- [ ] **Pinecone** — managed vector search, serverless indexes, metadata filtering
- [ ] **Qdrant** — high-performance vector similarity search with payload filtering
- [ ] **Weaviate** — AI-native vector database with hybrid search (BM25 + vector)
- [ ] **Milvus / Zilliz** — open-source vector DB (self-hosted Milvus or Zilliz Cloud managed)

### Graph + Multi-Model Databases
- [ ] **Neo4j + Aura Agent** — native graph storage for memory links/relationships, Aura for managed cloud
- [ ] **SurrealDB 3.0** — multi-model (document + graph + vector) in a single backend

### Relational + Vector Extensions
- [ ] **PostgreSQL + pgvector** — familiar Postgres with vector similarity search extension
- [ ] **TiDB / TiDB Cloud** — distributed SQL with vector search, MySQL-compatible

### Cache + Search Engines
- [ ] **Redis (RediSearch)** — in-memory store with vector similarity + full-text search modules
- [ ] **MongoDB Atlas Vector** — document store with native vector search indexes

### Architecture
- [ ] **Storage adapter trait** — `MemoryBackend` trait with async CRUD + recall + search interface
- [ ] **Backend registry** — config-driven backend selection (`--backend pinecone`, `--backend postgres`, etc.)
- [ ] **Migration tooling** — `ai-memory migrate --from sqlite --to <backend>` for zero-downtime backend switches
- [ ] **Hybrid mode** — local SQLite cache + remote backend (offline-first with sync)

## v1.0.0 — Production GA

**Focus:** Stability, documentation, ecosystem.

- [ ] **API stability guarantee** — all MCP tools, HTTP endpoints, CLI commands frozen
- [ ] **Migration tooling** — import from Claude auto-memory, ChatGPT memory, Cursor memory exports
- [ ] **Plugin SDK** — TypeScript/Python SDKs for building on top of ai-memory
- [ ] **Mobile SDK** — lightweight client for iOS (Swift) and Android (Kotlin) apps
- [ ] **Official MCP Registry v2** — updated server.json for any protocol changes
- [ ] **Security audit** — third-party code review and penetration test
- [ ] **TOON v2** — next-gen compression with schema inference (target: 85%+ reduction)

## Future / Exploratory

- **Federated memory** — memory graphs spanning multiple organizations with privacy boundaries
- **Temporal reasoning** — "what did I know about X as of date Y" queries
- **Memory visualization** — web UI for exploring memory graphs, clusters, and links
- **Voice interface** — recall/store via speech-to-text for mobile and wearable use cases
- **LLM-as-judge evaluation** — automated quality scoring of stored memories
- **Agentic memory management** — autonomous tier self-organizes, promotes, and garbage-collects

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
