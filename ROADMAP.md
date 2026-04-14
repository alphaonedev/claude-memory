# ai-memory Roadmap

> **ai-memory is the nervous system for AI agent collectives — persistent, synced, human-owned knowledge infrastructure that makes every agent smarter because any agent learned.**

> Single repo: `alphaonedev/ai-memory-mcp`
> Current production version: **v0.5.4.4** (2026-04-13)
> License: Apache-2.0 | Trademark: ai-memory(TM) USPTO Serial No. 99761257
> Execution model: **AI-built, human-vetted** — Claude Code 24x7, orchestrated by AlphaOne

---

## North Star

In 20 years, every system will have dozens of AI agents — operational, security, development, monitoring, planning. These agents will come and go. Models will change. Vendors will die. Platforms will shift.

But the **collective knowledge** — what the agents learned, what worked, what failed, what matters — must persist across all of them, forever.

ai-memory is that knowledge layer. Not a tool. Not a library. **Infrastructure.**

Like DNS for names, SMTP for email, SQLite for embedded databases — ai-memory for AI agent memory.

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

All implementation is AI-executed via Claude Code agents operating 24x7x365. The human role is:
- **Gatekeeper** — `@alphaonedev` approves all merges to `main` (CODEOWNERS enforced)
- **Architect** — approve design decisions
- **Quality gate** — vet all code against [Engineering Standards](docs/ENGINEERING_STANDARDS.md)

**LOE unit** = 1 session (one focused Claude Code interaction producing reviewable output).

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

## Phase 1 — Smart Recall + Agent Identity

**Target: v0.6.0 | LOE: 6-8 sessions**

Two objectives: make recall dramatically smarter, and lay the schema foundation for multi-agent.

### 1a. Smart Recall

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Context-budget-aware recall | 1-2 | `budget_tokens` parameter — return as many memories as fit in N tokens. **No competitor has this.** LLMs have finite context windows. "Give me the most relevant memories that fit in 4K tokens" is the killer feature. |
| Graph-aware recall | 1-2 | 1-hop linked memories included in recall results. SQLite recursive CTEs — `WITH RECURSIVE` traversal. If you recall a memory, you also get everything it's linked to (related_to, derived_from). |
| Decay scoring tuning | 0.5 | Configurable half-life parameter for recency decay. |

### 1b. Agent Identity

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| `agent_id` schema field | 0.5 | New column on memories table. Every memory knows which agent stored it. Foundation for all multi-agent features. |
| `scope` field | 0.5 | `private` / `project` / `team` / `global` visibility enforcement at query time. Works with existing namespace hierarchy. |
| Agent registration | 0.5 | `memory_agent_register` MCP tool — agents announce themselves with an ID and capability set. |

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

## Phase 3 — Multi-Agent Sync

**Target: v0.8.0 | LOE: 8-10 sessions**

The unlock. AI agents become a collective intelligence.

### 3a. Merge Protocol (Sync Layer L0)

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| CRDT-lite merge rules | 2 | Defined conflict resolution for every field: last-write-wins for content, max-wins for access_count, union for tags, higher-confidence-wins for contradictions. |
| Vector clock per agent | 1 | Each agent tracks its sync state — "last memory I've seen from Agent B." Enables efficient delta sync. |
| Merge verification | 1 | `ai-memory sync --dry-run` shows what would change before committing. |

### 3b. Peer Sync (Sync Layer L2)

| Task | Sessions | Deliverable |
|------|:--------:|-------------|
| Auto background sync daemon | 1-2 | `ai-memory sync-daemon --peers /path/to/peer1.db,/path/to/peer2.db` — watches for changes, auto-merges. Built on `notify` crate + existing sync logic. |
| HTTP sync endpoint | 1 | `POST /api/v1/sync` — push/pull memories over the existing HTTP API. Agents on different machines sync via network. |
| Selective sync | 1 | Sync only specific namespaces or scopes. Team agents sync team memories. Private stays private. |

### Use Cases This Enables

**Shift handoff:** Night shift agents sync to day shift agents automatically. Zero knowledge gap.

**Swarm intelligence:** 100 agents processing tickets. Each learns. Memory syncs across the swarm. The 100th ticket benefits from all 99 prior learnings.

**Knowledge inheritance:** Server decommissioned. Agent dies. Replacement agent syncs and has full institutional knowledge from day one. Zero onboarding.

**Distributed immune system:** One agent encounters a novel failure pattern. Stores the signature. Syncs to all agents in the mesh. Entire fleet immunized.

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

| Phase | Milestone | Sessions | What It Unlocks |
|:-----:|-----------|:--------:|-----------------|
| 0 | Foundation (v0.5.4) | 5-7 | **COMPLETE** |
| 1 | Smart Recall + Agent Identity (v0.6.0) | 6-8 | Budget-aware recall, agent-attributed memories |
| 2 | Knowledge Graph Engine (v0.7.0) | 5-7 | Connected knowledge, multi-hop reasoning |
| 3 | Multi-Agent Sync (v0.8.0) | 8-10 | Collective intelligence across systems |
| 4 | Autonomous Curator (v0.9.0) | 4-6 | Self-improving memory |
| 5 | Team Hub (v0.9.5) | 4-6 | Organizational shared knowledge |
| 6 | Production GA (v1.0.0) | 6-8 | Stability contract, SDKs, portability |
| 7 | Federation & Protocol (v1.x+) | 8-12 | Infrastructure for humanity |
| | **TOTAL** | **47-64** | |

---

## What We Will NOT Build

Decisions made, not deferred. These are intentional exclusions:

- **10 database backends** — SQLite is the backbone. PostgreSQL is the one optional hub. No Qdrant, Pinecone, Weaviate, Milvus, Neo4j, SurrealDB, TiDB, Redis, MongoDB.
- **StorageBackend trait abstraction** — Killed. Zero user value, high complexity. If PostgreSQL is needed, it gets its own implementation, not a generic trait.
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
