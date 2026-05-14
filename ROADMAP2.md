# ai-memory — Roadmap v2 (Consolidated, Audit-Reconciled, Evidence-Backed)

> **Document classification:** Public-facing strategic roadmap.
> **Date:** 2026-04-29
> **Supersedes:** the prior `ROADMAP.md` (Phase 0–6, drafted at v0.5.4.4) and the 2026-04-29 charter-set roadmap. Where they conflict, this document wins.
> **Trademark:** ai-memory™ — USPTO Serial No. 99761257
> **License:** Apache 2.0 — permanent, non-revocable, non-relicenseable.
> **Production version at write time:** v0.6.3.1 (shipped 2026-04-30; this audit's text dates 2026-04-29 use "v0.6.3" inline because Patch 1 had not yet shipped at the time of writing — the contract has since landed and §7.2 is now SHIPPED).

---

## 0. Executive position in one paragraph

Everything that compiles into the `ai-memory` binary is Apache 2.0, forever. There is no closed-source roadmap. There is no commercial-only feature. There is no "open-core" gotcha where the substrate is free but the useful parts cost money. The four-charter set and the prior phased roadmap are reconciled here: every engineering deliverable in either is OSS, every gap surfaced in the v0.6.3 source-code audit has a slot, every commitment that vanished in the prior rewrite is recovered or formally cut. A managed-service deployment tier consumes this substrate but paywalls none of it.

---

## 1. North Star

**AI endpoint memory is a primitive, not a product.**

AI agents are stateless by default. Every session starts from zero. Models get replaced. Vendors shut down. Infrastructure gets rebuilt. The knowledge disappears with them.

ai-memory makes knowledge persistent. What agents learn survives the agent, the model, the vendor, and the platform. One agent learns it, every agent knows it — across systems, across teams, across time.

No AI agent should ever have to relearn what any AI agent already knows.

---

## 2. Design philosophy — non-negotiable

- **Zero tokens until recall.** Memory is not loaded into context until explicitly requested.
- **Zero infrastructure.** A single SQLite file is the default deployment.
- **Zero latency.** Local-first, no network calls in the hot path.
- **Zero lock-in.** MCP-compatible with any AI client. Apache 2.0 forever.
- **Zero knowledge loss.** Agents die, models change, memories survive.

SQLite is the backbone. Local-first is the moat. Every feature must preserve this.

---

## 3. Execution model

**Human-led, AI-accelerated development.** Humans maintain full oversight over all AI code implementations. AI coding agents (Claude Code, Codex, Grok, others) propose; humans approve.

- **Owner & gatekeeper** — `@alphaonedev` approves all merges to `main` (CODEOWNERS enforced).
- **Architect** — humans make all design decisions.
- **Quality gate** — humans vet all code against engineering standards.
- **Contributors** — both human developers and human-supervised AI coding sessions.

**LOE unit** = 1 session = one focused AI-assisted coding interaction producing human-reviewable output.

---

## 4. State of the world at v0.6.3 — evidence baseline

This is the floor every plan below builds on. Numbers are sourced from the public test hub and the published benchmark page.

### 4.1 Test coverage and gates

| Metric | Result | Source |
|---|---|---|
| Library tests passing (v0.6.3.1) | 1,886 / 1,886 (was 1,600 on v0.6.3) | release notes |
| Total tests (lib + integration, v0.6.3.1) | 1,886 lib + 49+ integration | release notes |
| Line coverage (v0.6.3.1) | **93.84%** (gate ≥93%, buffer +0.84pp) | release notes |
| Region coverage (v0.6.3 baseline) | 93.11% | evidence.html |
| Function coverage (v0.6.3 baseline) | 92.55% | evidence.html |
| Modules ≥ 90% coverage (v0.6.3 baseline) | 39 of 47 (7 at 100%) | evidence.html |
| Platform CI matrix | ubuntu-latest, macos-latest, windows-latest | evidence.html |
| Schema version (v0.6.3.1) | v19 (was v15 on v0.6.3; ladder v15→v17→v18→v19) | release notes |
| Schema version (v0.7.0 in-flight, as of L1-5 `3aa00c5`) | **v30** (sqlite) — ladder v19 → v20 (v0.6.4 audit log) → v22 (v0.7.0 RC: attestation + recursive-learning inclusion) → v29 (L0.7-1 base, recursive-learning Task 1/8 reflection_depth) → v30 (L1-1) → further bumps queued for B1 CHECK constraints + A4 L1-6 Goal | commit log on `feat/v0.7.0-layer-1` |

> **Doc-vs-substrate qualifier.** Schema versions can advance ahead of this document during in-flight work; the doc is updated at every layer §16 gate. Numbers in this row are "as of" the named commit on the named integration branch.

### 4.2 Ship-gate (4 phases on 4-node DigitalOcean)

| Phase | Result | Wall time |
|---|---|---|
| Phase 1 — Functional (single-node CRUD, MCP handshake, curator) | ✅ green | 3 s |
| Phase 2 — Federation (W=2 of N=3 quorum, eventual consistency) | ✅ green | 1 m 56 s |
| Phase 3 — Migration (SQLite ↔ Postgres round-trip idempotency) | ✅ green | 1 m 25 s |
| Phase 4 — Chaos (50× kill_primary_mid_write, convergence ≥0.995) | ✅ green | 5 m 24 s |
| **Total** | **4/4** | **~14 m** |

### 4.3 A2A-gate (multi-framework × multi-transport matrix)

| Cell | Status at v0.6.3 |
|---|---|
| ironclaw / off | green |
| ironclaw / tls | green |
| ironclaw / **mtls** (certification cell) | **green — 48/48 scenarios** |
| hermes / off | green |
| hermes / tls | green |
| hermes / mtls | green |
| mixed-framework × {off,tls,mtls} | blocked on terraform topology (not ai-memory) |

- A2A campaign wall: ~28 m total
- Composition: 35 baseline scenarios + 4 auto-append + 9 new for v0.6.3
- v0.6.2 prior cert: 37/37 mTLS, 35/35 TLS, 35/35 off (2026-04-24)

### 4.4 Distribution channels (5 of 5 live)

- crates.io · Homebrew · Fedora COPR · Docker GHCR · APT PPA
- All five published smoke-tested at v0.6.3 cut. PR #466 merged 21:48:22 UTC. Pipeline run #25021409589.

### 4.5 LongMemEval — published

| Metric | Result |
|---|---|
| Recall@5 | **97.8%** (489/500) |
| Recall@10 | 99.0% (495/500) |
| Recall@20 | 99.8% (499/500) |
| Throughput (keyword) | 232 q/s (2.2 s for 500 questions) |
| Throughput (LLM-expanded) | 142 q/s (3.5 s) |
| Cloud cost | $0 |

ICLR 2025 benchmark, pure SQLite FTS5+BM25, zero cloud. **This score has shipped — it is not a v0.6.3.1 deliverable.** What v0.6.3.1 owes is the reranker-on / reranker-off / curator-on variants for full quality-range disclosure.

### 4.6 Performance budgets (Apple M2, 16 GB, SQLite reference)

| Operation | Tier | p95 budget |
|---|---|---|
| memory_store | keyword | ≤ 5 ms |
| memory_store | semantic | ≤ 25 ms (MiniLM 384d) |
| memory_store | autonomous | ≤ 60 ms (nomic 768d) |
| memory_get | any | ≤ 2 ms |
| memory_search | keyword | ≤ 8 ms |
| memory_recall | semantic | ≤ 35 ms (FTS5 70% / HNSW 30%) |
| memory_recall | autonomous | ≤ 90 ms (cross-encoder 100→10) |
| memory_link | any | ≤ 4 ms |
| memory_promote | any | ≤ 8 ms |
| memory_consolidate | smart | ≤ 1500 ms (LLM-bound) |
| memory_kg_query | any | ≤ 50 ms (depth 3, <1k edges) |
| memory_get_taxonomy | any | ≤ 30 ms (depth 8) |
| memory_archive_purge | any | ≤ 200 ms / 1000 rows |
| sync_push | any | ≤ 15 ms (TLS 1.3) |
| bulk_create | any | ≤ 2000 ms (100 rows + fanout) |

CI guard: `bench --baseline performance/baseline.json` fails any PR that exceeds budget by >10%.

### 4.7 Surface area shipped

- **43 MCP tools** (audit confirmed: zero stub handlers; three are tier-gated and return explicit `Err` when LLM/embedder absent)
- **42 HTTP endpoints**
- **26 CLI commands**
- **4 feature tiers:** keyword (FTS5 only) · semantic (+ MiniLM 384d) · smart (+ Ollama LLM) · autonomous (+ nomic 768d + cross-encoder rerank)
- **3 memory tiers:** short (6 h) · mid (7 d) · long (permanent)
- **6-factor recall scoring:** FTS relevance · priority · access count · confidence · tier boost · recency decay

**v0.7.0 grand-slam ship (as of `feat/v0.7.0-layer-1` HEAD, registry-confirmed):**

- **52 MCP tools total** at v0.7.0 grand-slam ship: 43 baseline + `memory_reflect` (recursive-learning Task 4/8) + `memory_skill_register` / `_list` / `_get` / `_resource` / `_export` (L1-5 Agent Skills, **5 tools**, not 6 — `memory_skill_promote_from_reflection` is L2-6 and ships in v0.8.0) + `memory_load_family` (B1 always-on) + `memory_subscribe_replay` / `_dlq_list` (K7 subscription reliability)
- **25 hook events on `l1/compaction-pipeline`** at L1-7 ship: 20 baseline (Bucket 0 plan) + `pre_recall_expand` (G10 hot-path) + `pre_reflect` + `post_reflect` (recursive-learning Task 6/8, `21 → 23`) + `pre_compaction` + `on_compaction_rollback` (L1-7, `23 → 25`). v0.7.0 RC base before recursive-learning lands ships **22 events** (20 + 2 from G10 + Bucket 0 substrate); the `23` floor sits on `feat/v0.7.0-recursive-learning`; the `25` floor sits on `feat/v0.7.0-layer-1` after L1-7 lands

> **Doc-vs-substrate qualifier.** The hook count and tool count in this block are "as of" the named integration branch HEAD at the time of writing. Both can advance in subsequent layer work; the doc is updated at every §16 gate.

### 4.8 Certification posture (cold honesty)

- **A2A-Certified internal:** yes (v0.6.2 + v0.6.3)
- **Ship-Gate internal:** yes (9/9 certifications + 5/5 channels green at v0.6.2 cut)
- **Third-party compliance held:** none (no SOC 2 / ISO 27001 / FedRAMP / HIPAA)
- **Cryptographic agent attestation:** schema column reserved (`memory_links.signature`); not implemented in v0.6.3 (lands v0.7 Bucket 1)
- **Multi-region distributed consensus:** vision for v1.0+; not in v0.6.3

---

## 5. Source-code audit findings — what the code actually does (v0.6.3, commit 8a584a2)

A six-agent parallel audit of every line covering storage, recall, tool surface, auto-features, governance, and KG/lifecycle produced 22 distinct findings. Categorized and mapped below.

### 5.1 Real and load-bearing (use confidently)

- **Hybrid recall** — FTS5 + HNSW (`instant-distance`), content-length-adaptive blend `w·cos + (1-w)·norm_fts`, exponential time decay. Both branches do real work.
- **Cross-encoder rerank** — `cross-encoder/ms-marco-MiniLM-L-6-v2` via candle-CPU; 0.6·orig + 0.4·CE blend; serialized through a `Mutex<BertModel>`.
- **KG query** — recursive CTE on `memory_links`, max depth 5, bitemporal (`valid_from`/`valid_until`), cycle-safe path tracking.
- **Approval gate** — wired end-to-end on store/delete/promote when a namespace has explicit `metadata.governance` policy. Pending actions queue, Human/Agent/Consensus(N) approvers, execute-on-final-approval.
- **N-level namespace chain** — `build_namespace_chain` walks `/`-derived ancestors plus explicit `parent_namespace`, depth 8, cycle-safe. **For display.** (See §5.4 for the gap.)
- **TTL-based GC** — real, optional archive-before-delete, idempotent.
- **Webhook signing** — HMAC-SHA256, SSRF guard, secret hashed at rest.
- **Migration discipline** — schema v15, BEGIN EXCLUSIVE wrappers, WAL mode, foreign keys ON.

### 5.2 Real but narrower than the docs imply

- **Auto-consolidation** — lexical Jaccard clustering (threshold 0.55, max 8/cluster), then one LLM summarize call per cluster. **No embeddings used in clustering.**
- **Auto-tagging** — single canned prompt to Ollama, line-split + lowercase. **No vocabulary, no validation against existing tags.**
- **Contradiction detection** — FTS title match (top 5 same-namespace) → yes/no LLM string match. **Not embedding-based.**
- **Hybrid recall namespace filter** — applied **post-ANN, in Rust**, not pre-ANN. Small namespaces can return zero semantic results when ANN top-50 is dominated by other namespaces. **Production hazard.**
- **Knowledge "graph"** — recursive CTE on a single 5-column links table. **No graph engine, no query language.** (Cypher-on-AGE planned for v0.7 Bucket 2.)
- **`memory_get_taxonomy`** — namespace folder counts via `GROUP BY namespace`. **Not a tag taxonomy.**
- **Promote** — default = column flip (`tier='long', expires_at=NULL`); `--to-namespace` mode = clone + `derived_from` link. **Not a typed state machine.** (Becomes one in v0.8 Pillar 2.)
- **Embeddings** — MiniLM is in-process candle; nomic 768d is **delegated to Ollama HTTP sidecar** despite docs implying native. Cold-start = HF download or Ollama daemon required.

### 5.3 Capabilities-JSON theater (advertised, not implemented in v0.6.3)

| Capability flag | Reality | Roadmap home |
|---|---|---|
| `memory_reflection: true` | No `reflect()` function exists. Pure advertisement. | Reword in v0.6.3.1 capabilities v2; lands v0.7+ |
| `permissions.mode: "ask"` | Hard-coded constant; never read by gate | v0.7 Bucket 3 |
| `approval.default_timeout_seconds: 30` | Reported, never enforced (no sweeper) | v0.7 Bucket 3 |
| `approval.subscribers: 0` | Hard-zero; no API to subscribe | v0.7 Bucket 3 |
| `hooks.by_event: {}` | Always empty; no event registry | v0.7 Bucket 0 |
| `rule_summary: []` | Always empty | v0.7 Bucket 3 |
| `compaction.enabled: false` | No daemon code in v0.6.3 (placeholder for v0.8 Pillar 2.5) | v0.8 Pillar 2.5 |
| `transcripts.enabled: false` | No capture path in v0.6.3 (placeholder for v0.7 Bucket 1.7) | v0.7 Bucket 1.7 |

### 5.4 Substantive gaps and bugs (priority-ordered)

| # | Finding | Severity | Roadmap home |
|---|---|---|---|
| **G1** | **Namespace inheritance applied to standard *display* but NOT to governance *enforcement*.** `resolve_governance_policy` checks the leaf only. Children of a governed parent are completely ungoverned. | **High** (security-shaped) | **v0.7 Bucket 3 — cutline-protected** |
| G2 | HNSW capped at 100k entries with **silent oldest-eviction** (`hnsw.rs:19,107`). No telemetry. | High | v0.7 Bucket 0 (eviction event) |
| G3 | HNSW is **in-memory only**; rebuilt cold on every restart (O(N) read of all embeddings) | Medium | v0.9 (paired with default-on rerank) |
| G4 | Mixed embedding dims (384 vs 768) **silently tolerated** at schema level — cosine returns 0.0 on mismatch | Medium-High (data integrity) | v0.6.3.1 |
| G5 | `archived_memories` has **no embedding column** — archive lossy for vector search. Restore resets `tier='long'` + `expires_at=NULL` | Medium | v0.6.3.1 |
| G6 | `UNIQUE(title, namespace)` + INSERT-on-conflict **silently mutates** existing row instead of erroring | Medium | v0.6.3.1 |
| G7 | Reranker `Mutex<BertModel>` **serializes** all parallel recalls. ~10–50 ms/doc CPU forward pass | Medium-High under concurrency | v0.7 Bucket 0 (batch), v0.9 (pool) |
| G8 | Cross-encoder **silently falls back to lexical Jaccard** on HF download fail. No request-time signal | Medium | v0.6.3.1 (capabilities v2) |
| G9 | Webhooks fire on `memory_store` only — **promote/delete/link/consolidate are silent** | Medium | v0.6.3.1 (or v0.7 Bucket 0) |
| G10 | `memory_expand_query` **never auto-invoked** from inside recall — caller must wire it themselves | Low (intentional under "zero tokens until recall") | v0.7 Bucket 0 (`pre_recall` hook opt-in) |
| G11 | Embedder silent degrade to keyword-only when nomic/Ollama down — recall still returns, no signal | Low-Medium | v0.6.3.1 (capabilities v2) |
| G12 | `memory_links.signature` column exists but is **never written nor verified** | Medium | v0.7 Bucket 1 (already scoped) |
| G13 | Cross-arch **endianness** in stored f32 BLOBs — silently corrupts under cross-arch federation | Low now, painful later | v0.6.3.1 |
| G14 | `kg_invalidate` has no audit column | Low | v0.7 Bucket 2 |
| G15 | Stats live-counted (no cache) — fine at 152 entries; profile at scale | Defer | watch only |
| G16 | Schema migration v16 is no-op for SQLite (alignment with Postgres) | Doc | doc fix |

### 5.5 Public-surface lag (not a code bug, an ops bug)

| Surface | Stale state | Action |
|---|---|---|
| `ai-memory-ship-gate` landing page | Latest documented = v0.6.0.0 (Campaign r25, 2026-04-20). v0.6.3 results NOT on landing page despite being green | v0.6.3.1 ops |
| `ai-memory-ai2ai-gate` landing page | Latest documented = v0.6.2 cert (2026-04-24). v0.6.3 48/48 result not surfaced. v3r23 still cites unresolved S18/S39, which v0.6.3 closed | v0.6.3.1 ops |

---

## 6. Recovered commitments from the prior phased roadmap

The `ROADMAP.md` (Phase 0–6, drafted at v0.5.4.4) made commitments that did not survive the rewrite into the charter set. Cross-walked against actually-shipped v0.6.3:

| Commitment | Phase | Audit status | Disposition |
|---|---|---|---|
| `metadata` JSON column, `agent_id`, agent registration | 1a | ✅ shipped | done |
| Hierarchical namespace paths, visibility prefixes, vertical promote | 1b | ✅ shipped | done |
| **N-level rule inheritance** | 1b | ⚠️ display only — gate uses leaf only | **G1 fix in v0.7 Bucket 3** |
| Governance metadata, roles, approval workflow, approver types | 1c | ✅ shipped | done |
| **`budget_tokens` parameter for context-budget-aware recall** | 1d | ✅ shipped (v0.6.3.1 R1, with cl100k_base BPE tokenization) | done |
| Hierarchy-aware recall (auto-include ancestors) | 1d | ✅ shipped (FTS expansion) | done |
| `memory_graph_query` (multi-hop) | 2 | ✅ shipped as `memory_kg_query` | done |
| **`memory_find_paths` (A→B path discovery)** | 2 | ❌ MIA | **R2 — recover in v0.7 Bucket 2 alongside AGE** |
| **Auto link inference** (LLM-detected `related_to`/`contradicts` on store) | 2 | ❌ MIA | **R3 — recover in v0.7 Bucket 0 as `post_store` hook** |
| Temporal reasoning (point-in-time queries) | 2 | ✅ shipped (`valid_from`/`valid_until`) | done |
| CRDT-lite merge rules, vector clock | 3a | ⚠️ partial (`sync_state` table; merge rules underspecified) | v0.8 Pillar 3 |
| Peer sync daemon, HTTP endpoint, selective sync | 3b | ✅ shipped (HTTP API + federation) | done |
| Background curator daemon | 4 | ⚠️ code in `autonomy.rs`/`curator.rs` but no standalone CLI surface | **R4 — surface as `ai-memory curator` daemon in v0.8 Pillar 2.5** |
| **Auto-extraction from conversations** | 4 | ❌ MIA | **R5 — recover in v0.7 Bucket 1.7 as `pre_store` hook on transcripts** |
| **Consensus memory** (4-of-5 → confidence 0.95) | 4 | ❌ MIA (Approval has Consensus(N) for *write authorization*, not *truth determination*) | **R6 — recover in v0.8 Pillar 3** |
| **`ai-memory doctor` health dashboard** | 4 | ✅ shipped (v0.6.3.1 R7, 7-section severity-tagged dashboard) | done |
| PostgreSQL + pgvector hub, hub-spoke topology, migration CLI | 5 | ✅ shipped (Postgres SAL adapter; AGE planned for v0.7) | done |
| API stability guarantee | 6 | pending v1.0 | v1.0 |
| **Plugin SDK Python + TypeScript** | 6 | ❌ explicitly cut | **stays cut — MCP is the SDK** |
| Memory portability spec | 6 | promoted to v0.6.3.1 | v0.6.3.1 |
| Security audit | 6 | pending v1.0 | v1.0 |
| **TOON v2 schema inference** (85%+ token reduction) | 6 | ❌ MIA in new roadmap | **R8 — recover or formally cut in v0.9** |

---

## 7. Releases — consolidated forward plan

### 7.1 v0.6.3 — Structured Memory + Performance — SHIPPED 2026-04-27

The grand-slam. Six streams (A: hierarchy taxonomy · B: schema v15 with temporal columns + signature placeholder · C: KG query/timeline/invalidate + entity registry · D: duplicate detection · E: bench tool · F: PERFORMANCE.md + bench.yml CI guard).

Status: **done**. See §4 for evidence.

### 7.2 v0.6.3.1 — Honesty Patch + Recovered Commitments + Doc Currency — SHIPPED 2026-04-30

Existing scope: **Capabilities v2 + Memory Portability Spec v1**. (LongMemEval already shipped at v0.6.3 — replaced with reranker-variant disclosure.)

#### Capabilities v2 — honesty (closes §5.3 theater)

- v2 schema reports honest live state: `recall_mode_active: "hybrid" | "keyword_only" | "degraded"`, `reranker_active: "neural" | "lexical_fallback" | "off"`, `permissions.mode: "advisory"` (until v0.7), drop `subscribers` / `by_event` / `rule_summary` / `default_timeout_seconds` until populated, mark `memory_reflection` as planned-not-implemented.
- v1 client compatibility preserved via `schema_version` discriminator.

#### Data integrity (closes G4, G5, G6, G13)

- Add `embedding_dim` column to `memories`; refuse mixed-dim writes; surface `dim_violations` count in stats.
- Add `embedding`, `original_tier`, `original_expires_at` columns to `archived_memories`; restore preserves originals.
- `memory_store` gains `on_conflict: "error" | "merge" | "version"` parameter. Default for new clients: `error`. Legacy `merge` opt-in.
- Endianness magic byte on stored f32 BLOBs (cheap now, painful after federation).

#### Webhook event coverage (closes G9)

- Wire `dispatch_event` into `promote`, `delete`, `link`, `consolidate` paths. Existing signing/SSRF unchanged.

#### Recovered commitments

- **R1 — `budget_tokens` parameter on `memory_recall`.** Token-counted greedy fill; return as many ranked memories as fit. ~3 sessions. **Highest-leverage recovery in the plan.** Lifts the killer feature into the OSS surface and pairs with the LongMemEval reranker-variant disclosure.
- **R7 — `ai-memory doctor` CLI.** Reports fragmentation, stale-with-no-recall, unresolved contradictions, sync lag, dim violations, eviction count, channel-publish status. Reads Capabilities v2 + ad-hoc SQL. ~2 sessions.

#### Memory Portability Spec v1

- Schema + JSON export format + TOON wire format documented as a public standard at `memory.dev/spec/v1` (or equivalent). Establishes the data model as a category standard.

#### LongMemEval reranker-variant disclosure

- Already-published R@5 97.8% / R@10 99.0% / R@20 99.8% gets companion runs: reranker-on / reranker-off / curator-on. Methodology repo, reproducibility scripts, charts.

#### Public-surface currency (closes §5.5)

- Update `ai-memory-ship-gate` landing page to show v0.6.3 4/4 phases green (currently lags at v0.6.0.0).
- Update `ai-memory-ai2ai-gate` landing page to show v0.6.3 48/48 mTLS cert (currently lags at v0.6.2). Mark S18/S39 as resolved (closed during v0.6.3 campaign).
- Automate landing-page sync: each ship-gate run posts the result JSON; the page reads it.

#### v0.6.3.1 cutline if slipping

Keep: Capabilities v2 honesty, R1 (`budget_tokens`), G4 (embedding_dim integrity), public-surface currency.
Defer: G5/G6/G9, R7 (doctor), TOON wire format polish.

**Effort:** ~17 sessions on top of original Cap v2 scope. Realistic: 4 weeks.

### 7.3 v0.7 — Trust + A2A Maturity — Q2 2026 (June target)

#### Bucket 0 — Hook Pipeline

Programmable lifecycle events at every memory operation point. Subprocess JSON-over-stdio with daemon-mode IPC for hot paths.

- 20 lifecycle events at plan time (16 base + 2 compaction + 2 transcripts). **Actual grand-slam ship is 25** on `feat/v0.7.0-layer-1` (20 plan + `pre_recall_expand` G10 + `pre_reflect` + `post_reflect` recursive-learning Task 6/8 + `pre_compaction` + `on_compaction_rollback` L1-7); see §4.7 grand-slam block for the ladder.
- Decision types: `Allow` / `Modify(MemoryDelta)` / `Deny` / `AskUser`.
- Chain ordering by priority with first-deny-wins short-circuit.
- Hard timeouts per event class (5000 ms write, 2000 ms read).
- `~/.config/ai-memory/hooks.toml` config with hot reload.
- `post_recall` and `post_search` default `mode = "daemon"` to preserve the v0.6.3 50 ms-recall budget. `mode = "exec"` requires explicit override.
- Existing `subscriptions` system continues to work; hooks are additive.

**Audit absorbs:**
- G2 — emit `on_index_eviction` hook event with evicted_id; surface eviction count in stats.
- G7 — reranker batching (Mutex throughput): group concurrent requests, run one forward pass over the union, demux. (Pool-of-N comes in v0.9 alongside default-on rerank.)
- G10 — `pre_recall` daemon-mode hook for opt-in query expansion (`memory_expand_query` becomes pipeable without violating "zero tokens until recall").

**Recoveries absorb:**
- **R3 — Auto-link inference** as `post_store` daemon-mode hook. LLM examines stored content vs recent neighbors, proposes `related_to`/`contradicts` links. Default off; opt-in per namespace. ~3 sessions.
- **R5 — Auto-extraction from conversations** as `pre_store` hook on transcripts (Bucket 1.7 substrate). ~2 sessions.

#### Bucket 1 — Ed25519 Attested Identity

Fills the v0.6.3 dead `signature` column with real cryptographic attestation.

- Per-agent Ed25519 keypair (operator-supplied, explicit; not derived from agent_id).
- Outbound signing: every `memory_links` write fills the `signature` column.
- Inbound verification: peer accepting a link verifies signature against `observed_by` claim.
- `attest_level` enum: `unsigned` / `self-signed` / `peer-attested`.
- Append-only `signed_events` audit table.

**Exit criteria:** `verify()` returns `signature_verified: true` for at least one signed link in the test corpus. (Closes G12.)

**Out of OSS scope:** Hardware-backed key storage (TPM/HSM/Secure Enclave) deployment. The OSS provides the *abstraction*; the certified-managed *deployment* is the commercial-service tier.

#### Bucket 1.7 — Sidechain Transcripts

Raw conversation/reasoning trail in zstd-compressed BLOBs, linked to derived memories via `memory_transcript_links`.

- Default off (opt-in per namespace).
- Audit-required namespaces opt in.
- Zstd level 3 compression (5–10× typical ratio).
- Per-namespace TTL with archive → prune lifecycle.
- `memory_replay <id>` reconstructs the transcript chain from a memory.
- Substrate for R5 auto-extraction.

#### Bucket 2 — Apache AGE Acceleration

Postgres SAL adapter detects AGE extension and projects `memory_links` as a property graph for Cypher access. Recursive CTE path stays as the SQLite fallback.

- `memory_kg_query`, `memory_kg_timeline`, `memory_kg_invalidate` gain Cypher implementations on AGE-enabled Postgres.
- Dual-path test discipline: same query on AGE-Postgres vs CTE-SQLite produces identical results.
- PERFORMANCE.md updated with separate p95/p99 budgets for AGE-mode and CTE-mode.
- Bench gate: AGE-mode p95 ≥30% faster than CTE-mode at depth=5 (else AGE complexity isn't justified).

**Audit absorbs:**
- G14 — `kg_invalidate` audit edge in Cypher path.
- Hybrid recall namespace pre-filter (short-term ANN over-fetch heuristic for small namespaces; long-term per-namespace HNSW shard or `sqlite-vec` migration in v0.9).

**Recoveries absorb:**
- **R2 — `memory_find_paths(source, target)`** MCP tool. Cypher one-liner on AGE; recursive CTE on SQLite fallback. ~2 sessions.

#### Bucket 3 — A2A Maturity + Subscription Reliability + Per-Agent Quotas + Permissions + Approval API

Refactors the existing `governance` system into the rules+modes+hooks model; extends existing `pending_actions` with SSE + HMAC + `remember=forever`.

- A2A: correlation IDs, ACKs with retry, TTL, message-replay protection.
- Subscription reliability: retry-on-5xx, DLQ, replay-from-cursor, HMAC signing.
- Per-agent rate limits and storage caps.
- Permission system: rules + modes + hooks → decision, deny-first/ask-by-default.
- Approval API: HTTP + SSE + MCP, with `remember=forever` progressive trust.
- HMAC signing for approval API is **non-optional**.
- Migration tooling: `ai-memory governance migrate-to-permissions` CLI.

**Audit absorbs:**
- **G1 — Namespace inheritance enforcement (cutline-protected).** `resolve_governance_policy` walks `build_namespace_chain`, not just leaf. First non-null policy wins. Inheritance config flag per-policy: `inherit: bool` (default true). Adds ship-gate test: parent has `Approve` policy, child has none → write to child must require approval. **Even if everything else slips, this fix ships.** ~4 sessions.
- Pending-action timeout sweeper (`default_timeout_seconds` becomes real) — single SELECT-and-update on a 60 s timer.
- `permissions.mode` actually consulted by gate.
- Approval-event routing through existing subscription system (`approval.subscribers` becomes real).
- `rule_summary` populated.

#### v0.7 cutline if slipping

Keep: Bucket 0, Bucket 1, Bucket 1.7, Bucket 2, **G1 inheritance fix**.
Defer to v0.7.1: A2A test scenarios full sweep, per-agent quotas, full governance-to-permissions migration.

### 7.4 v0.8 — Distributed Coordination Substrate — Q4 2026

**Document classification:** AI-NHI-advised, deconflicted with ROADMAP2.md baseline at the v0.7.0 → v0.8.0 transition.
**Anchor reference:** AI-NHI advisory dated 2026-05-11 (final after v0.7.0 ship).
**Deconflict discipline:** Every item below is checked against this file's §5 audit findings, §6 recovered commitments, and §7 release plan. Net-new items vs the original 2026-04-29 §7.4 are flagged explicitly.

**Executive position.** v0.8.0 is the **Distributed Coordination Substrate** release. The original §7.4 charter scoped three pillars (Distributed Task Queue, Typed Cognition, CRDTs) plus Pillar 2.5 (Compaction Pipeline). This expansion adds three new primitives to Pillar 1 (signed signals, attested checkpoints, routines) borrowed from the rohitg00/agentmemory competitive analysis but differentiated by cryptographic non-repudiation and federation across organizational trust boundaries. It adds five strategic adjacencies surfaced during v0.7.0 ship-window analysis: Claude Code plugin marketplace install, vLLM as first-class inference backend, LongMemEval Gemma 4 refresh, model-signature verification chain, and a real-time WebSocket viewer. **Total net add against the prior §7.4 scope: +22.5 sessions** (8.5 for coordination expansion + 14 for adjacencies + cross-cutting). New v0.8 total: ~47 sessions. Compatible with Q4 2026 ship target at the demonstrated cadence.

#### Competitive landscape — what changed during the v0.7.0 ship window

| Reference | Strategic posture | v0.8 implication |
|---|---|---|
| **Anthropic Managed Agents** (dreaming / outcomes / multiagent orchestration, 2026-05-06) | Two-markets, not one-market. Anthropic owns managed-memory inside Claude. ai-memory owns substrate-ownership outside Claude (regulated multi-org, air-gapped, customer hardware, vendor-failure resilient). | No scope change. The positioning is durable; their orchestration runs within a single Anthropic-managed deployment, this substrate's federation runs across organizational trust boundaries. |
| **rohitg00/agentmemory** (v0.7.2, Apr 2026) | Apache 2.0, ~20K LOC TypeScript on iii-engine, 581 tests, 41 MCP tools, triple-stream retrieval, 4-tier consolidation, P2P mesh sync. Wins on developer-experience polish. | Three primitives belong in this substrate (signals, checkpoints, routines — expanded below). Three developer-experience adjacencies in §4.8.x: Claude Code plugin marketplace install, WebSocket viewer, bi-directional CLAUDE.md sync. Sentinels and sketches stay deferred — runtime-layer, not substrate. |
| **Muvon/octocode** | Code-search-and-graph tool. Different product category. | No scope change. Confirms "Apache 2.0 + Rust + MCP" alone is not a differentiator — substrate ownership, federation, and forensic substrate must carry the strategic position. |

#### Pillar 1 — Distributed Coordination Substrate (expanded)

Original §7.4 Pillar 1 (preserved verbatim): action/task with state machine (pending → claimed → in_progress → done | failed | abandoned), dependency-DAG enforcement, lease + heartbeat for resilience, federation-aware W-of-N quorum on shared namespaces. Baseline ~12.5 sessions.

##### Already in baseline (restated)

- `memory_action_create / update / transition / delete / query / dag` MCP tools
- Action state machine with substrate-enforced transitions
- Dependency DAG with typed edges (`requires` / `unlocks` / `blocks` / `gated_by` / `sibling`)
- Lease + heartbeat for resilience (sweeper releases expired leases, emits `signed_events` audit entry)
- Federation-aware quorum claiming (W-of-N agreement among peer namespaces required for transitions)
- Vector clock per `action_id` for federation merge
- `memory_lease_acquire / renew / release / query` MCP tools

##### NEW — Signed signals (+3 sessions)

Multi-agent coordination across federation boundaries with cryptographic non-repudiation. Today this happens via shared-memory polling or out-of-band channels (Slack, webhooks) — neither auditable, tamper-evident, nor federation-aware. Signals are first-class memory: durable, queryable, federation-replicable, cryptographically attested.

**Differentiated from competitors.** agentmemory signals carry read receipts but no cryptographic guarantee. This substrate's signals are Ed25519-signed by the sender (reusing v0.7.0 Track H attestation infrastructure), verified on read, hash-chained into `signed_events`. Sender cannot repudiate. Recipient cannot fabricate. Audit chain is procurement-defensible.

Data model (additive to the v0.7.0 grand-slam terminal schema, see §4.1 ladder):

```sql
CREATE TABLE signals (
    id              TEXT PRIMARY KEY,
    namespace       TEXT NOT NULL,
    from_agent      TEXT NOT NULL,
    to_agent        TEXT,                         -- NULL = broadcast within namespace
    subject         TEXT NOT NULL,
    body            TEXT NOT NULL,                -- JSON-typed payload
    signal_type     TEXT NOT NULL,                -- authorize | notify | request | response | broadcast
    in_reply_to     TEXT,                         -- threading
    correlation_id  TEXT,                         -- group related signals
    references      TEXT NOT NULL DEFAULT '[]',   -- JSON array of memory_ids or action_ids
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER,
    delivered_at    INTEGER,
    read_at         INTEGER,
    acknowledged_at INTEGER,
    signature       BLOB NOT NULL,                -- Ed25519 over canonical content
    sender_pubkey   BLOB NOT NULL,                -- explicit, not just agent_id reference
    FOREIGN KEY (namespace) REFERENCES namespaces(name),
    FOREIGN KEY (in_reply_to) REFERENCES signals(id)
);
```

Federation semantics: cross-namespace signal delivery requires sender's pubkey to be allowlisted in recipient's federation peers. W-of-N quorum on signal-creation. **The multi-org-trust-boundary primitive** — a compliance agent in one organization cannot send into another's namespace unless the recipient's federation allowlist includes that agent's pubkey.

MCP tools (5): `memory_signal_send`, `memory_signal_read`, `memory_signal_inbox`, `memory_signal_thread`, `memory_signal_ack`.

##### NEW — Attested checkpoints (+3 sessions) — **cutline-protected**

Regulated workflows require waiting on external conditions before proceeding: compliance review completion, CI green, security scan passed, human approval received, deployment window opened, regulatory clearance issued. Today these conditions live in disparate systems (Jira, ServiceNow, Jenkins, manual email approval, regulator portals). Checkpoints are first-class memory: an external condition with cryptographically attested resolution.

**Procurement-grade-critical.** Separation of duties is a regulatory requirement (SOX §404, FFIEC, HIPAA §164.308(a)(3), GDPR Article 32). A checkpoint primitive with Ed25519 resolution attestation turns SoD enforcement into a substrate-level guarantee: the agent literally cannot proceed past the checkpoint, the resolution is cryptographically attested, the resolver's identity is verified, the audit chain is tamper-evident. Regulators ask about this by name during examination. **No competitor in the field offers this primitive.**

Four condition types: `approval` (resolver pubkey must match approver list), `external_signal` (resolved by inbound signal with matching correlation_id), `condition_predicate` (background evaluator periodically checks namespace state), `deadline` (auto-resolves at timestamp).

MCP tools (4): `memory_checkpoint_create`, `memory_checkpoint_resolve`, `memory_checkpoint_query`, `memory_checkpoint_verify` (returns full attestation chain as procurement-grade evidence packet).

##### NEW — Routines (+2 sessions)

Parameterized action templates with frozen-immutability for regulatory hold. JSON template with action declarations + edge declarations using `{{parameter}}` placeholders. Useful for FFIEC-aligned loan origination, HIPAA-aligned consent capture, drug-trial enrollment checklists. `memory_routine_run` instantiates with parameter values, creating actions + edges + checkpoints atomically.

MCP tools (5): `memory_routine_create`, `memory_routine_freeze`, `memory_routine_run`, `memory_routine_status`, `memory_routine_list`.

##### NEW — Explicit frontier/next MCP surface (+0.5 session)

Agent runtimes (Claude Code, OpenClaw, Cursor) ask "what should I do next" thousands of times across deployments. Surfacing this explicitly removes the need for every runtime to write its own priority-ranking SQL over actions + leases:

- `memory_action_frontier` — return ranked list of currently-unblocked actions (no unmet dependencies, no active lease) in a namespace
- `memory_action_next` — return the single highest-priority unblocked action for the calling agent's namespace permissions

Trivial implementation over Pillar 1 baseline.

##### What is NOT in Pillar 1 scope

- **Sentinels** (event-driven watchers: webhook/timer/threshold/pattern/approval triggers). Straddles substrate/runtime boundary. Defer to v0.9 or v1.0+ research. Promote earlier only on commercial-tier customer demand.
- **Sketches** (ephemeral exploratory action graphs). Developer-experience nicety. Belongs in agent runtime. Decline.
- **LLM-orchestrated action selection.** Substrate exposes frontier; runtime decides. §10 agent-runtime cut applies cleanly.
- **Outbound notification delivery** (email, Slack, webhook outbound for signals). Signals are stored, queryable, federation-replicated. Outbound delivery is integration layer, not substrate.

#### Pillar 2 — Typed Cognition (unchanged from prior §7.4)

- Typed memory enums: `Goal`, `Plan`, `Step`, `Observation`, `Decision`
- Relation taxonomy: `step.advances → plan`, `plan.serves → goal`, etc.
- `memory_cognition_register`, `memory_cognition_query`, `memory_cognition_supersede` MCP tools
- Strict typing validation: Plan must point at Goal; Step at Plan; etc.
- Promote becomes a typed state machine, not a column flip (closes §5.2 narrowness)
- Tag taxonomy as constrained overlay (closes auto_tag uncurated-free-text issue)
- Typed contradiction detection: Decision A vs Decision B on same Goal as candidate set
- Naming hygiene: rename `memory_get_taxonomy` → `memory_namespace_taxonomy`; new `memory_cognition_taxonomy` returns typed-memory distribution

Effort: ~4 sessions baseline.

#### Pillar 2.5 — Compaction Pipeline (unchanged from prior §7.4)

Six-stage with verify+rollback. Maps to typed-cognition supersession.

- Pipeline: dedupe → cluster → eligibility → summarize → persist → verify
- Stage 6 rollback when verify fails
- Pressure triggers calibrated against PERFORMANCE.md p95 budgets
- Bounded compaction subagent: single LLM call, no tools, no loops, structured JSON output
- New hook events `pre_compaction` and `on_compaction_rollback`
- Default `enabled = false` (Ollama dependency)
- Cosine clustering as primary path; Jaccard becomes the cheap pre-filter
- Size-pressure GC triggers (closes "GC is TTL-only")
- **R4 — `ai-memory curator` standalone daemon CLI** wraps Pillar 2.5's compaction + Bucket 0's auto-link-inference + auto-extraction into one operator-visible daemon

Effort: ~5 sessions baseline.

#### Pillar 3 — CRDTs (unchanged from prior §7.4)

- Core CRDT type set: G-Counter (`access_count`), PN-Counter (general counters), LWW-Register with attested-identity tiebreak, OR-Set (`tags`)
- Per-memory vector clock (agent_id → Lamport tick)
- Federation push/pull merges via CRDT semantics (replaces last-writer-wins on `updated_at`)
- Conflict-aware curator: distinguishes mergeable conflicts from human-resolution-required
- LWW-Register tiebreak: ship as `(attestation_level, agent_id, monotonic_local_clock)` with documented consequences
- **R6 — Consensus-based truth determination.** When N agents store conflicting facts, confidence becomes function of agent count (4-of-5 agree → 0.95)

Effort: ~3 sessions baseline.

#### Strategic adjacencies — NET-NEW from v0.7.0 ship-window analysis

##### §7.4.A LongMemEval Gemma 4 refresh — pre-v0.7.0 distribution (+1 session, urgent)

Current state: published numbers ran with gemma3:4b. Production v0.7.0 deploys Gemma 4 throughout. Honesty-discipline gap.

Fix: re-run with `CURATOR_MODEL=gemma4:e4b` on reference hardware. Publish updated R@5/R@10/R@20 plus per-category breakdown alongside reranker-variant disclosure. ~30 minutes of compute. ~1 session of work end-to-end. Belongs *before* v0.7.0 procurement-grade distribution; flagged inside v0.8 §7.4 only because it was identified during strategic-planning sessions and must not drop.

##### §7.4.B Claude Code plugin marketplace install (+1 session)

Today installation requires manual MCP config in `~/.claude.json` — friction wall before first recall. agentmemory's one-line `/plugin install` is procurement-grade developer-onboarding ergonomics this substrate does not yet match.

Fix: create `.claude-plugin/` directory in repo with marketplace manifest. Register the MCP server, the four shipped skills (`/recall`, `/remember`, `/search`, `/forget`), v0.7.0 hooks. Publish to Claude Code plugin marketplace.

##### §7.4.C vLLM as first-class inference backend (per RFC #651) (+5 sessions) — **cutline-protected**

Issue #651 RFC proposes a 7-backend trait architecture with Cargo features for compile-time selection. Make first-class **Ollama + vLLM**, not Ollama + candle:

- Operator ergonomics: vLLM is what regulated enterprise NVIDIA H100/H200/L40S clusters actually deploy at scale (PagedAttention for multi-tenant throughput). candle is fine for in-process but newest model support is hit-or-miss.
- Failure-mode discipline: Ollama HTTP isolates inference failure from daemon. In-process candle crashes the daemon. For 24/7 federation daemons in regulated production, isolation matters more than the 10-30ms HTTP overhead.
- Cross-platform consistency: Ollama works the same on macOS/Linux/Windows/WSL. candle requires per-platform build artifacts.

v0.8.0 scope: implement the trait; keep Ollama as default forever; add vLLM as first-class alternative (OpenAI-compatible HTTP, accommodates customer-managed inference clusters). Defer candle, mistralrs, mlx-rs, llama-cpp-rs, TensorRT-LLM, ChatRTX, MLX-LM-remote to v0.8.x or community-supported.

**Why v0.8.0 not later.** The enterprise NVIDIA cluster path is what procurement-grade buyers ask about. Without vLLM, the commercial deployment tier cannot honestly answer "yes, ai-memory deploys on our H100 fleet at scale."

##### §7.4.D Model signature verification chain (+2 sessions) — strategic IP

Federal procurement asks specifically about supply-chain attestation on model weights. The v0.7.0 Ed25519 attestation infrastructure (already shipping for `memory_links` via Track H) reapplies cleanly to model weights:

| Component | Today | v0.8.0 |
|---|---|---|
| Model digest tracked | implicit (Ollama-supplied) | explicit; written into `signed_events` on first load |
| Model identity attested | no | Ed25519 over `(digest, vendor, version)` by AlphaOne release key |
| Loader verification | trust-on-first-use via Ollama | reject mismatched digest at load; refuse to start if signature absent/invalid |
| Audit chain | not tied to model used | every `signed_events` row carries the `model_digest` that produced it |
| Customer evidence packet | none | `ai-memory model-attest --evidence > packet.json` |

New `model_attestations` table (additive on top of v0.7.0 grand-slam terminal schema, see §4.1 ladder): `model_id`, `model_digest`, `attested_by_pubkey`, `signature`, `attestation_date`, `source_url`. Loader gains `verify_model_attestation()` before model instantiation; refuses to load on signature mismatch. Audit chain records `model_id` with every LLM-derived output.

**Why it's strategic.** Turns "we run Gemma 4" into "we run cryptographically attested Gemma 4 with verifiable supply-chain provenance." Different procurement conversation entirely. Currently **no competitor has this** — neither Anthropic Managed Memory, agentmemory, Total Recall, Hindsight, nor Mastra OM. See issue #654 for the IP-investment dossier.

##### §7.4.E Distilled hot-path model — research, lands v0.8 cycle if corpus collection clears

Investment A from issue #654. Train a small model (300M-700M) on Gemma 4 teacher outputs for the four bounded structured-output tasks (`auto_tag`, `detect_contradiction`, `expand_query`, `summarize_memories`). Ship distilled weights embedded with the published binary; <2GB payload; CPU-only with mlx/wgpu acceleration when available.

| Task | v0.7.0 model | Distilled target | Latency target |
|---|---|---|---|
| `auto_tag` | gemma3:4b @ ~0.7s p50 | 300M | <100ms p50 |
| `detect_contradiction` | gemma4:e4b @ ~3-30s p50 | 300M | <200ms p50 |
| `expand_query` | gemma4:e4b @ ~3-15s p50 | 500M | <500ms p50 |
| `summarize_memories` | gemma4:e4b @ ~5-30s p50 | 700M | <2s p50 |

**Long pole**: training-corpus collection (~100k pairs per task at the Gemma 4 teacher quality bar; $1k-5k API budget). Engineering itself is small (few days on a single H100). Composes with §7.4.D so distilled weights themselves get attested signatures — cryptographic provenance over the entire inference path including substrate-owned weights.

##### §7.4.F Real-time WebSocket viewer (+2 sessions) — v0.8.1 candidate

Optional axum subroute on the HTTP daemon (`ai-memory serve --viewer`) exposing WebSocket stream of memory events + namespace tree + active leases/signals/checkpoints + recent `signed_events`. Default off (security-by-default — no listening port unless explicitly enabled). Protected by an operator-supplied secret when on. Belongs in the next minor after Pillar 1 lands; not blocking v0.8.0.

#### Hook pipeline expansion — v0.7.0 → v0.8.0

v0.7.0 grand-slam ships **25 lifecycle events** on `feat/v0.7.0-layer-1` (20 plan baseline + `pre_recall_expand` G10 + `pre_reflect` + `post_reflect` recursive-learning Task 6/8 + `pre_compaction` + `on_compaction_rollback` L1-7). v0.8.0 adds 10 events for coordination substrate on top of that. Backward compatible.

| Event | Fires at | Decision types |
|---|---|---|
| `pre_action_create` | Before action insert | Allow / Modify(action_delta) / Deny / AskUser |
| `pre_state_change` | Before action transition | Allow / Deny |
| `post_state_change` | After action transition | Notify only |
| `pre_lease_acquire` | Before lease insert | Allow / Deny |
| `on_lease_expire` | When sweeper releases expired lease | Notify only |
| `pre_signal_send` | Before signal write | Allow / Modify(signal_delta) / Deny |
| `post_signal_ack` | After signal acknowledged | Notify only |
| `pre_checkpoint_create` | Before checkpoint write | Allow / Deny |
| `post_checkpoint_resolve` | After checkpoint resolved | Notify only |
| `pre_routine_run` | Before routine instantiation | Allow / Modify(parameters) / Deny |

#### Schema migration — v30 → v3X

v0.7.0 grand-slam ships schema **v30** at L1-5 (sqlite, ladder per §4.1 schema row).
The original §7.4 plan called for v21 (audit log + Ed25519 attestation columns) → v22 (Pillar 1); the in-flight v0.7.0 work consumed v20–v30 ahead of doc-time, so v0.8.0 Pillar 1 expansion lands at **v3X (above v30)** with the additive tables enumerated below — exact terminal version pinned at the §16 gate.

Migration v30 → v3X (above v30):
- Add `actions` + `action_edges` tables
- Add `leases` table
- Add `signals` table
- Add `checkpoints` table
- Add `routines` + `routine_runs` tables
- Add `model_attestations` table (per §7.4.D)

All `CREATE TABLE` operations are additive. No existing table modifications. Migration idempotent + reversible. Discipline matches §11 quality gates.

#### Effort summary — v0.8.0 total scope

| Component | Baseline | Expansion | Total |
|---|---|---|---|
| Pillar 1 — actions/leases/DAG/federation (baseline) | 12.5 | 0 | 12.5 |
| Pillar 1 — Signed signals (NEW) | 0 | +3 | 3 |
| Pillar 1 — Attested checkpoints (NEW) | 0 | +3 | 3 |
| Pillar 1 — Routines (NEW) | 0 | +2 | 2 |
| Pillar 1 — Frontier/next surface (NEW) | 0 | +0.5 | 0.5 |
| Pillar 2 — Typed Cognition | 4 | 0 | 4 |
| Pillar 2.5 — Compaction + R4 curator daemon | 5 | 0 | 5 |
| Pillar 3 — CRDTs + R6 consensus | 3 | 0 | 3 |
| §7.4.B Claude Code plugin marketplace install | 0 | +1 | 1 |
| §7.4.C vLLM first-class inference backend (RFC #651) | 0 | +5 | 5 |
| §7.4.D Model signature verification chain | 0 | +2 | 2 |
| Hook pipeline integration (10 new events) | 0 | +1.5 | 1.5 |
| Schema migration v30 → v3X (above v30) | 0 | +0.5 | 0.5 |
| Test suite (~540 new tests) | 0 | +3 | 3 |
| Documentation + reproducibility scripts | 0 | +1 | 1 |
| **TOTAL** | **24.5** | **+22.5** | **~47 sessions** |

At the demonstrated cadence (4 production releases in 14 days through v0.6.4 + v0.7.0), 22.5 net-new sessions ≈ 6-8 calendar weeks. Compatible with Q4 2026 ship target.

#### v0.8.0 cutline if slipping

**Keep (cutline-protected):**
- Pillar 1 base (actions + leases + DAG + federation) — baseline
- **Attested checkpoints (§Pillar 1 NEW)** — procurement-grade separation-of-duties primitive
- **Pillar 3 CRDT four-primitive set with documented merge** — baseline
- **vLLM first-class inference backend (§7.4.C)** — enterprise NVIDIA path

**Defer to v0.8.1 if substrate ships clean:**
- Routines
- Claude Code plugin marketplace install
- Real-time WebSocket viewer
- Pillar 2 typed cognition

**Defer to v0.9 if slippage severe:**
- Signed signals — keep if possible
- Model signature verification chain

#### The three highest-leverage moves in v0.8.0

Updated from §9 (which named v0.6.3.1, v0.7 G1, and v0.7 hooks). v0.8.0's three are:

1. **Attested checkpoints.** Separation-of-duties primitive that regulators ask about by name. No competitor has it. Cutline-protected — ships even if other Pillar 1 work slips. **The single highest-leverage commitment in v0.8.0.**
2. **Signed signals across organizational trust boundaries.** Cryptographically non-repudiable inter-agent messaging across federation peers. Hardens the federation thesis from "memory sync" to "workflow coordination with cryptographic audit." Pairs structurally with checkpoints (signal can resolve a checkpoint).
3. **vLLM as first-class inference backend.** Closes the "can ai-memory deploy at scale on our NVIDIA H100 fleet" procurement question. PagedAttention is the difference between handling 10 concurrent agents and 1,000 in regulated multi-tenant production.

Bonus strategic IP (not cutline-protected): **model signature verification chain.** Substrate-level supply-chain attestation no competitor offers.

#### Commercial-tier coupling (what v0.8.0 enables)

Note: this section names commercial deployment surfaces in generic terms ("commercial deployment tier", "managed-service offering"). Brand-specific commitments live outside ROADMAP2; everything here is Apache 2.0 substrate.

- **Federate tier:** operational support for cross-org signal allowlist management, checkpoint approver matrix, routine versioning across trust boundaries. New service surface that did not exist before v0.8.
- **Vertical tier (Financial Services):** pre-built routine templates for FFIEC-aligned workflows — loan origination, KYC, AML escalation, suspicious activity reporting. Customers customize parameters; substrate enforces structure and audit chain.
- **Vertical tier (Healthcare):** pre-built routine templates for HIPAA-aligned workflows — consent capture, BAA tracking, breach response, 42 CFR Part 2 release authorization.
- **Attest tier:** procurement-grade evidence packets for separation-of-duties controls. "Here is the cryptographic chain proving Compliance Officer X authorized Action Y at Time T, verifiable independently against the substrate's `signed_events` log."
- **Inference layer:** vLLM first-class backend + model signature verification = the commercial tier can honestly answer two procurement questions today's competitors cannot: (1) "Does ai-memory deploy at scale on our NVIDIA H100 fleet?" — yes, via vLLM with PagedAttention; (2) "How do we know the model wasn't swapped between attestation and inference?" — cryptographic chain from `model_attestations`.

### 7.5 v0.9 — Skill Memories + Function Calling + Default-On Reranker — Q1 2027

- **Skill memories** — `tier=long, namespace=_skills/<id>` formalized as a first-class type with `parameters_schema`, `invocation_record`, `version`. `memory_skill_register`, `memory_skill_invoke`, `memory_skill_list` MCP tools.
- **Function calling in `llm.rs`** — wire local Gemma 4 LLM to a tool-calling protocol so curator passes can use targeted operations rather than blind text generation.
- **Cross-encoder reranker default-on** — closes the published reranker-on quality range. HF-Hub model auto-fetch on first use; **fail loud (`mode: "degraded"`)** when model not available, no silent lexical fallback.
- **Streaming tool responses** — for long-running MCP tools (recall over very large stores, federation broadcasts).

#### Operator-controlled telemetry — v0.7.0 commitment carried forward

`ai-memory` does not phone home. No outbound network call is initiated by the binary except to destinations the operator has explicitly configured (federation peers on the mTLS allowlist, optional HuggingFace embedder fetch, optional Ollama LLM endpoint). All tracing spans go to operator-configured sinks only: stderr by default, opt-in rolling file appender via `[logging]` in `config.toml`, and an OTLP exporter shipping at v1.0 per §7.6. Span content is operation metadata only — `agent_id`, namespace, duration, result — never memory content. `AI_MEMORY_ANONYMIZE=1` redacts the agent_id in externally-visible spans. Full policy: [`docs/telemetry.md`](docs/telemetry.md).

**Audit absorbs:**
- G3 — HNSW persistence to disk (sqlite-vec migration or on-disk index). Removes O(N) cold-start.
- G7 step 2 — BertModel pool sized to physical CPU count (prerequisite for default-on reranker; otherwise Mutex serialization makes default-on a regression).
- G8 — fail-loud reranker fallback in `recall` response.

**Recoveries (optional):**
- **R8 — TOON v2 schema inference** (target 85%+ token reduction). Recover or formally cut. ~2 sessions if recovered.

### 7.6 v1.0 — Federation Maturity + Portability + Audit — Q2 2027

- **Auto-discovery** — mDNS for local-network peer discovery, hardcoded peer list as fallback.
- **End-to-end encryption** — operator-side keys, transport-layer encryption for federation push/pull beyond the existing mTLS layer.
- **MVCC strict-consistency mode** — opt-in per namespace for use cases that need CP rather than AP. CRDTs from v0.8 remain default.
- **OpenTelemetry standardization** — all internal tracing converts to OTel spans.
- **Strict semver discipline** — breaking changes require major-version bumps from v1.0.
- **Memory Portability Spec v2** — multi-implementation interop tests. Reference implementations in two languages besides Rust.
- **Public security audit** — by named third-party firm, full report published. **Specifically tests:** namespace-inheritance enforcement (G1), signature verification (G12), approval timeout sweeper, HMAC coverage on every privileged endpoint.
- **API stability guarantee** — all MCP tools, HTTP endpoints, CLI commands frozen at v1.0 surface.
- **Lock semantics from audit:** `on_conflict` default (`error`); `signature_verified` consumer-guidance; `eviction` telemetry contract.

### 7.7 v1.x and beyond — what continues to be open source

Forever. Including:

- **Hardware attestation hooks** — TPM/HSM/Secure Enclave abstraction. (Certified-managed deployment is the commercial-service tier; the abstraction is OSS.)
- **Cross-modal memory** — image/audio/code-AST embeddings on the same HNSW index, different embedders.
- **Federated learning of recall weights** — agents adapt scoring locally, sync the *weights* across the mesh, not just the memories.
- **Skill marketplace protocol** — registration/discovery/signing/invocation. (Curated marketplace ops = the commercial-service tier; the protocol is OSS.)
- **Custom embedder integrations** — OpenAI, Voyage, Cohere, Ollama, local Sentence Transformers, all behind a trait.

---

## 8. Cumulative remediation effort summary

| Slot | Existing scope | Audit fixes | Recovered commitments | Net add (sessions) |
|---|---|---|---|---|
| **v0.6.3.1** | Cap v2 + Portability + LongMemEval-variant + doc currency | G4–G6, G8, G9, G11, G13 | R1, R7 | +17 |
| **v0.7 Bucket 0** | Hook pipeline | G2, G7-step1, G10 | R3, R5 | +7 |
| **v0.7 Bucket 1** | Ed25519 | G12 (closes column) | — | 0 |
| **v0.7 Bucket 1.7** | Transcripts | (substrate for R5) | — | 0 |
| **v0.7 Bucket 2** | AGE | G14, ANN pre-filter | R2 | +4 |
| **v0.7 Bucket 3** | Permissions+Approval | **G1 (cutline)**, theater fixes | — | +8 |
| **v0.8 Pillar 1** | Task queue | — | — | 0 |
| **v0.8 Pillar 2** | Typed cognition | promote-as-state-machine, tag taxonomy, typed contradictions, taxonomy rename | — | +4 |
| **v0.8 Pillar 2.5** | Compaction | cosine cluster primary, size GC | R4 | +5 |
| **v0.8 Pillar 3** | CRDTs | LWW tiebreak doc | R6 | +3 |
| **v0.9** | Skill + Default rerank | G3, G7-step2, G8 fail-loud | R8 (optional) | +6 |
| **v1.0** | Federation + Stability | G1/G12 audit-locked, on_conflict frozen | — | covered |
| **CUT** | (Plugin SDKs, separate v0.9.5 hub) | — | — | — |
| **WATCH** | — | G15, G16 | — | 0 |

**Total net add: ~54 sessions ≈ 9 weeks of focused human-AI pair work, distributed over 12 months.**

---

## 9. The three highest-leverage moves

1. **`budget_tokens` recall (R1, v0.6.3.1).** Old roadmap's "killer feature, no competitor has this." Letta has it. The new charter set silently dropped it. Recovering it for v0.6.3.1 alongside the LongMemEval reranker-variant disclosure means the published 97.8% R@5 score gets to advertise the killer feature simultaneously. **Compounding leverage.**
2. **Namespace-inheritance enforcement (G1, v0.7 Bucket 3, cutline-protected).** The audit's biggest security-shaped finding. Old roadmap promised "N-level rule inheritance." Code delivers display-only inheritance. This is the gap a procurement team finds and walks away from. **Cutline-protected — ships even if everything else slips.**
3. **Auto-link inference + auto-extraction as `post_store`/`pre_store` hooks (R3+R5, v0.7 Bucket 0).** Old Phase 2 / Phase 4 commitments that vanished. With Bucket 0 as substrate, they cost ~5 sessions combined. Without them, the curator daemon (R4) and consensus memory (R6) have nothing to work on. **They are the missing inputs to the v0.8 vision.**

---

## 10. What gets cut — confirmed final

- **Plugin SDK Python + TypeScript** — MCP is the SDK. One integration surface. Headcount discipline.
- **Backends beyond SQLite + PostgreSQL** — SQLite default; Postgres-with-AGE for team hub. No others.
- **Mobile SDKs** — not until post-GA.
- **Cloud-hosted memory storage** — ai-memory is infrastructure, not SaaS. Self-hosted always.
- **Web UI for memory management** — terminal-first. Visualization = separate project reading the SQLite file.
- **AI agent runtime / orchestration** — ai-memory is a memory layer, not a competitor to Claude Code / Cursor / Letta on agent execution.
- **General-purpose subagent spawning** — bounded compaction subagent (v0.8 Pillar 2.5) is the only LLM autonomy in ai-memory.
- **Separate v0.9.5 "Team Hub" milestone** — collapsed into v0.7 Bucket 2 (AGE).

---

## 11. Quality gates — every release

```bash
cargo fmt --check
cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test
cargo audit
cargo llvm-cov --fail-under-lines 92    # locked at 93.08% baseline
ai-memory bench --baseline performance/baseline.json
```

Plus per-release:

- Ship-gate 4 phases green (functional, federation, migration, chaos).
- A2A-gate cell certification (ironclaw-mtls minimum; full 6-cell matrix for major versions).
- All 5 distribution channels publish smoke-tested (`memory_capabilities` returns valid response).
- Reproducible build verification.
- GPG-signed git tag.
- **NEW v0.6.3.1+:** Public-surface landing pages (ship-gate, A2A-gate) auto-update from latest result JSON. No stale verdict on a public page.

---

## 12. Public-facing artifacts

| Artifact | URL | Currency target |
|---|---|---|
| Source code | github.com/alphaonedev/ai-memory-mcp | always current |
| At-a-glance | alphaonedev.github.io/ai-memory-mcp/at-a-glance.html | per release |
| Test hub | alphaonedev.github.io/ai-memory-test-hub/ | per release |
| Per-release evidence | alphaonedev.github.io/ai-memory-test-hub/releases/<version>/ | per release |
| Ship-gate landing | alphaonedev.github.io/ai-memory-ship-gate/ | **must auto-update — currently stale at v0.6.0.0** |
| A2A-gate landing | alphaonedev.github.io/ai-memory-ai2ai-gate/ | **must auto-update — currently stale at v0.6.2** |
| Performance | alphaonedev.github.io/ai-memory-mcp/performance.html | per release |
| Changelog | github.com/alphaonedev/ai-memory-mcp/blob/main/CHANGELOG.md | per release |
| Roadmap (this doc) | github.com/alphaonedev/ai-memory-mcp/blob/main/ROADMAP2.md | live |
| Memory Portability Spec | memory.dev/spec/v1 (or equivalent) | v0.6.3.1 launch |
| Production Deployment Guide | github.com/alphaonedev/ai-memory-mcp/blob/main/docs/production-deployment.md | v0.7.0 (gap A1, issue #692) |
| Security Policy | github.com/alphaonedev/ai-memory-mcp/blob/main/SECURITY.md | v0.7.0 (gap E2, issue #692) |
| Telemetry & Observability Policy | github.com/alphaonedev/ai-memory-mcp/blob/main/docs/telemetry.md | v0.7.0 (gap E3, issue #692) |
| Adoption Metrics Dashboard | alphaonedev.github.io/ai-memory-mcp/adoption.html | v0.7.0 (gap F2, issue #692; auto-update via `scripts/update-adoption-metrics.sh`) |
| Competitive Benchmarks | github.com/alphaonedev/ai-memory-mcp/tree/main/benchmarks/competitive-benchmarks | v0.7.0 launch (gap F1, issue #692; scaffolding shipped, full run at launch) |

---

## 13. Distribution channels (5 of 5)

- **crates.io** — Rust package registry
- **Homebrew** — `brew install ai-memory`
- **Fedora COPR** — `dnf copr enable alphaonedev/ai-memory && dnf install ai-memory`
- **Docker GHCR** — `docker pull ghcr.io/alphaonedev/ai-memory:latest`
- **APT PPA** — Ubuntu/Debian (Jim Bridger PPA)

Pre-built binaries via `cargo binstall ai-memory` or direct download from GitHub Releases.

---

## 14. Trademark and brand discipline

`ai-memory™` is a USPTO-registered trademark owned by AlphaOne LLC. Brand-specific commercial-service-tier trademarks live outside this document.

Apache 2.0 explicitly does not grant trademark rights. Forks of the codebase cannot use the name `ai-memory`. This is the brand moat that survives even if the code becomes a commodity.

---

## 15. Commitment to OSS permanence

1. **No relicense.** Never to BSL, SSPL, AGPL, Elastic License, or any other non-OSI-approved license.
2. **No paywall on existing features.** No feature that ships in any released version of ai-memory will subsequently be removed and reintroduced as commercial-only.
3. **No commercial-only roadmap items.** This document is the complete roadmap. There is no parallel closed-source roadmap.
4. **No code-locked-behind-services.** Commercial-service-tier offerings do not require running modified ai-memory code. Customers can switch from a managed tier to self-managed at any time without code changes.

If any of these commitments are ever broken, OSS users have the right to fork the last Apache 2.0 release and continue indefinitely. The trademark prevents the fork from using the `ai-memory` name; the code path remains open.

---

## 16. v0.8.0 Policy Engine 100% Audit Trail Closeout

Closes the remaining ~5% gap between v0.7.0 Option B (issues
[#693](https://github.com/alphaonedev/ai-memory-mcp/issues/693) +
[#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691) +
[#694](https://github.com/alphaonedev/ai-memory-mcp/issues/694) +
[#695](https://github.com/alphaonedev/ai-memory-mcp/issues/695) +
[#696](https://github.com/alphaonedev/ai-memory-mcp/issues/696)) and
the full property documented by the operator directive of 2026-05-14:

> "Every tool call passes through a policy engine; the engine logs
> every refusal cryptographically; severity-classified rules can
> escalate to human."

Tracking: [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)
(epic) with 8 sub-tasks (V08-PE-1 through V08-PE-8). Full architectural
detail at [`docs/policy-engine.md`](docs/policy-engine.md) and audit
coverage matrix at
[`docs/security/audit-trail-coverage.md`](docs/security/audit-trail-coverage.md).

### Sub-task summary

- **V08-PE-1: Mandatory-hook profile** — `--enforce` for
  procurement-tier deployments. The daemon refuses to serve when the
  Claude Code PreToolUse hook is not installed. Closes the
  out-of-band-actions gap by raising the cost of "I forgot to install
  the hook" from silent permissiveness to refuse-to-start.
- **V08-PE-2: Read-action gating** — `AgentAction::Read` variant +
  wire-point coverage across recall / search / list / get /
  session_boot. Today the K9 `Permissions::evaluate` pipeline gates
  the substrate-INTERNAL read path (memory-scoped); V08-PE-2 adds the
  top-level engine surface so reads land in `signed_events` alongside
  writes.
- **V08-PE-3: Subprocess-chain visibility** — eBPF on Linux, dtrace
  on macOS. Surfaces the fork+exec chain underneath a permitted Bash
  invocation so a `bash -c "evil_thing"` whose child then spawns an
  unrelated process is visible to the engine and chain-logged.
- **V08-PE-4: Persistent audit queue** — durable across daemon
  restart. Closes the hard-crash gap in PE-3's process-local
  deferred queue ([#696](https://github.com/alphaonedev/ai-memory-mcp/issues/696)).
  Design candidate: on-disk WAL-style queue with periodic
  fsync + drain-on-recovery at boot.
- **V08-PE-5: Severity-based human escalation** — adds
  `Decision::Escalate { rule_id, prompt }`. Pairs with the L1-8
  Approval-API surface (already shipped): an Escalate verdict emits
  a `pending_action`, the operator dashboard surfaces it, the
  operator's allow/deny decision joins the audit chain. Closes the
  "rules can escalate to human" half of the operator directive.
- **V08-PE-6: TPM-bound binary integrity** — daemon attests the
  shipping binary against a signed manifest at boot. Closes the
  last partial mitigation for out-of-band actions: a forked binary
  that no-ops the hook fails attestation and the operator's TPM
  refuses to release the rule-signing key.
- **V08-PE-7: Refuse-by-default profile** — procurement-tier rule
  set that ships `enabled = 1, attest_level = operator_signed` out
  of the box for a vendored operator key (with an opt-out path for
  fresh self-hosted operators who want the default-permissive
  cold-start contract).
- **V08-PE-8: Audit-trail completeness verifier** — `ai-memory
  verify-audit-trail`. Walks the `signed_events` chain end to end:
  monotonic sequence check + Ed25519 signature check per row +
  cross-reference against the expected event surface (memories,
  links, approvals, migrations). Closes the verification loop the
  v0.7.0 ship cannot mechanically perform today.

### Effort

22-28 sessions · 3-4 weeks wall-clock · MEDIUM-HIGH risk. At the v0.7.0
cadence (4 production releases in 14 days through v0.6.4 + v0.7.0),
22-28 sessions sit inside the Q4 2026 v0.8.0 window without
displacing any prior commitment from §7.4 (Distributed Coordination
Substrate). The audit-trail closeout is **additive** to the v0.8.0
scope — it does not replace Pillar 1 / Pillar 2 / Pillar 2.5 / Pillar 3
or the strategic adjacencies (§7.4.A-F).

### Cutline discipline if slipping

- **Keep (cutline-protected):** V08-PE-1 mandatory-hook profile,
  V08-PE-5 severity-based escalation, V08-PE-8 completeness verifier
  — these are the three sub-tasks that close the operator's stated
  property literally.
- **Defer to v0.8.1 if substrate slips:** V08-PE-3 subprocess-chain
  visibility (eBPF / dtrace work has platform-specific risk).
- **Defer to v0.9 if slippage severe:** V08-PE-6 TPM-bound integrity
  (depends on TPM toolchain maturity in the deployment fleet);
  V08-PE-7 refuse-by-default profile (operator-side rollout
  exercise).

The three cutline-protected sub-tasks together close ~90% of the
remaining 5% gap. V08-PE-2 read-action gating folds in if cycle
budget allows — it widens visibility but does not close a
distinct property the operator directive named.

---

## 17. Net

ai-memory v0.6.3 shipped clean: 1,809 tests, 93.08% coverage, ship-gate 4/4, A2A 48/48 mTLS, 5/5 channels, LongMemEval R@5 97.8% / R@10 99.0% / R@20 99.8%, 43 MCP tools, schema v15. v0.6.3.1 then landed (2026-04-30) with the never-lose-context release: 1,886 lib tests (+281), 93.84% line coverage, schema v19 (ladder v15→v17→v18→v19), 7 new CLI surfaces (boot/install/wrap/logs/audit/doctor/bench), and 17 documented integrations across 10 platforms. v0.7.0 grand-slam ship state (as of `feat/v0.7.0-layer-1` HEAD, L1-7 / L1-5 landed): schema **v30** (sqlite ladder per §4.1), **52 MCP tools** total (including 5 Agent Skills tools from L1-5, not 6 — the 6th `_promote_from_reflection` is L2-6 / v0.8.0), and **25 hook lifecycle events** (see §4.7 grand-slam block for the ladder).

The audit found 22 distinct gaps. None block the published v0.6.3 claims. One (G1 — namespace-inheritance enforcement) is a security-shaped bug that gets a cutline-protected slot in v0.7 Bucket 3. Eight are capabilities-JSON theater that v0.6.3.1 Capabilities v2 makes honest. The remaining thirteen distribute cleanly across v0.6.3.1 / v0.7 / v0.8 / v0.9 / v1.0.

Eight commitments dropped in the prior rewrite (`budget_tokens`, `memory_find_paths`, auto-link inference, auto-extraction, consensus memory, `ai-memory doctor`, curator-as-daemon, TOON v2) are recovered into existing buckets — none requires a new milestone.

Two public landing pages (ship-gate, A2A-gate) lag the actual ship and must auto-update from result JSON going forward.

This is the public-facing OSS roadmap. v0.6.3.1 (Q2 2026, ~4 weeks). v0.7 (Q2 2026, June). v0.8 (Q4 2026). v0.9 (Q1 2027). v1.0 (Q2 2027). Apache 2.0. Forever.

---

*Cleared hot. Stack is laid. Ship the OSS. Forever.*

*Document classification: Public-facing. Eligible for posting at github.com/alphaonedev/ai-memory-mcp/blob/main/ROADMAP2.md.*
