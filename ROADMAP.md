# ai-memory Roadmap

> **AI endpoint memory.** Every AI agent gets persistent, synced memory — the same way every network endpoint gets an IP address. A primitive, not a product.

> Single repo: `alphaonedev/ai-memory-mcp`
> Current production version: **v0.5.4.4** (2026-04-13)
> License: Apache-2.0 | Trademark: ai-memory(TM) USPTO Serial No. 99761257
> Execution model: **AI-built, human-vetted** — Claude Code 24x7, orchestrated by AlphaOne

---

## North Star

**AI endpoint memory is a primitive, not a product.**

AI agents are stateless by default. Every session starts from zero. Models get replaced. Vendors shut down. Infrastructure gets rebuilt. The knowledge disappears with them.

ai-memory makes knowledge persistent. What agents learn survives the agent, the model, the vendor, and the platform. One agent learns it, every agent knows it — across systems, across teams, across time.

No AI agent should ever have to relearn what any AI agent already knows.

---

## Design Philosophy

**Zero-cost memory for AI agents.**

- Zero tokens until recall
- Zero infrastructure (single SQLite file)
- Zero latency (local-first, no network)
- Zero lock-in (works with any MCP-compatible AI)
- Zero knowledge loss (agents die, memories survive)

SQLite is the backbone. Local-first is the moat. Every feature must preserve this.

---

## Execution Model

**Human-led, AI-accelerated development.** Humans maintain full oversight over all AI code implementations. AI coding agents (Claude Code, OpenAI Codex, xAI Grok, and others) are tools under human direction — not autonomous developers.

- **Owner & Gatekeeper** — `@alphaonedev` approves all merges to `main` (CODEOWNERS enforced). Every line of code is human-reviewed before it reaches production.
- **Architect** — humans make all design decisions. AI agents propose, humans approve.
- **Quality gate** — humans vet all code against [Engineering Standards](docs/ENGINEERING_STANDARDS.md). AI agents run the checks, humans interpret the results.
- **Contributors** — both human developers and human-supervised AI coding sessions. All contributions follow the same PR process regardless of who (or what) wrote the code.

**LOE unit** = 1 session (one focused AI-assisted coding interaction producing human-reviewable output).

**Future possibility:** As AI agent development matures, ai-memory itself could enable the transition to AI-led development teams — agents that remember what they built, why they built it, and what broke last time. The persistent memory that makes autonomous AI development viable may be the product we're building. That future isn't here yet, but when it arrives, ai-memory will be the infrastructure it runs on.

---

## The Sync Architecture

The defining capability of ai-memory beyond v1.0. Five layers, built bottom-up:

```
┌─────────────────────────────────────────────────────┐
│              SYNC PROTOCOL LAYERS                   │
├─────────────────────────────────────────────────────┤
│ L4: Federation    org ←→ org, open knowledge commons│
│ L3: Hub           PostgreSQL shared state for teams  │
│ L2: Mesh          agent ←→ agent, peer-to-peer sync  │
│ L1: Transport     file copy, HTTP API, message bus   │
│ L0: Merge         CRDT-lite conflict resolution      │
├─────────────────────────────────────────────────────┤
│ FOUNDATION (exists today in v0.5.4.4)               │
│ - SQLite WAL (concurrent readers)                   │
│ - ai-memory sync (bidirectional merge)              │
│ - Namespace isolation + three-level rule propagation│
│ - Contradiction detection + confidence scoring      │
│ - Memory linking (related_to, supersedes,           │
│   contradicts, derived_from)                        │
└─────────────────────────────────────────────────────┘
```

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

## Phase 1 — Memory Schema, Hierarchy & Governance

**Target: v0.6.0 | LOE: 8-10 sessions**

The foundation for everything that follows. Three pillars: evolve the schema without breaking anything, establish the memory hierarchy, and define who governs what.

### 1a. Schema Evolution

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| `metadata` JSON column | 1 | Add `metadata TEXT NOT NULL DEFAULT '{}'` to memories table. Fixed core columns stay (id, tier, namespace, title, content, priority, confidence, access_count, timestamps, embedding). Everything else lives in flexible JSON — `agent_id`, `scope`, `schema_version`, `governance`, custom fields. No migrations ever again. |
| `agent_id` in metadata | 0.5 | Every memory knows which agent stored it. Populated on store via MCP/CLI/API. Foundation for multi-agent attribution. |
| Agent registration | 0.5 | `memory_agent_register` — agents announce themselves with an ID, type (AI model, human, system), and capability set. Stored in metadata. |

**Design principle:** The core columns are the primitive — they will not change in 20 years. The metadata JSON is the evolution surface — any future feature adds a field to metadata, not a column to the schema. Old databases open in new code. New databases open in old code. Sync between different versions works because unknown metadata fields are preserved, not rejected.

### 1b. Memory Hierarchy

Namespaces become hierarchical paths. Visibility flows up. Policies flow down.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Hierarchical namespace paths | 1-2 | `/`-delimited namespaces: `org/unit/team/agent`. Existing flat namespaces still work — hierarchy is opt-in. |
| Visibility rules | 1 | An agent at `alphaone/engineering/platform/agent-1` can read its own memories + team + unit + org + collective. Enforced at query time via namespace prefix matching. |
| Memory promotion (vertical) | 0.5 | Promote a memory UP the hierarchy: agent learning → team knowledge → unit policy → org standard. Existing `memory_promote` extended with a `--to-namespace` flag. |
| N-level rule inheritance | 1 | Extend three-level standard system to N levels. Org rules cascade to units, units cascade to teams, teams cascade to agents. |

```
collective                                    ← cross-org commons
└── alphaone                                  ← org memory
    ├── engineering                            ← unit memory
    │   ├── platform                           ← team memory
    │   │   ├── agent-claude-ops-1             ← individual agent
    │   │   └── agent-grok-monitor-2           ← individual agent
    │   └── security                           ← team memory
    │       └── agent-claude-sec-1             ← individual agent
    └── operations                             ← unit memory
        └── sre                                ← team memory
            └── agent-codex-deploy-1           ← individual agent
```

### 1c. Governance

Every level of the hierarchy has a governance model: who can store, who can promote, who can delete, and who approves changes at that level.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Governance metadata | 1 | Each namespace level can define a `governance` policy in its standard: `{ "write": "any", "promote": "approve", "delete": "owner", "approver": "human" }` |
| Governance roles | 0.5 | `owner` (full control), `writer` (store + update), `reader` (recall + search only). Stored per-namespace in standards. |
| Approval workflow | 1 | When governance requires approval for promotion or deletion, the action is queued with status `pending`. An approver (human or designated AI agent) confirms or rejects. |
| Governance type | 0.5 | `"approver": "human"` or `"approver": "agent:agent-id"` or `"approver": "consensus:3"` (N agents must agree). The governance model itself is flexible — some orgs want humans in the loop, some want AI autonomy, some want consensus. ai-memory doesn't choose — it provides the mechanism. |

**Why governance matters:** Without it, any agent can write anything anywhere. That's fine for a single agent. For 100 agents in an organization, it's chaos. Governance is the difference between collective intelligence and collective noise. But governance must be flexible — a startup with 3 agents wants zero friction. An enterprise with 1000 agents wants approval chains. The governance model is metadata, not code — it's configured per namespace, not hardcoded.

### 1d. Smart Recall

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Context-budget-aware recall | 1-2 | `budget_tokens` parameter — return as many memories as fit in N tokens. **No competitor has this.** LLMs have finite context windows. "Give me the most relevant memories that fit in 4K tokens" is the killer feature. |
| Hierarchy-aware recall | 0.5 | Recall automatically includes memories from the agent's level + all ancestor namespaces. An agent in `alphaone/engineering/platform` gets platform memories + engineering policies + org standards in one recall. |

---

## Phase 2 — Knowledge Graph Engine

**Target: v0.7.0 | LOE: 5-7 sessions**

Memories become a connected graph, not a flat list. This is the irreplaceable moat — no flat vector store can compete with connected knowledge.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Graph query tool | 1-2 | `memory_graph_query` — "find everything connected to X within N hops." SQLite recursive CTEs, zero new dependencies. |
| Path finding | 1 | `memory_find_paths` — "how is memory A connected to memory B?" Traverses the link graph. |
| Auto link inference | 1-2 | LLM-based automatic detection of related_to and contradicts relationships on store. Uses existing Ollama integration (smart+ tiers). |
| Temporal reasoning | 1 | "What did I know about X as of date Y?" Point-in-time queries using `created_at` / `updated_at` filtering. |

**Why this matters:** When an agent recalls "PostgreSQL configuration," it doesn't just get that memory — it gets the linked architecture decision, the performance benchmark, the incident that caused the change, and the team policy that governs it. Connected knowledge is exponentially more valuable than isolated facts.

---

## Phase 3 — Memory Sharing & Sync

**Target: v0.8.0 | LOE: 8-10 sessions**

The unlock. AI agents become a collective intelligence.

### 3a. Merge Protocol (Sync Layer L0)

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| CRDT-lite merge rules | 2 | Defined conflict resolution for every field: last-write-wins for content, max-wins for access_count, union for tags, higher-confidence-wins for contradictions. Governance metadata determines whether merge is automatic or requires approval. |
| Vector clock per agent | 1 | Each agent tracks its sync state — "last memory I've seen from Agent B." Enables efficient delta sync. |
| Merge verification | 1 | `ai-memory sync --dry-run` shows what would change before committing. |

### 3b. Peer Sync (Sync Layer L2)

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Auto background sync daemon | 1-2 | `ai-memory sync-daemon --peers /path/to/peer1.db,/path/to/peer2.db` — watches for changes, auto-merges. Built on `notify` crate + existing sync logic. |
| HTTP sync endpoint | 1 | `POST /api/v1/sync` — push/pull memories over the existing HTTP API. Agents on different machines sync via network. |
| Selective sync | 1 | Sync follows the hierarchy. Team agents sync team-level memories. Unit agents sync unit-level. Private stays private. Governed by namespace visibility rules from Phase 1. |

### What This Enables

**Individual memory:** An agent's private knowledge. Stored at `org/unit/team/agent-id`. Visible only to that agent. No sync unless the agent promotes it.

**Team memory:** Shared knowledge for a team of agents working together. Stored at `org/unit/team`. Auto-synced between all agents in the team. Governed by team-level governance policy.

**Unit memory:** Department-wide knowledge. Stored at `org/unit`. Policies, standards, shared context for all teams in the unit.

**Organizational memory:** Company-wide knowledge. Stored at `org`. Compliance rules, architecture standards, incident learnings. Every agent in the org inherits it.

**Collective memory:** The global organization's complete knowledge — the union of all memories at all levels. An org-level query returns everything: individual learnings, team knowledge, unit policies, org standards. The collective IS the organization's AI intelligence.

**Shift handoff:** Night shift agents sync to day shift agents automatically. Zero knowledge gap.

**Swarm intelligence:** 100 agents processing tickets. Each learns at the individual level. Valuable learnings get promoted to team level. Team level syncs across the swarm. The 100th ticket benefits from all 99 prior learnings.

**Knowledge inheritance:** Server decommissioned. Agent dies. Replacement agent syncs team + unit + org memories and has full institutional knowledge from day one.

**Distributed immune system:** One agent encounters a novel failure pattern. Promotes it to team level. Syncs to all agents in the team. If it's critical, promoted to org level — entire fleet immunized.

---

## Phase 4 — Autonomous Curator

**Target: v0.9.0 | LOE: 4-6 sessions**

ai-memory stops being reactive and becomes **self-improving.**

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Background curator daemon | 1-2 | `ai-memory curator` — runs periodically. Auto-consolidates related memories, detects contradictions proactively, suggests promotions/demotions. Uses existing Ollama integration. |
| Auto-extraction | 2 | Watch conversations (via MCP hooks) and auto-store facts, decisions, and corrections. Auto-stored memories get confidence < 1.0, human-stored get 1.0. |
| Consensus memory | 1 | When multiple agents store conflicting facts, use confidence + agent count to determine truth. If 4 of 5 agents agree, confidence is 0.95. |
| Memory health dashboard | 0.5 | `ai-memory doctor` — fragmentation, stale memories, unresolved contradictions, sync lag. |

---

## Phase 5 — Team Hub (Sync Layer L3)

**Target: v0.9.5 | LOE: 4-6 sessions**

For agent teams that need a central sync point. SQLite remains the default for individual agents. PostgreSQL is the **shared hub** — not a replacement, a coordination layer.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| PostgreSQL + pgvector hub | 2-3 | Central sync point. Agents push/pull via HTTP API. `tsquery` for FTS, pgvector for embeddings. `--backend postgres` flag. |
| Hub-spoke sync topology | 1 | Agents sync to hub. Hub distributes to all agents. Offline-first — agents work disconnected, sync when connected. |
| Migration CLI | 1 | `ai-memory migrate --from sqlite --to postgres` |

---

## Phase 5b — Data Tier Backends (Mid-to-Big Infrastructure)

**Target: v0.9.5+ | LOE: 15-19 sessions | Post-GA for Tier 3-4**

SQLite is the backbone for small infrastructure (single agent, single system). PostgreSQL (Phase 5) is the hub for mid infrastructure (agent teams, multi-system). For big infrastructure — enterprise scale, specialized workloads, high-throughput agent swarms — optional data tier backends unlock domain-specific performance.

**Gate:** Each tier requires the `StorageBackend` trait (introduced in Phase 5 with PostgreSQL). Backends are additive — SQLite always works, backends are opt-in.

### Small Infrastructure (default — ships today)

| Backend | Notes |
|---------|-------|
| **SQLite + FTS5** | Default. Local-first. Zero infrastructure. Handles 10K+ memories, 500+ q/s keyword tier. |

### Mid Infrastructure (Phase 5)

| Backend | Sessions | Notes |
|---------|:--------:|-------|
| **PostgreSQL + pgvector** | 2-3 | Team hub. Shared sync point. `tsquery` for FTS, pgvector for embeddings. Closest SQLite analog. |

### Big Infrastructure — Tier 2: Vector-Native

For agent swarms needing sub-millisecond vector search at 100K+ memories.

| Backend | Sessions | Difficulty | Notes |
|---------|:--------:|:----------:|-------|
| **Qdrant** | 1 | Low-Medium | Purpose-built vector DB, payload filtering, simple REST API. Best for pure semantic recall at scale. |
| **Pinecone** | 1 | Low-Medium | Managed vector search, clean SDK, metadata filtering. Best for teams that want zero-ops vector search. |
| **Weaviate** | 1-2 | Medium | Hybrid BM25 + vector built in, GraphQL query layer. Best for hybrid recall without custom blending. |
| **Milvus / Zilliz** | 1-2 | Medium | Strong vector search, scalar filtering. Best for massive-scale (millions of memories) deployments. |

### Big Infrastructure — Tier 3: Multi-Model & Graph

For agent collectives needing native graph traversal and multi-model queries.

| Backend | Sessions | Difficulty | Notes |
|---------|:--------:|:----------:|-------|
| **Neo4j** | 2-3 | High | Native graph model for memory links. Cypher queries for multi-hop traversal. Vector index (5.x+). Best for knowledge graph-heavy workloads. |
| **SurrealDB** | 2 | Medium-High | Multi-model (document + graph + vector). SurrealQL. Best for teams wanting one backend for everything. |

### Big Infrastructure — Tier 4: Relational & Cache

For enterprise environments with existing database infrastructure.

| Backend | Sessions | Difficulty | Notes |
|---------|:--------:|:----------:|-------|
| **TiDB** | 1-2 | Medium | MySQL-compatible + vector search, distributed transactions. Best for enterprises already on MySQL/TiDB. |
| **Redis (RediSearch)** | 1-2 | Medium | In-memory FTS + vector. No ACID — eventual consistency. Best for ultra-low-latency recall where durability is handled by SQLite sync. |
| **MongoDB Atlas Vector** | 1-2 | Medium | Document model maps to Memory struct. Atlas Vector Search. Best for teams already on MongoDB. |

### Data Tier Infrastructure

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| `StorageBackend` trait | 1-2 | Async CRUD, recall, search, FTS, transaction interfaces. Introduced with PostgreSQL in Phase 5. |
| `ai-memory migrate` CLI | 1 | `--from sqlite --to <backend>` zero-downtime migration between any backends. |
| Hybrid mode | 1 | Local SQLite cache + remote backend. Offline-first with sync. Agents always work locally, backends provide scale and sharing. |
| Integration test matrix | 1 | CI pipeline testing all backends against full test suite. |

**Contribution model:** Tier 2-4 backends can be community-contributed. AlphaOne builds and maintains SQLite + PostgreSQL. Community contributes backends against the `StorageBackend` trait and maintains them.

---

## Phase 6 — Production GA

**Target: v1.0.0 | LOE: 6-8 sessions**

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| API stability guarantee | 1 | Freeze all MCP tools, HTTP endpoints, CLI commands — semver contract |
| Plugin SDK (TypeScript) | 1-2 | `@alphaone/ai-memory` npm package |
| Plugin SDK (Python) | 1-2 | `ai-memory-sdk` PyPI package |
| Memory portability spec | 1 | Export format as a published specification. If ai-memory dies in 10 years, users can read their data with any tool. |
| Security audit | 1 | Full code review, fuzzing, dependency audit |
| TOON v2 | 0.5 | Schema inference compression, target 85%+ token reduction |

---

## Phase 7 — Federation & Protocol Standard (Sync Layer L4)

**Target: v1.x+ | LOE: 8-12 sessions | The 20-Year Play**

ai-memory becomes a **standard**, not just a product.

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Federation protocol | 2-3 | Org-to-org memory sharing with consent, access control, and anonymization. Agent teams in Organization A share operational knowledge with Organization B's agents. |
| Open Knowledge Commons | 2-3 | A public pool of anonymized operational knowledge. Like open-source code, but open-source agent knowledge. "This Kubernetes configuration causes OOM kills" — shared across the community. |
| Protocol specification | 2 | Published RFC-style spec for ai-memory sync protocol, memory schema, TOON format, and recall scoring algorithm. Other projects implement it. |
| Adversarial memory | 1-2 | Red team vs blue team agents building competing knowledge bases. Automated adversarial security testing that gets harder on both sides over time. |
| Cross-domain synthesis | 1 | Agents from different domains (security, infrastructure, application) query collective memory to answer questions no single agent can. "What's our exposure to CVE-2026-4521?" synthesized from security + inventory + network + SLA knowledge. |

---

## Program Summary

| Phase | Milestone | Sessions | What It Unlocks | Infra Scale |
|:-----:|-----------|:--------:|-----------------|:-----------:|
| 0 | Foundation (v0.5.4) | 5-7 | **COMPLETE** | Small |
| 1 | Schema, Hierarchy & Governance (v0.6.0) | 8-10 | Flexible schema, org hierarchy, governance model | Small |
| 2 | Knowledge Graph Engine (v0.7.0) | 5-7 | Connected knowledge, multi-hop reasoning | Small |
| 3 | Memory Sharing & Sync (v0.8.0) | 8-10 | Collective intelligence across systems | Small-Mid |
| 4 | Autonomous Curator (v0.9.0) | 4-6 | Self-improving memory | Small-Mid |
| 5 | Team Hub — PostgreSQL (v0.9.5) | 4-6 | Organizational shared knowledge | Mid |
| 5b | Data Tier Backends (v0.9.5+) | 15-19 | Enterprise-scale, specialized workloads | Mid-Big |
| 6 | Production GA (v1.0.0) | 6-8 | Stability contract, SDKs, portability | All |
| 7 | Federation & Protocol (v1.x+) | 8-12 | Open knowledge commons | All |
| | **TOTAL** | **64-85** | | |

---

## What We Will NOT Build

Decisions made, not deferred. These are intentional exclusions:

- **Backends as default** — SQLite is always the default. PostgreSQL is the mid-tier hub. Tier 2-4 backends (Qdrant, Pinecone, Weaviate, Milvus, Neo4j, SurrealDB, TiDB, Redis, MongoDB) are opt-in for big infrastructure and primarily community-contributed.
- **CRDT full implementation** — CRDT-lite merge rules for metadata fields. Not a full CRDT library. Pragmatic conflict resolution, not theoretical purity.
- **Mobile SDKs** — Not until post-GA. The agents are the users, not mobile apps.
- **Cloud-hosted service** — ai-memory is infrastructure, not SaaS. Self-hosted always. No vendor lock-in.
- **Web UI** — Terminal-first. If visualization is needed, it's a separate project that reads the SQLite file.

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
