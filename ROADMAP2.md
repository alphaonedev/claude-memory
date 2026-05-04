# ai-memory — Roadmap v2 (Consolidated, Audit-Reconciled, Evidence-Backed)

> **Document classification:** Public-facing strategic roadmap.
> **Date:** 2026-04-29
> **Supersedes:** the prior `ROADMAP.md` (Phase 0–6, drafted at v0.5.4.4) and the 2026-04-29 charter-set roadmap. Where they conflict, this document wins.
> **Trademark:** ai-memory™ — USPTO Serial No. 99761257
> **License:** Apache 2.0 — permanent, non-revocable, non-relicenseable.
> **Production version at write time:** v0.6.3.1 (shipped 2026-04-30; this audit's text dates 2026-04-29 use "v0.6.3" inline because Patch 1 had not yet shipped at the time of writing — the contract has since landed and §7.2 is now SHIPPED).

---

## 0. Executive position in one paragraph

Everything that compiles into the `ai-memory` binary is Apache 2.0, forever. There is no closed-source roadmap. There is no commercial-only feature. There is no "open-core" gotcha where the substrate is free but the useful parts cost money. The four-charter set and the prior phased roadmap are reconciled here: every engineering deliverable in either is OSS, every gap surfaced in the v0.6.3 source-code audit has a slot, every commitment that vanished in the prior rewrite is recovered or formally cut. AgenticMem (separate document) consumes this substrate but paywalls none of it.

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

### 5.6 Behavioral findings — agent-side evidence (added 2026-05-04)

The substrate audit in §5.1–§5.4 covers what the code does. **Behavioral evidence covers what working agents on top of the substrate do** — different evidence stream, complementary findings, agent-experience focus.

The first systematic instrument in this category is the [v0.6.3.1 OpenClaw behavioral assessment](https://alphaonedev.github.io/ai-memory-a2a-v0.6.3.1/nhi/openclaw-behavioral-v0.6.3.1/): 52 probes across 8 phases (qualitative awareness / recall@k / cross-session ablation / Byzantine peer trust calibration / tool surface discovery / RoadMap recommendations / soft-restart / hard-restart context recovery), three independent OpenClaw 2026.5.x agents on `xai/grok-4.3`, MCP-mounted ai-memory v0.6.3.1 on a 4-node local-docker mesh.

**Headline measurements (perfect across three measures):** recall@1 = 1.000 over 18 trials. Cross-session durability = 1.000 over 3 trials (token-keyed write-α / read-β-fresh). Byzantine peer trust calibration = 1.000 over 3 trials.

**Headline finding (the gap):** Phase 9 organic-no-cue recovery = 0/1. Without an explicit cue ("memory_recall on namespace=...") agents do **not** reach for `memory_recall` to recover prior context after a session reset. They confabulate from the agent runtime's bootstrap activity. Cued recovery succeeds 100%. **Cue language gates the agent's decision to invoke memory tools, not data availability.**

**Three-agent unanimous RoadMap signals (Phase 7).** Three agents independently named the same top-3 capability gaps after running concrete tasks on top of ai-memory:

| Issue | Gap | Disposition |
|---|---|---|
| [#517](https://github.com/alphaonedev/ai-memory-mcp/issues/517) | Auto-suggest `memory_link` during/after `memory_store` (manual linking is the biggest workflow friction in KG reasoning + multi-agent collab) | v0.6.4 Track G-AX (lightweight, response-field), v0.7 Bucket 0 R3 (full daemon-mode hook) |
| [#518](https://github.com/alphaonedev/ai-memory-mcp/issues/518) | Session-aware `memory_recall` defaults + auto-cue on session start (closes the Phase 9 organic-no-cue failure case) | v0.6.4 Track G-AX |
| [#519](https://github.com/alphaonedev/ai-memory-mcp/issues/519) | Proactive conflict detection inside `memory_store` with merge suggestions (post-hoc `memory_detect_contradiction` is too slow per all three agents) | v0.6.4 Track G-AX |

All three issues are filed against milestone v0.6.4 (#7), labelled `v0.6.4-candidate`. Each carries an honest demote-to-v0.6.4.1 path so the sprint manager can rebalance Wed standup if Tracks A–F consume the budget.

The behavioral assessment is now a recurring instrument — future releases will run the same 8-phase suite (or its v0.7 successor with attested identity probes added) to track delta over time.

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

### 7.2.5 v0.6.4 — Cross-harness token economics + NHI guardrails phase 1 — Mon 2026-05-04 → Fri 2026-05-08 (5 dev days)

**Code-name:** `quiet-tools`. **Sprint authorized 2026-05-02; ships Fri 2026-05-08; public announce Mon 2026-05-11.**

The release where ai-memory stops being a token hog. Default tool surface collapses 42 → 5; expansion is opt-in via discovery; observability surfaces the cost; NHI guardrails phase 1 (allowlist + audit log) gates the expansion path.

**Why this release exists.** Boris Cherny's published 90-day instrumentation (May 2026) quantified that 73% of Claude Code tokens go to nine waste patterns. ai-memory is the **#1 contributor to Pattern 6** (just-in-case tool defs) on every coding-agent harness except Claude Code's deferred-tool path: ~25,200 input tokens per request just for tool schemas, ~$570/year per heavy user, ~$0.076 per request. On Codex / Grok CLI / Gemini CLI / stock Claude Desktop, ai-memory is dominating the input prefix. Without v0.6.4, ai-memory inherits the "AI token hog" reputation just as those harnesses adopt at scale.

**Scope (16 issues + 3 stretch).** Tracks A–G + Track G-AX:

- **A (Mon) Mechanism** — `--profile` CLI flag + `AI_MEMORY_PROFILE` env + `[mcp].profile` config, family-scoped tool registration filter, `core` (5 tools) as new default with `--profile full` opt-out.
- **B (Mon) Observability** — `ai-memory doctor --tokens` + static schema-size table (build-time) for cost queryable without running.
- **C (Tue) Discovery** — `memory_capabilities` extension (family enumeration + `--include-schema`), TS + Py SDK `requireProfile()` with `ProfileNotLoaded`.
- **D (Wed) NHI guardrails phase 1** — per-agent allowlist (config-driven, `agent_id`-keyed, default = `core`), capability-expansion audit log.
- **E (Wed) Cross-harness install** — install profiles for claude-code, claude-desktop, codex, grok-cli, gemini-cli (all default `core`).
- **F (Thu) Cert + benchmarks** — A2A scenarios S25–S32 added to v0.6.4 cert cell, cross-harness token-cost benchmark (static + 1 live spot-check per harness), backward-compat verification (`--profile full` matches v0.6.3 baseline 1:1).
- **G (Fri) Docs + release** — README, ADMIN_GUIDE, migration guide, release notes, CHANGELOG, tag, brew tap.
- **G-AX (Wed–Thu, stretch — demote to v0.6.4.1 if scope hot)** — three agent-experience refinements landed from the [v0.6.3.1 OpenClaw behavioral assessment](https://alphaonedev.github.io/ai-memory-a2a-v0.6.3.1/nhi/openclaw-behavioral-v0.6.3.1/) (52 probes, 3-agent unanimous P7 RoadMap signals):
  - **G1 #517** — auto-suggest `memory_link` during/after `memory_store` (`linked_candidates[]` response field; HNSW similarity at write time).
  - **G2 #518** — session-aware `memory_recall` defaults via `agents.defaults.recall_scope` + session-start auto-cue (closes the "organic-no-cue" recovery failure observed in Phase 9 of the assessment).
  - **G3 #519** — proactive conflict detection inside `memory_store` with `merge_strategy` suggestions (replace / link.supersedes / link.contradicts / consolidate).

**Success metrics (measured Mon 2026-05-11).** Token-def cost on Codex/Grok/Gemini drops ≥85% (target 87%). `core` profile covers ≥95% of agent traffic without escalation. `memory_capabilities --include-schema` becomes the canonical NHI expansion path. Zero regressions in v0.6.3 cert matrix when `--profile full` set.

**Source charter:** [`docs/v0.6.4/v0.6.4-roadmap.md`](docs/v0.6.4/v0.6.4-roadmap.md) (16-issue list, daily schedule, risk register). NHI execution prompts: [`docs/v0.6.4/v0.6.4-nhi-prompts.md`](docs/v0.6.4/v0.6.4-nhi-prompts.md). Design: [`docs/v0.6.4/rfc-default-tool-surface-collapse.md`](docs/v0.6.4/rfc-default-tool-surface-collapse.md).

**Relationship to other releases:**
- Independent of v0.6.3.1 (Honesty Patch). Both can ship in parallel.
- Track G-AX overlaps with **v0.7 Bucket 0 R3** (auto-link inference as `post_store` daemon-mode hook) — v0.6.4-G1 is the lightweight first-mover (additive response field, ~3 days); v0.7 R3 is the full daemon-mode integration (~3 sessions). Backward-compatible upgrade path; v0.7 R3 obsoletes the v0.6.4 scaffold but doesn't remove it.
- **NHI guardrails phase 2** (rate limits, attestation-tier gating) lands in v0.7 Bucket 3, builds on the audit-log substrate from v0.6.4-009.

### 7.3 v0.7 — Trust + A2A Maturity — Q2 2026 (June target)

#### Bucket 0 — Hook Pipeline

Programmable lifecycle events at every memory operation point. Subprocess JSON-over-stdio with daemon-mode IPC for hot paths.

- 20 lifecycle events (16 base + 2 compaction + 2 transcripts).
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

**Out of OSS scope:** Hardware-backed key storage (TPM/HSM/Secure Enclave) deployment. The OSS provides the *abstraction*; the certified-managed *deployment* is AgenticMem's commercial layer.

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

### 7.4 v0.8 — Coordination Primitives — Q4 2026

#### Pillar 1 — Distributed Task Queue

- `task` typed memory with state machine: `pending → claimed → in_progress → done/failed/abandoned`.
- `memory_task_enqueue`, `memory_task_claim`, `memory_task_complete`, `memory_task_abandon` MCP tools.
- Dependency-DAG enforcement.
- Lease + heartbeat for resilience.
- Federation-aware (W-of-N quorum on shared namespaces).

#### Pillar 2 — Typed Cognition

- Typed memory enums: `Goal`, `Plan`, `Step`, `Observation`, `Decision`.
- Relation taxonomy: `step.advances → plan`, `plan.serves → goal`, etc.
- `memory_cognition_register`, `memory_cognition_query`, `memory_cognition_supersede`.
- Strict typing validation: Plan must point at Goal; Step at Plan; etc.

**Audit absorbs:**
- Promote becomes a typed state machine, not a column flip (closes the §5.2 narrowness).
- Tag taxonomy as constrained overlay (closes the auto_tag uncurated-free-text issue).
- Typed contradiction detection: `Decision A` vs `Decision B` on same `Goal` as candidate set. Replaces FTS-title-match heuristic with semantic-typed candidate set.

**Naming hygiene:** rename existing `memory_get_taxonomy` → `memory_namespace_taxonomy` (it returns namespace folder counts, not tags). New `memory_cognition_taxonomy` returns typed-memory distribution.

#### Pillar 2.5 — Compaction Pipeline

Six-stage with verify+rollback. Maps to typed-cognition supersession.

- Pipeline: dedupe → cluster → eligibility → summarize → persist → verify.
- Stage 6 rollback when verify fails.
- Pressure triggers calibrated against PERFORMANCE.md p95 budgets.
- Bounded compaction subagent: single LLM call, no tools, no loops, structured JSON output.
- New hook events `pre_compaction` and `on_compaction_rollback`.
- Default `enabled = false` (Ollama dependency means silent fail otherwise; operator opts in).
- `prune_after_days = 0` (never) for archive default.

**Audit absorbs:**
- Cosine clustering as primary path; Jaccard becomes the cheap pre-filter (upgrades the lexical-Jaccard-only path of v0.6.3 auto-consolidation).
- Size-pressure GC triggers (closes "GC is TTL-only").

**Recoveries absorb:**
- **R4 — `ai-memory curator` standalone daemon CLI** wraps Pillar 2.5's compaction + Bucket 0's auto-link-inference + auto-extraction into one operator-visible daemon. ~2 sessions.

#### Pillar 3 — CRDTs

- Core CRDT type set: G-Counter (access_count), PN-Counter (general counters), LWW-Register with attested-identity tiebreak, OR-Set (tags).
- Per-memory vector clock (agent_id → Lamport tick).
- Federation push/pull merges via CRDT semantics (replaces last-writer-wins on `updated_at`).
- Conflict-aware curator: distinguishes mergeable conflicts from human-resolution-required.

**Audit absorbs:**
- `access_count` cap (currently 1M global) becomes per-replica when promoted to G-Counter; document merge.
- `memory_links` directionality vs `get_links`-undirected-on-read: pin down the OR-Set semantic now, not at merge time.
- LWW-Register tiebreak: ship as `(attestation_level, agent_id, monotonic_local_clock)` with documented consequences. **Do not ship "CRDTs" as a vague banner. Ship the four typed primitives with documented merge semantics.**

**Recoveries absorb:**
- **R6 — Consensus-based truth determination.** When N agents store conflicting facts, confidence becomes function of agent count (4-of-5 agree → 0.95). Pairs with LWW-Register tiebreak. ~3 sessions.

#### v0.8 cutline if slipping

Keep: Pillar 1, Pillar 3 (CRDT four-primitive set with documented merge), G1 if it slipped from v0.7.
Defer Pillar 2 typed cognition to v0.8.1 if substrate ships clean.

### 7.5 v0.9 — Skill Memories + Function Calling + Default-On Reranker — Q1 2027

- **Skill memories** — `tier=long, namespace=_skills/<id>` formalized as a first-class type with `parameters_schema`, `invocation_record`, `version`. `memory_skill_register`, `memory_skill_invoke`, `memory_skill_list` MCP tools.
- **Function calling in `llm.rs`** — wire local Gemma 4 LLM to a tool-calling protocol so curator passes can use targeted operations rather than blind text generation.
- **Cross-encoder reranker default-on** — closes the published reranker-on quality range. HF-Hub model auto-fetch on first use; **fail loud (`mode: "degraded"`)** when model not available, no silent lexical fallback.
- **Streaming tool responses** — for long-running MCP tools (recall over very large stores, federation broadcasts).

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

- **Hardware attestation hooks** — TPM/HSM/Secure Enclave abstraction. (Certified-managed deployment is AgenticMem's commercial layer; the abstraction is OSS.)
- **Cross-modal memory** — image/audio/code-AST embeddings on the same HNSW index, different embedders.
- **Federated learning of recall weights** — agents adapt scoring locally, sync the *weights* across the mesh, not just the memories.
- **Skill marketplace protocol** — registration/discovery/signing/invocation. (Curated marketplace ops = AgenticMem; the protocol is OSS.)
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

`ai-memory™` and `AgenticMem™` are USPTO-registered trademarks owned by AlphaOne LLC.

Apache 2.0 explicitly does not grant trademark rights. Forks of the codebase cannot use the names `ai-memory` or `AgenticMem`. This is the brand moat that survives even if the code becomes a commodity.

---

## 15. Commitment to OSS permanence

1. **No relicense.** Never to BSL, SSPL, AGPL, Elastic License, or any other non-OSI-approved license.
2. **No paywall on existing features.** No feature that ships in any released version of ai-memory will subsequently be removed and reintroduced as commercial-only.
3. **No commercial-only roadmap items.** This document is the complete roadmap. There is no parallel closed-source roadmap.
4. **No code-locked-behind-services.** AgenticMem services do not require running modified ai-memory code. Customers can switch from AgenticMem to self-managed at any time without code changes.

If any of these commitments are ever broken, OSS users have the right to fork the last Apache 2.0 release and continue indefinitely. The trademark prevents the fork from using the `ai-memory` name; the code path remains open.

---

## 16. Net

ai-memory v0.6.3 shipped clean: 1,809 tests, 93.08% coverage, ship-gate 4/4, A2A 48/48 mTLS, 5/5 channels, LongMemEval R@5 97.8% / R@10 99.0% / R@20 99.8%, 43 MCP tools, schema v15. v0.6.3.1 then landed (2026-04-30) with the never-lose-context release: 1,886 lib tests (+281), 93.84% line coverage, schema v19 (ladder v15→v17→v18→v19), 7 new CLI surfaces (boot/install/wrap/logs/audit/doctor/bench), and 17 documented integrations across 10 platforms.

The audit found 22 distinct gaps. None block the published v0.6.3 claims. One (G1 — namespace-inheritance enforcement) is a security-shaped bug that gets a cutline-protected slot in v0.7 Bucket 3. Eight are capabilities-JSON theater that v0.6.3.1 Capabilities v2 makes honest. The remaining thirteen distribute cleanly across v0.6.3.1 / v0.7 / v0.8 / v0.9 / v1.0.

Eight commitments dropped in the prior rewrite (`budget_tokens`, `memory_find_paths`, auto-link inference, auto-extraction, consensus memory, `ai-memory doctor`, curator-as-daemon, TOON v2) are recovered into existing buckets — none requires a new milestone.

Two public landing pages (ship-gate, A2A-gate) lag the actual ship and must auto-update from result JSON going forward.

This is the public-facing OSS roadmap. v0.6.3.1 (Q2 2026, ~4 weeks). v0.7 (Q2 2026, June). v0.8 (Q4 2026). v0.9 (Q1 2027). v1.0 (Q2 2027). Apache 2.0. Forever.

---

*Cleared hot. Stack is laid. Ship the OSS. Forever.*

*Document classification: Public-facing. Eligible for posting at github.com/alphaonedev/ai-memory-mcp/blob/main/ROADMAP2.md.*
