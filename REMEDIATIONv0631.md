# REMEDIATION v0.6.3 — Full-Spectrum Closure Plan

> **Scope:** Everything required to fully remediate v0.6.3 (shipped 2026-04-27) so the published capabilities, architectures, and tier claims are honest, complete, and load-bearing across T1–T5.
> **Vehicle:** v0.6.3.1 patch release (Q2 2026, ~4 weeks).
> **Companion docs:** `ROADMAP2.md` (forward plan v0.6.3.1 → v1.0), `audits/v063-source-code-audit.md` (this audit), `architectures.html` + T1–T5 pages.
> **Date:** 2026-04-29.
> **Authoring posture:** Brass tacks. No fluff. Every fix names files, lines, success criteria, and a copy-paste AI NHI prompt starter.

---

## 0. Architectural mapping — v0.6.3 audit findings vs T1–T5

The architectures page declares: *"Each tier inherits everything below it."* That makes every audit finding T1 in scope and every higher tier downstream. The matrix below shows where each finding **first becomes load-bearing**.

| Audit finding | T1 (1 node, 1 agent) | T2 (1 node, ≥1 agents) | T3 (4-node cluster) | T4 (rack-scale) | T5 (multi-region) |
|---|---|---|---|---|---|
| **G1** Namespace inheritance not enforced (gate uses leaf only) | minor | **CRITICAL** (governance is the T2 differentiator) | **CRITICAL** | **CRITICAL** | **CRITICAL** |
| **G2** HNSW silent oldest-eviction at 100k | bites near ceiling | bites near ceiling | bites per-node | bites per-node | bites per-node |
| **G3** HNSW in-memory only (rebuilt cold) | restart cost | restart cost | restart cost ×N | restart cost ×N | restart cost ×N |
| **G4** Mixed embedding dims silently tolerated | data integrity | data integrity | **fleet drift** | **fleet drift** | **regional drift** |
| **G5** Archive drops embedding column | recall lossy on restore | recall lossy on restore | per-node drift | per-rack drift | per-region drift |
| **G6** UNIQUE(title,namespace) silent merge on conflict | latent bug | latent bug | **partition risk** (re-merged rows on rejoin) | **partition risk** | **partition risk** |
| **G7** Reranker `Mutex<BertModel>` serialization | single-thread fine | **contention** (10–20 agents) | **contention** | **contention** | **contention** |
| **G8** Reranker silent fallback to lexical | quality degraded silently | quality degraded silently | fleet drift | fleet drift | regional drift |
| **G9** Webhooks fire on `store` only | event coverage gap | event coverage gap | **federation gap** (promote/delete/link/consolidate don't propagate via subs) | **federation gap** | **federation gap** |
| **G10** `memory_expand_query` not auto-invoked from recall | feature is dead weight | feature is dead weight | feature is dead weight | feature is dead weight | feature is dead weight |
| **G11** Embedder silent degrade to keyword-only | quality degraded silently | quality degraded silently | fleet drift | fleet drift | regional drift |
| **G12** `memory_links.signature` column dead | n/a | n/a | trust gap | **trust gap** | **trust gap** |
| **G13** f32 endianness no magic header | n/a (single arch) | n/a | **cross-arch corruption** | **cross-arch corruption** | **cross-region corruption** |
| **G14** `kg_invalidate` no audit column | minor | minor | audit gap | audit gap | audit gap |
| **G15** Stats live-counted, no cache | fine | fine | fine | profile at scale | profile at scale |
| **G16** v16 schema migration no-op | doc | doc | doc | doc | doc |
| **R1** `budget_tokens` recall (recovered) | killer feature | killer feature | killer feature | killer feature | killer feature |
| **R7** `ai-memory doctor` CLI (recovered) | operator visibility | operator visibility | **fleet visibility** | **fleet visibility** | **regional visibility** |
| **§5.5** Public-surface lag on ship-gate / A2A-gate landing pages | trust signal | trust signal | trust signal | **procurement-blocking** | **procurement-blocking** |

**Reading the matrix:**
- T1 deployments today are **mostly fine** with v0.6.3. The data-integrity items (G4, G5, G6, G13) become latent bugs that compound at higher tiers.
- T2 is where **G1** transforms from cosmetic to security-shaped. Governance is the T2 differentiator and v0.6.3's gate enforces only at the leaf. **This must ship in v0.6.3.1.**
- T3 is where the silent-degradation items (G8, G11, G2 evictions, G6 partition merges) cause **fleet drift** — every node thinks it's healthy while quality varies invisibly.
- T4/T5 inherit everything plus add the cross-arch and trust-boundary concerns (G13, G12).

Three findings — **G1, G6, G13** — get worse with deployment scale and must be fixed before they ossify into compatibility constraints. They are the "fix now or pay forever" items.

---

## 1. v0.6.3 ↔ tier capability honesty check

What each tier page advertises as "ships today" vs what the audit found:

### T1 — claims vs reality

| T1 claim | v0.6.3 reality | Action |
|---|---|---|
| "Sub-millisecond vector recall at 10⁵ rows" | True (instant-distance HNSW) | none |
| "~10⁶ memories before RAM constraints" | **HNSW caps at 100,000 with silent oldest-eviction** | **G2 telemetry; cap doc** |
| "Auto-promotion to higher tiers after 5th access" | True (`PROMOTION_THRESHOLD = 5`) | none |
| "Capabilities introspection v2" | Reports flags; many are theatre | **Capabilities v2 honesty fixes** |
| "Auto-tagging and contradiction detection" | Hard-errors without Ollama (smart+ tier); single canned LLM prompts | document candidly |

### T2 — claims vs reality

| T2 claim | v0.6.3 reality | Action |
|---|---|---|
| "Per-namespace governance policies" | True for leaf namespace only | **G1: walk inheritance chain** |
| "PendingAction queue for governance workflows" | Approval gate wired end-to-end on store/delete/promote | none |
| "Hierarchical policy inheritance (default at `org/`, overridable at `org/team/`)" | **Policy display walks chain; gate uses leaf only** | **G1: must fix in v0.6.3.1** |
| "Capabilities introspection (v0.6.3): hybrid recall, auto-tagging, contradiction analysis, approval workflows" | Some flags are constants, not live state | **Capabilities v2 honesty** |
| "memory_links.signature claims identity (v0.6.3)" | Column exists, **never written nor verified** | document as v0.7 deliverable |

### T3 — claims vs reality

| T3 claim | v0.6.3 reality | Action |
|---|---|---|
| "10 broadcast functions wired into write path" | True (`replication.rs` 422 lines) | none |
| "Quorum-write contract returns 503 quorum_not_met" | True | none |
| "sync_push fanout for store/delete/link/consolidate" | True | none |
| "Subscription webhooks" | **Fire on `memory_store` only — not promote/delete/link/consolidate** | **G9: full event coverage** |
| "mTLS peer mesh with SHA-256 fingerprint allowlist" | True | none |
| "Cross-node consistency: eventual" | True via `sync_state` vector clocks | none |

### T4 — claims vs reality

| T4 claim | v0.6.3 reality | Action |
|---|---|---|
| "Postgres + pgvector behind `sal-postgres` Cargo feature; correctness fixed in v0.6.x" | True | none |
| "v0.7 GA targets: shared distributed store, Postgres replicas, shared vector index" | Future commitment | tracked in ROADMAP2 |
| "Cryptographic agent attestation (`signature` field reserved T3; v0.7 T4)" | Column reserved, dead | tracked in ROADMAP2 v0.7 Bucket 1 |

### T5 — claims vs reality

| T5 claim | v0.6.3 reality | Action |
|---|---|---|
| "Vision v1.0+" | True; nothing in v0.6.3 ships at T5 scale | none |
| Hardware-backed key custody (TPM/SE/KMS) | OSS abstraction in v0.7+; managed in AgenticMem | tracked |
| Distributed consensus (Raft/Paxos) | v1.0+ design | tracked |

**Net architectural posture:** T1 is honest with minor tweaks. **T2 has one critical bug (G1)** that contradicts the architecture page's promise of hierarchical policy inheritance. T3 has the webhook-event-coverage gap (G9). T4/T5 are appropriately marked as roadmap.

---

## 2. Remediation phases (chronological dependency-aware)

Eight phases organized so dependencies flow downhill. P0 is ops-only (no code). P1–P3 land code in this order. P4–P5 are independent of P1–P3 and can run in parallel. P6–P7 are validation/disclosure phases. P8 is documentation.

| Phase | Title | Touches | Depends on | Effort (sessions) | Tier impact |
|---|---|---|---|---|---|
| **P0** | Public-surface currency | docs/sites only | none | 1–2 | trust signal — all tiers |
| **P1** | Capabilities v2 honesty | `config.rs`, `mcp.rs` | none | 2–3 | T1+ |
| **P2** | Data-integrity hardening (G4, G5, G6, G13) | `db.rs`, `hnsw.rs`, schema migration | P1 (Cap v2 surfaces results) | 4–5 | T1+, gets worse with tier |
| **P3** | Recall observability (G2, G8, G11) | `hnsw.rs`, `reranker.rs`, `mcp.rs` recall path | P1 | 2–3 | T1+ |
| **P4** | **Governance inheritance enforcement (G1)** — **cutline-protected** | `db.rs::resolve_governance_policy`, ship-gate test | none | 3–4 | T2+ |
| **P5** | Webhook event coverage (G9) | `mcp.rs` for promote/delete/link/consolidate | none | 1–2 | T3+ |
| **P6** | `budget_tokens` recall (R1) | `mcp.rs::handle_recall`, scoring | P3 | 2–3 | T1+ |
| **P7** | `ai-memory doctor` CLI (R7) | new CLI subcommand reading P1–P3 surfaces | P1, P2, P3 | 2 | T1+; **fleet doctor** in T3+ |
| **P8** | LongMemEval reranker-variant disclosure + Memory Portability Spec v1 | `benchmarks/`, `docs/spec/v1.md` | P1 | 2–3 | trust signal — all tiers |

**Total effort:** 19–25 sessions. Realistic timeline: 4 weeks with concurrency on P4/P5 against P1–P3.

**Cutline if v0.6.3.1 must ship in 2 weeks:** keep **P0, P1, P2 (just G4), P4 (G1)**. Defer P3, P5, P6, P7, P8 to v0.6.3.2.

---

## 3. Phase-by-phase remediation specs

Each phase below is a self-contained execution unit. Format: **Goal → Files → Steps → Tests → Success criteria → AI NHI prompt starter**.

---

### Phase P0 — Public-surface currency

**Goal:** Make the public ship-gate and A2A-gate landing pages reflect v0.6.3 reality. Currently lagging at v0.6.0.0 and v0.6.2 respectively.

**Files / surfaces:**
- `https://alphaonedev.github.io/ai-memory-ship-gate/` (lag: latest documented = v0.6.0.0; v0.6.3 evidence exists per release-evidence page but not surfaced on landing)
- `https://alphaonedev.github.io/ai-memory-ai2ai-gate/` (lag: latest cert = v0.6.2 / 2026-04-24; v3r23 still cites unresolved S18/S39 which v0.6.3 closed)
- Each gate's CI pipeline (must auto-update landing JSON)

**Steps:**
1. Add v0.6.3 result block to ship-gate landing: 4/4 phases green, 14m wall, breakdown by phase.
2. Add v0.6.3 result block to A2A-gate landing: 48/48 ironclaw-mtls green, S18 + S39 closed, 28m wall.
3. Convert both landing pages to read from `releases/<version>/summary.json` instead of inline-edited HTML. Latest published version becomes "current."
4. Add to release pipeline: every ship-gate / A2A-gate run posts a `summary.json` artifact; landing page CI republishes when artifact lands.

**Tests:** Manual verification that both landing pages now display v0.6.3 as latest. Automated: a release-blocking job that fails the version bump if the landing JSON for the new version doesn't exist within 1h of release.

**Success criteria:**
- [ ] Ship-gate landing top-of-page shows "v0.6.3 — 4/4 phases green — 2026-04-27"
- [ ] A2A-gate landing top-of-page shows "v0.6.3 — 48/48 ironclaw-mtls green — 2026-04-27"
- [ ] S18 and S39 marked "resolved in v0.6.3"
- [ ] Both landing pages read from `summary.json`; PR bumping version cannot merge without the JSON

#### AI NHI prompt starter — P0

```
Role: Site reliability + docs engineer.
Repos: alphaonedev/ai-memory-ship-gate (Pages), alphaonedev/ai-memory-ai2ai-gate (Pages).

Context: ai-memory v0.6.3 shipped 2026-04-27. Per
https://alphaonedev.github.io/ai-memory-test-hub/releases/v0.6.3/, the ship-gate
ran 4/4 green in 14m wall and the A2A-gate ran 48/48 green at ironclaw-mtls in
28m wall, closing scenarios S18 (semantic expansion) and S39 (SSH STOP/CONT
reliability) that were open at v3r23. The two landing pages currently lag at
v0.6.0.0 and v0.6.2 respectively, which makes a procurement reader think the
project is dormant.

Task:
1. Update each landing page to surface v0.6.3 as the latest cert with full
   per-phase / per-cell breakdown.
2. Convert each landing page from inline-edited HTML to a template that reads
   from `releases/<version>/summary.json`. The "latest" pointer is the highest
   semver in releases/. No more manual edits to the landing page on release.
3. Add a release-blocking GitHub Actions job in each repo: on tag push, the job
   verifies a `releases/<tag>/summary.json` exists with the schema {version,
   campaign_run_id, phases[]|cells[], pass_count, fail_count, wall_seconds,
   verdict}. Fail the release if missing.

Constraints:
- Don't break existing per-release evidence pages.
- Preserve existing campaign artifact links.
- Keep the page static-renderable (GitHub Pages).

Acceptance:
- Visiting either landing page shows v0.6.3 as the headline result.
- Pushing a `v0.6.4-rc1` tag without summary.json fails CI.
- Pushing with a valid summary.json updates the landing page within one
  workflow run.

Open the existing landing pages first, infer the current schema, then propose
your migration plan as a checklist before editing. Stop when the checklist is
ready and wait for approval.
```

---

### Phase P1 — Capabilities v2 honesty

**Goal:** Stop the capabilities JSON from advertising features that don't exist or settings that aren't read. Schema v2 with `schema_version="2"` discriminator preserves v1 client compatibility.

**Files (citations from audit):**
- `src/config.rs:199-236` — `CapabilityFeatures` struct, hard-coded defaults
- `src/config.rs:328-388` — `CapabilityPermissions`, `CapabilityCompaction`, `CapabilityTranscripts`
- `src/mcp.rs:1324-1362` — `handle_capabilities_with_conn` — where live counts get overlaid

**Steps:**
1. Bump `schema_version` to `"2"` in `Capabilities` struct.
2. **Replace lying flags with honest live state:**
   - `recall_mode_active: "hybrid" | "keyword_only" | "degraded" | "disabled"` — computed from current embedder + LLM availability.
   - `reranker_active: "neural" | "lexical_fallback" | "off"` — read from the actual `CrossEncoder` enum variant at startup.
   - `permissions.mode: "advisory"` — until P4 lands; document semantics.
3. **Drop fields that don't have backing implementation:**
   - `default_timeout_seconds` (no sweeper)
   - `approval.subscribers` (no API)
   - `hooks.by_event` (no event registry)
   - `rule_summary` (always empty)
4. **Mark planned-not-implemented:**
   - `memory_reflection: { planned: true, version: "v0.7+" }` instead of `true`.
   - `compaction: { planned: true, version: "v0.8+", enabled: false }` instead of `enabled: false` alone.
   - `transcripts: { planned: true, version: "v0.7+", enabled: false }`.
5. **Preserve v1 compatibility:** when client sends `Accept-Capabilities: v1`, return the legacy shape. Default response is v2.

**Tests (add to `tests/capabilities_v2.rs`):**
- `cap_v2_reports_recall_mode_keyword_only_when_no_embedder`
- `cap_v2_reports_reranker_off_when_disabled_at_startup`
- `cap_v2_reports_reranker_lexical_fallback_when_neural_init_failed`
- `cap_v2_omits_dropped_fields_in_v2_response`
- `cap_v1_compat_returns_legacy_shape_on_accept_header`

**Success criteria:**
- [ ] No capability flag in v2 response is a hard-coded constant if the underlying machinery doesn't exist.
- [ ] Booleans that meant "this exists" become objects `{ planned, version, enabled }` when the feature is roadmap.
- [ ] `recall_mode_active` and `reranker_active` change at runtime when the underlying engine changes.
- [ ] All existing v0.6.3 callers using v1 capabilities continue to pass.

#### AI NHI prompt starter — P1

```
Role: Senior Rust engineer on ai-memory-mcp. v0.6.3 is shipped.

Context: The audit at audits/v063-source-code-audit.md identified that
src/config.rs hard-codes capability flags that promise features the code does
not implement (memory_reflection, permissions.mode, default_timeout_seconds,
hooks.by_event, rule_summary, approval.subscribers, compaction.enabled,
transcripts.enabled). The values are reported truthfully (mostly false) but
their PRESENCE in the JSON implies the feature is wired up. It is not.

Operators reading the capabilities response cannot distinguish "this feature is
disabled but available" from "this feature does not exist in this build."

Goal: Ship a Capabilities v2 schema (schema_version="2") that:
  1. Uses live runtime state for hybrid recall and reranker availability.
  2. Drops fields whose backing implementation does not exist.
  3. Marks planned features explicitly as { planned, version, enabled }.
  4. Preserves backward compat when client requests v1.

Files to touch:
  - src/config.rs (CapabilityFeatures, CapabilityPermissions,
    CapabilityCompaction, CapabilityTranscripts)
  - src/mcp.rs handle_capabilities_with_conn at L1324-1362 — wire live
    overlays.
  - src/handlers.rs HTTP capabilities endpoint — accept-header negotiation.
  - tests/capabilities_v2.rs (new file)
  - CHANGELOG.md — note schema bump.

Implementation order:
  1. Read src/config.rs:199-388 in full.
  2. Read src/mcp.rs:1324-1362 in full.
  3. Draft the v2 schema as a Rust struct with serde rename / skip rules.
  4. Implement live overlays for recall_mode_active and reranker_active.
  5. Add accept-header negotiation in handlers.rs.
  6. Add the five test cases listed.
  7. Update CHANGELOG.md.
  8. Run cargo fmt --check && cargo clippy -- -D warnings -D clippy::all -D
     clippy::pedantic && cargo test.

Anti-goals:
  - Do NOT add new features. This is a honesty patch.
  - Do NOT modify capability detection logic for tiers — only the reporting.
  - Do NOT break v1 clients. v1 must remain reachable via accept-header.

Stop after step 3 (draft schema as Rust struct) and present for review. Do not
proceed to live overlays until the schema is approved.
```

---

### Phase P2 — Data-integrity hardening (G4, G5, G6, G13)

**Goal:** Close four silent-corruption / silent-mutation paths that get worse with tier scale.

**G4 — Mixed embedding dims silently tolerated**

- Add `embedding_dim INTEGER` column to `memories` (and `archived_memories`) in migration v17.
- `set_embedding` populates the dim alongside the BLOB.
- New `Memory::store` path refuses writes whose embedding dim doesn't match the namespace's existing dim. First write to an empty namespace establishes the dim.
- Backfill migration: for existing rows, infer dim from BLOB length (`len / 4`).
- Add `dim_violations: u64` to stats (rows with mismatched or missing dim post-migration).

**G5 — Archive lossy + restore resets**

- Schema v17 adds `embedding BLOB`, `embedding_dim INTEGER`, `original_tier TEXT`, `original_expires_at TEXT` to `archived_memories`.
- `archive_memory` (`db.rs:893-938`): copy embedding + tier + expires_at into the archive row.
- `restore_archived` (`db.rs:2917-2984`): preserve original tier and expires_at; do not reset to long.

**G6 — UNIQUE(title,namespace) silent merge on conflict**

- Extend `memory_store` MCP tool with optional `on_conflict: "error" | "merge" | "version"`.
- Default for v0.6.3.1+ clients (capability-negotiated): `error`.
- v0.6.3 clients: default remains `merge` for backward compat (gate on a feature flag).
- `version` mode appends a monotonic suffix to the title (`title (2)`, `title (3)`).

**G13 — f32 endianness magic byte**

- Migration v17 prepends a 1-byte header to embeddings on write (`0x01` for little-endian f32).
- Read path checks the header; rejects with `EmbeddingFormatError` if the byte is unexpected.
- Backfill: existing rows are still readable because they don't have the header; treat absence-of-header as "legacy LE-f32" and tolerate-once. New writes always carry the byte.

**Files:**
- `migrations/sqlite/0011_v0631_data_integrity.sql` (new)
- `src/db.rs` — schema v17 hookup at `migrate()`
- `src/db.rs:893-938`, `src/db.rs:2917-2984` — archive/restore paths
- `src/mcp.rs::handle_store` (`mcp.rs:720`) — `on_conflict` wiring
- `src/embeddings.rs` — magic-byte header
- `src/db.rs::set_embedding` — dim column + magic byte writer

**Tests:**
- `archive_preserves_embedding_and_tier_on_restore`
- `mixed_dim_write_rejected_after_first_dim_established`
- `legacy_no_header_embedding_still_readable`
- `endianness_corruption_detected_on_be_byte`
- `store_on_conflict_error_returns_409`
- `store_on_conflict_merge_preserves_v063_behavior`

**Success criteria:**
- [ ] Ship-gate Phase 3 (migration round-trip) green with 1000+ archived rows including embeddings.
- [ ] `dim_violations` reports 0 on a fresh `--db` and on a properly-migrated v0.6.3 → v0.6.3.1 store.
- [ ] An attempt to store a 384-d embedding into a namespace that established 768-d returns a typed error, not a silent zero-cosine.

#### AI NHI prompt starter — P2

```
Role: Senior Rust + SQLite engineer on ai-memory-mcp.

Context: v0.6.3 has four data-integrity findings that get worse with deployment
scale. Per the v0.6.3 source-code audit at audits/v063-source-code-audit.md:

  G4: src/db.rs has no schema-level guard preventing mixed-dim embeddings (384
       MiniLM vs 768 nomic). cosine() returns 0.0 on length mismatch (L214,
       L232) and dup-check skips (L1996-2025). HNSW assumes uniform dim.
       Production hazard: silent recall collapse if an operator switches
       embedder mid-life.

  G5: archived_memories schema (db.rs:286-308) has no embedding column.
       restore_archived (db.rs:2917-2984) resets tier='long' and
       expires_at=NULL regardless of original. Archive is lossy for vector
       search and lossy for tier policy.

  G6: UNIQUE(title, namespace) + INSERT-on-conflict at db.rs:646-660 silently
       mutates an existing row instead of erroring. Title is effectively a
       primary key but the mutation is invisible.

  G13: Embeddings are stored as raw little-endian f32 BLOBs with no endianness
       header. Federation across mixed-arch clusters silently corrupts. Cheap
       to fix today, expensive after federation ships.

Goal: A single migration v17 + handler updates that fix all four. The
migration must round-trip cleanly under ship-gate Phase 3 (SQLite ↔ Postgres).

Files (read first, edit second):
  - src/db.rs (schema definition + migrate(), archive_memory at 893,
    restore_archived at 2917, set_embedding around 3123, recall paths
    referencing embedding lengths)
  - src/embeddings.rs (cosine, set_embedding helpers)
  - src/mcp.rs::handle_store at 720 (on_conflict parameter)
  - src/handlers.rs HTTP /memories/store handler
  - migrations/sqlite/0011_v0631_data_integrity.sql (new)
  - tests/data_integrity_v17.rs (new)

Implementation order:
  1. Read all four file sections in full.
  2. Draft the migration SQL: ALTER TABLEs to add embedding_dim,
     archived_memories.embedding/embedding_dim/original_tier/
     original_expires_at. Backfill embedding_dim on memories from
     length(embedding)/4. Backfill archived_memories.original_tier='long'
     and original_expires_at=NULL for rows that pre-existed (acknowledging
     the loss; the alternative is to look up the live row, which is
     impossible because it's been deleted).
  3. Add the magic-byte format: writers prepend 0x01 (LE f32). Readers tolerate
     missing-header as legacy LE-f32 but reject 0x02 (BE f32) until v0.7
     adds endianness conversion.
  4. Add on_conflict to memory_store tool schema. Default: server checks the
     client's capability profile; v1 clients keep legacy merge, v2 clients
     default to error.
  5. Update archive_memory and restore_archived to preserve the new columns.
  6. Add `dim_violations: u64` to stats.
  7. Add the 6 tests listed.
  8. Run the full quality gate.

Anti-goals:
  - Do NOT change recall scoring logic. Storage hardening only.
  - Do NOT migrate to sqlite-vec. That's a v0.9 item.
  - Do NOT remove the silent-merge codepath outright; legacy v1 clients still
    rely on it. Gate the new behavior behind capability negotiation.

Acceptance:
  - cargo test passes at >=92% coverage.
  - Ship-gate Phase 3 (migration) round-trips 1000+ archived rows including
    embeddings with zero data loss in BOTH directions.
  - A test that stores a 384-d embedding then a 768-d embedding into the same
    namespace fails the second store with a typed error.
  - A test that flips the magic byte to 0x02 in the BLOB returns a typed
    error on read (not garbage).

Stop after step 2 (migration SQL drafted) and present for review.
```

---

### Phase P3 — Recall observability (G2, G8, G11)

**Goal:** Every silent-degradation path in the recall pipeline becomes observable at request time and at capabilities time.

**G2 — HNSW silent oldest-eviction at 100k**

- `hnsw.rs:107` already evicts oldest when `MAX_ENTRIES = 100_000`. Add: emit a structured tracing event `hnsw.eviction { evicted_id, reason: "max_entries_reached" }` and increment a counter exposed via `memory_stats`.
- New stat field: `index_evictions_total: u64`.
- Capabilities v2 surfaces `hnsw.evicted_recently: bool` (last 60s rolling).

**G8 — Reranker silent fallback to lexical Jaccard**

- `reranker.rs:59-66` constructs `Lexical` when `Neural` init fails. Add: emit a startup event `reranker.fallback { from: "neural", to: "lexical", reason }`.
- `recall` response gets a new field `meta: { reranker_used: "neural" | "lexical" | "none" }`.
- Capabilities v2 `reranker_active` reflects the actual variant (already added in P1; this phase ensures the surface in `recall` matches).

**G11 — Embedder silent degrade to keyword-only**

- `mcp.rs:1289-1293` falls back when embedder fails. Add: response gets `meta: { recall_mode: "hybrid" | "keyword_only" }`.
- A new Prometheus-shaped counter: `recall_mode_total{mode="..."}`.

**Files:**
- `src/hnsw.rs` — eviction event + counter
- `src/reranker.rs` — fallback event
- `src/mcp.rs::handle_recall` — response `meta` block
- `src/db.rs::stats` — add eviction count
- `src/tracing.rs` (or wherever the tracing macros are) — new event names

**Tests:**
- `recall_response_meta_reports_keyword_only_when_embedder_disabled`
- `recall_response_meta_reports_lexical_when_neural_unavailable`
- `hnsw_eviction_increments_counter`

**Success criteria:**
- [ ] No path in the recall pipeline degrades quality without leaving a request-time trace.
- [ ] An operator running `ai-memory doctor` (P7) can see a fleet-wide histogram of recall_mode and reranker_used.

#### AI NHI prompt starter — P3

```
Role: Senior Rust observability engineer on ai-memory-mcp.

Context: Per the v0.6.3 audit, the recall pipeline has three silent degrade
paths:

  G2: src/hnsw.rs at L107 evicts the oldest entry when MAX_ENTRIES=100_000 with
      no telemetry. Operators near the cap lose data invisibly.

  G8: src/reranker.rs L59-66 silently falls back from neural BERT to lexical
      Jaccard if HF model download fails at startup. recall responses do not
      surface which mode was used.

  G11: src/mcp.rs L1289-1293 falls back from hybrid recall to keyword-only when
       the embedder fails. The recall returns; the caller has no idea
       quality dropped.

Goal: Every silent-degrade path becomes observable BOTH at request time (in
the response meta block) AND at capabilities time (already done in P1, ensure
parity).

Files:
  - src/hnsw.rs (eviction event + counter)
  - src/reranker.rs (init fallback event)
  - src/mcp.rs::handle_recall L1165-1315 (response meta)
  - src/db.rs::stats (add index_evictions_total)
  - tests/recall_observability.rs (new)

Implementation order:
  1. Read each file section in full.
  2. Define a new struct RecallMeta { recall_mode, reranker_used,
     candidate_counts: { fts, hnsw }, blend_weight }.
  3. Wire RecallMeta into the recall response under a `meta` key. Existing
     callers (which don't read meta) must continue to work.
  4. Emit tracing events at HNSW eviction and reranker init fallback. Use
     existing tracing crate macros — do NOT add new dependencies.
  5. Add the three test cases.
  6. Update PERFORMANCE.md noting that the meta block adds ~50 bytes to recall
     responses; verify the p95 budget still holds.
  7. Quality gate.

Anti-goals:
  - Do NOT change recall scoring or fusion logic.
  - Do NOT add metrics infrastructure (Prometheus exporter etc). Tracing
    events + structured response only.
  - Do NOT break clients who don't request meta — make it always-present in
    response, callers ignore unknown fields per JSON convention.

Acceptance:
  - A test that disables the embedder and runs recall returns
    response.meta.recall_mode = "keyword_only".
  - A test that forces neural reranker init failure (mock) returns
    response.meta.reranker_used = "lexical" on subsequent recalls.
  - HNSW eviction increments index_evictions_total in stats.

Stop after step 2 (RecallMeta struct drafted) and present for review.
```

---

### Phase P4 — Governance inheritance enforcement (G1) — CUTLINE-PROTECTED

**Goal:** Close the audit's biggest finding. Currently `resolve_governance_policy` checks the leaf namespace only. Children of governed parents are completely ungoverned despite the architecture page promising "Hierarchical policy inheritance (default at `org/`, overridable at `org/team/`)".

**Files:**
- `src/db.rs:3754` — `resolve_governance_policy`
- `src/mcp.rs:1054-1107` — `build_namespace_chain` (already walks chain for display; reuse for enforcement)
- `src/mcp.rs::handle_store / handle_delete / handle_promote` — call sites
- `tests/governance_inheritance.rs` (new)
- Ship-gate Phase 1 functional scenarios — add inheritance scenario

**Steps:**
1. Refactor `resolve_governance_policy(conn, namespace) -> Option<GovernancePolicy>` to:
   - Build the chain via `build_namespace_chain`.
   - Walk chain leaf-first (most specific wins).
   - Return the **first non-null policy** found.
   - Honor a per-policy `inherit: bool` flag (default `true`) — `false` blocks parent inheritance at that level.
2. Add `inherit` field to `GovernancePolicy` (`models.rs:554-563`). Migrate existing rows with `inherit = true`.
3. Add cycle-safety: chain walker already cycle-safe via `MAX_EXPLICIT_DEPTH=8` and visited set.
4. **Add ship-gate test:** parent `alphaone/secure` has `Approve` policy; child `alphaone/secure/team-a` has none → write to child must require approval.
5. **Add ship-gate test:** parent has `Approve`; child has `Any` with `inherit: false` → child writes don't require approval (explicit override).
6. **Add ship-gate test:** depth-5 chain with policy at root — leaf write requires approval.

**Tests (must be in ship-gate Phase 1 functional, not just unit):**
- `inherit_default_governance_chain_5_deep_requires_approval_at_leaf`
- `inherit_false_at_child_blocks_parent_policy`
- `most_specific_policy_wins_when_both_set`
- `child_with_no_policy_inherits_parent_policy`
- `audit_no_silent_bypass_in_v063_compatibility_path`

**Success criteria:**
- [ ] Ship-gate Phase 1 includes 5 new inheritance scenarios; all green.
- [ ] Capabilities v2 reports `governance.inheritance: "enforced"` (was `"display_only"` pre-fix).
- [ ] Architectures pages T2–T5 can stop carrying the implicit caveat that inheritance is display-only.

**Cutline statement:** **Even if everything else in v0.6.3.1 slips, this fix ships.** Document this commitment in the CHANGELOG and the v0.6.3.1 release notes.

#### AI NHI prompt starter — P4

```
Role: Senior Rust + security-minded engineer on ai-memory-mcp.

Context: The v0.6.3 audit's highest-severity finding (G1) is a security-
shaped bug. The architecture page T2 advertises "Hierarchical policy
inheritance (default at `org/`, overridable at `org/team/`)" and the
capabilities response advertises the inheritance feature, BUT the actual gate
in src/db.rs:3754 (`resolve_governance_policy`) only consults the leaf
namespace. A namespace `alphaone/secure/team-a` with no explicit policy is
COMPLETELY UNGOVERNED even when its parent `alphaone/secure` has an Approve
policy.

The chain walker for DISPLAY already exists at src/mcp.rs:1054-1107
(`build_namespace_chain`) — it's correct, cycle-safe, depth-8 capped. The fix
is to reuse it from the gate.

This is the v0.6.3.1 cutline-protected item. Even if every other v0.6.3.1
deliverable slips, this ships.

Goal: `resolve_governance_policy` walks the inheritance chain leaf-first and
returns the first non-null policy found. A `inherit: bool` flag on each
policy (default true) lets a child explicitly opt out of parent inheritance.

Files:
  - src/db.rs:3754 (resolve_governance_policy)
  - src/db.rs:3779-3816 (evaluate_level — verify it doesn't bypass)
  - src/db.rs:3832 (enforce_governance — call site)
  - src/mcp.rs:1054-1107 (build_namespace_chain — reuse, do not duplicate)
  - src/models.rs:554-563 (GovernancePolicy struct — add inherit field)
  - migrations/sqlite/0012_governance_inherit.sql (new) — backfill inherit=true
  - tests/governance_inheritance.rs (new)
  - tests/ship_gate_governance_inheritance.rs (new — ship-gate scenarios)
  - CHANGELOG.md — call out as cutline-protected.

Implementation order:
  1. Read src/db.rs:3754-3870 in full (the entire governance evaluation
     pipeline).
  2. Read src/mcp.rs:1054-1107 to confirm chain walker is reusable as-is from
     a non-MCP context (it is — it takes a conn).
  3. Read src/models.rs:554-596 (GovernancePolicy + GovernanceLevel +
     ApproverType).
  4. Add `inherit: bool` (#[serde(default = "default_true")]) to
     GovernancePolicy.
  5. Refactor resolve_governance_policy to:
       fn resolve_governance_policy(conn, namespace) -> Option<GovernancePolicy> {
           let chain = build_namespace_chain(conn, namespace)?; // leaf-first
           for ns in chain {
               if let Some(policy) = get_namespace_governance(conn, &ns)? {
                   return Some(policy);  // most specific wins
               }
               // implicit: keep walking if no policy at this level
           }
           // explicit: honor inherit=false at any level by stopping the walk
           None
       }
     But carefully handle the `inherit=false` semantic: at level k, if the
     namespace at level k has a policy with inherit=false, stop the walk
     (do NOT consult parents above k). If level k has no policy at all,
     keep walking. This is subtle — write the loop carefully.
  6. Write the ship-gate scenarios (must touch the actual gate, not just
     mock functions).
  7. Update Capabilities v2 to report governance.inheritance="enforced".
  8. Update architecture page T2 (in docs/architectures/t2.md or wherever the
     source lives) to remove the implicit caveat.
  9. Quality gate + ship-gate Phase 1.

Anti-goals:
  - Do NOT add new policy fields beyond `inherit`. Scope creep risk.
  - Do NOT change the existing approval workflow (pending_actions queue,
     consensus voting). Only the policy resolution path.
  - Do NOT introduce a chain cache. Profile-driven optimization is a v0.7
     item if needed.

Acceptance:
  - All five new inheritance test scenarios green.
  - Ship-gate Phase 1 with the new scenarios green.
  - A test that creates an `alphaone/secure` Approve policy and writes to
    `alphaone/secure/team-a/agent-1` returns Pending(action_id).
  - A test that adds `inherit=false` to `alphaone/secure/team-a` allows
    writes to that subtree without approval.
  - Capabilities v2 reports governance.inheritance = "enforced".

Stop after step 5 (refactored function drafted with the inherit=false
handling) and present for review BEFORE writing any tests. Inheritance
semantics are easy to get subtly wrong.
```

---

### Phase P5 — Webhook event coverage (G9)

**Goal:** Webhooks currently fire on `memory_store` only. Subscribers expecting "memory lifecycle events" are missing promote / delete / link / consolidate events, which the architecture page T3 implies as standard.

**Files:**
- `src/mcp.rs:1011` — existing `dispatch_event` call site (the only one)
- `src/mcp.rs::handle_promote` (1894), `handle_delete` (1826), `handle_link` (2139), `handle_consolidate` (2162)
- `src/subscriptions.rs` — event payload definitions
- `tests/webhook_coverage.rs` (new)

**Steps:**
1. Define event payloads for the four new event types: `memory_promote`, `memory_delete`, `memory_link_created`, `memory_consolidated`.
2. Wire `dispatch_event` into each handler. Mirror the existing `memory_store` pattern (HMAC signing, SSRF guard, async dispatch).
3. Subscribe API gains a per-event-type filter: `subscribe(event_types: ["memory_store", "memory_link_created"])`. Default = all events for backward compat.
4. Capabilities v2 surfaces `webhook_events: ["memory_store", "memory_promote", "memory_delete", "memory_link_created", "memory_consolidated"]`.

**Tests:**
- `webhook_fires_on_promote` (mock subscriber)
- `webhook_fires_on_delete`
- `webhook_fires_on_link_created`
- `webhook_fires_on_consolidate`
- `subscriber_filtered_to_store_does_not_get_delete`

**Success criteria:**
- [ ] All five event types observable end-to-end through HMAC-signed webhook.
- [ ] T3 architecture page can be updated to drop the implied caveat.

#### AI NHI prompt starter — P5

```
Role: Senior Rust engineer on ai-memory-mcp.

Context: The v0.6.3 audit (G9) found that webhook subscriptions
(src/subscriptions.rs) only fire on memory_store. The single dispatch site is
src/mcp.rs:1011. Promote, delete, link, and consolidate handlers do not
emit events, despite the T3 architecture page implying full lifecycle
coverage.

Goal: Wire dispatch_event into the four other lifecycle handlers, with a
per-event-type subscription filter for clients that want narrow coverage.

Files:
  - src/subscriptions.rs (event payload definitions, subscribe filter)
  - src/mcp.rs::handle_promote (L1894), handle_delete (L1826),
    handle_link (L2139), handle_consolidate (L2162)
  - src/db.rs::list_subscriptions (filter by event_type if present)
  - tests/webhook_coverage.rs (new)
  - migrations/sqlite/0013_webhook_event_types.sql (new — adds event_types
    JSON column to subscriptions table)

Implementation order:
  1. Read src/subscriptions.rs in full.
  2. Read each of the four handlers to understand the data shape.
  3. Define the four new event payload structs (mirror the memory_store one).
  4. Add event_types column migration; default to all-events for existing
     subscribers (backward compat).
  5. Wire dispatch_event in each handler. Be careful: dispatch must happen
     AFTER the operation succeeds (not in the same txn). Use the existing
     async pattern.
  6. Add Capabilities v2 webhook_events field.
  7. Add the five tests.
  8. Quality gate.

Anti-goals:
  - Do NOT redesign the subscription model. Filter by event_type is the only
    new capability.
  - Do NOT change HMAC signing or SSRF guard logic.
  - Do NOT make webhook delivery synchronous on the request path.

Acceptance:
  - A subscriber registered with default settings receives 5 distinct event
    types after exercising store, promote, delete, link, consolidate.
  - A subscriber filtered to ["memory_store"] only receives store events.
  - HMAC verification still passes on every event.

Stop after step 3 (event payload structs drafted) and present for review.
```

---

### Phase P6 — `budget_tokens` recall (R1)

**Goal:** Recover the prior roadmap's "killer feature, no competitor has this." Pairs with the LongMemEval reranker-variant disclosure.

**Spec:**
- `memory_recall` gains optional `budget_tokens: u32` parameter.
- When set: scoring runs as today, then a token-counted greedy fill returns the highest-ranked memories whose cumulative content tokens fit under the budget.
- Token counter uses a deterministic tokenizer (tiktoken-rs `cl100k_base` is the de facto standard for Claude/GPT context budgeting).
- Response includes `meta.budget_tokens_used`, `meta.budget_tokens_remaining`, `meta.memories_dropped`.
- If a single highest-ranked memory exceeds budget, return it anyway (one memory always returned) plus a flag `meta.budget_overflow: true`.

**Files:**
- `src/mcp.rs::handle_recall` (1165)
- `src/scoring.rs` (or wherever the scoring lives)
- `Cargo.toml` (add `tiktoken-rs`)
- `tests/budget_tokens.rs` (new)
- `docs/recall.md` — document the feature

**Tests:**
- `budget_tokens_returns_subset_under_budget`
- `budget_tokens_returns_one_memory_when_overflow`
- `budget_tokens_zero_returns_zero_memories`
- `budget_tokens_unset_preserves_v063_behavior`

**Success criteria:**
- [ ] LongMemEval re-run with `budget_tokens=4096` shows R@5 within 0.5% of unbounded; latency ≤ 90 ms p95 (autonomous tier budget).
- [ ] PERFORMANCE.md gets a new row for `memory_recall (budget)`.
- [ ] At-a-glance page can claim `budget_tokens` as a differentiator.

#### AI NHI prompt starter — P6

```
Role: Senior Rust engineer on ai-memory-mcp. Performance-conscious.

Context: The prior phased ROADMAP.md (Phase 1d) committed to a budget_tokens
parameter on memory_recall: "Give me the most relevant memories that fit in
4K tokens." It was framed as the killer feature ("no competitor has this").
The new charter set silently dropped it. ROADMAP2.md (R1) recovers it for
v0.6.3.1.

Letta has it. We don't. We need it.

Goal: memory_recall accepts an optional budget_tokens parameter and returns
the highest-ranked memories that fit under the budget, using a deterministic
tokenizer.

Files:
  - src/mcp.rs::handle_recall (L1165)
  - src/db.rs::recall_hybrid (L3199-3502) — verify scoring path is unchanged
  - Cargo.toml — add tiktoken-rs (cl100k_base)
  - tests/budget_tokens.rs (new)
  - docs/recall.md (new or extend) — document the feature
  - PERFORMANCE.md — new budget-mode row

Implementation order:
  1. Read src/mcp.rs:1165-1315 (handle_recall in full).
  2. Read src/db.rs:3199-3502 (recall_hybrid).
  3. Add tiktoken-rs to Cargo.toml; pick cl100k_base (Claude/GPT default).
  4. Extend the recall request schema with budget_tokens: Option<u32>.
  5. After scoring + reranking, before returning, do a greedy fill:
       let mut total = 0u32;
       let mut out = vec![];
       for memory in ranked.iter() {
           let tokens = tokenize(memory.content).count() as u32;
           if total + tokens > budget && !out.is_empty() { break; }
           out.push(memory.clone());
           total += tokens;
       }
       Always return at least one if any matched (overflow flag set).
  6. Add response.meta.budget_tokens_used / remaining / memories_dropped /
     budget_overflow.
  7. Add the four tests.
  8. Run a benchmark and update PERFORMANCE.md with a budget row at
     budget_tokens=4096.
  9. Quality gate.

Anti-goals:
  - Do NOT change the scoring or fusion. Budget is a post-rank filter.
  - Do NOT introduce a custom tokenizer. Use cl100k_base from tiktoken-rs.
  - Do NOT cache tokenizations (premature optimization at v0.6.3.1 scale).

Acceptance:
  - Test: budget_tokens=10 returns ≤2 short memories.
  - Test: budget_tokens=4096 returns ~5-15 typical memories.
  - Test: budget_tokens=0 returns 0 memories with overflow=false.
  - Test: budget_tokens unset preserves v0.6.3 behavior byte-for-byte.
  - Re-run LongMemEval at budget_tokens=4096; R@5 within 0.5% of unbounded.

Stop after step 4 (request schema extended) and present for review.
```

---

### Phase P7 — `ai-memory doctor` CLI (R7)

**Goal:** Operator-visible health dashboard. Reads Capabilities v2 + ad-hoc SQL. **Becomes fleet doctor at T3+ via remote queries.**

**Spec:**
- New CLI subcommand: `ai-memory doctor [--db <path>] [--remote <url>] [--json] [--fail-on-warn]`.
- Reports:
  - Memory store health: total, by tier, by namespace, expiring soon, dim violations (P2).
  - Index health: HNSW size, eviction count (P3), cold-start cost estimate.
  - Recall health: rolling recall_mode distribution, reranker_used distribution.
  - Governance health: namespaces with policy / without, inheritance chain depth distribution, pending_actions backlog age.
  - Sync health (T3+): peer mesh status, vector clock skew, last successful sync_since per peer.
  - Webhook health: subscription count, recent delivery success rate.
  - Capabilities check: any v2 flag in unexpected state.
- Exit codes: 0 = healthy, 1 = warnings, 2 = critical.

**Files:**
- `src/cli/doctor.rs` (new)
- `src/main.rs` — register subcommand
- `src/db.rs` — `doctor_*` query helpers
- `tests/doctor_cli.rs` (new)

**Tests:**
- `doctor_reports_clean_on_fresh_db`
- `doctor_warns_on_dim_violations`
- `doctor_critical_on_pending_actions_older_than_24h`
- `doctor_remote_queries_capabilities_endpoint`

**Success criteria:**
- [ ] `ai-memory doctor` on a freshly-seeded test DB reports clean.
- [ ] `ai-memory doctor` on a synthesized broken state reports the right warnings.
- [ ] T3 deploy guide gets `ai-memory doctor --remote https://node-a:9077` example.

#### AI NHI prompt starter — P7

```
Role: Senior Rust engineer + operator-experience-aware on ai-memory-mcp.

Context: The prior phased ROADMAP.md (Phase 4) committed to "ai-memory
doctor" — a memory-health dashboard reporting fragmentation, stale memories,
unresolved contradictions, sync lag. It vanished in the new charter set.
ROADMAP2.md (R7) recovers it for v0.6.3.1.

The doctor reads three new surfaces that earlier phases land:
  - Capabilities v2 (P1) — feature truth.
  - Data integrity (P2) — dim_violations, archive consistency.
  - Recall observability (P3) — eviction counter, recall_mode distribution.

It also has a remote mode that becomes the FLEET DOCTOR at T3+.

Goal: A new CLI subcommand `ai-memory doctor [--db <path>] [--remote <url>]
[--json] [--fail-on-warn]` that produces a human-readable health report and
exits with 0/1/2 based on severity.

Files:
  - src/cli/doctor.rs (new)
  - src/main.rs (register subcommand)
  - src/db.rs (doctor_* helpers)
  - tests/doctor_cli.rs (new)
  - docs/operations/doctor.md (new) — usage, examples, exit codes

Implementation order:
  1. Read src/main.rs to understand the existing subcommand registration
     (likely clap-based).
  2. Read src/handlers.rs::handle_capabilities to understand the JSON shape
     in remote mode.
  3. Define the report sections: Storage, Index, Recall, Governance, Sync,
     Webhook, Capabilities. Each section is a struct with severity + facts.
  4. Implement the local mode (--db). Each section is a query.
  5. Implement the remote mode (--remote). Sections that can't be queried
     remotely (raw SQL) get NOT_AVAILABLE.
  6. Implement --json output (machine-readable for CI usage).
  7. Implement --fail-on-warn (exit 1 if any warning).
  8. Add the four tests.
  9. Update CHANGELOG.md and add docs/operations/doctor.md.

Severity rules (initial):
  - Critical: dim_violations > 0; pending_actions older than 24h; sync skew
    > 600s; HNSW evictions > 0.
  - Warning: any silent-degrade flag from Capabilities v2 (recall_mode !=
    "hybrid" on tiers that should support it); subscription delivery success
    < 95% over last hour.
  - Info: anything else worth reporting.

Anti-goals:
  - Do NOT add new monitoring infrastructure (Prometheus, OTel exporters).
    The doctor reads existing surfaces.
  - Do NOT make doctor write to the DB. Read-only.
  - Do NOT make doctor block the database. Use indexed queries.

Acceptance:
  - `ai-memory doctor --db /tmp/clean.db` exits 0 with green report.
  - `ai-memory doctor --remote https://node-a:9077 --json` returns valid
    JSON with all sections populated.
  - A synthesized broken state (1 dim_violation row, 1 pending_action older
    than 24h) returns exit 2 with both findings reported.

Stop after step 3 (report sections defined) and present for review.
```

---

### Phase P8 — LongMemEval reranker-variant disclosure + Memory Portability Spec v1

**Goal:** The published LongMemEval R@5 97.8% / R@10 99.0% / R@20 99.8% is the keyword-only path. Publish the reranker-on / reranker-off / curator-on variants. Pair with the Memory Portability Spec v1 publication so external systems can import / export ai-memory data with a stable contract.

**Spec — LongMemEval variants:**
- Re-run on the same harness with: keyword-only (already published), semantic+rerank-on, semantic+rerank-off, autonomous+curator-on.
- Publish methodology: hardware, model versions (MiniLM 384, nomic 768, BERT cross-encoder), tokenizer.
- Add a comparison chart to `benchmarks/longmemeval/results.md`.

**Spec — Memory Portability Spec v1:**
- Document the export format (JSON + TOON) for memories, links, namespace metadata, archived memories, agents, entities, subscriptions.
- Document the migration v17 schema (post-P2) as the canonical reference schema.
- Versioned at `memory.dev/spec/v1` (or equivalent on the project's GitHub Pages site).
- Two reference implementations required for v1.0 (deferred per ROADMAP2 §7.6); for v0.6.3.1 the spec itself is the deliverable.

**Files:**
- `benchmarks/longmemeval/run_variants.sh` (new)
- `benchmarks/longmemeval/results.md` (new or update)
- `docs/spec/v1.md` (new) — Memory Portability Spec
- GitHub Pages publish under `/spec/v1/`

**Tests:**
- LongMemEval variants reproducible from the published methodology (manual at v0.6.3.1; automated in CI at v0.7).

**Success criteria:**
- [ ] Three variant rows on `benchmarks/longmemeval/results.md` (keyword, semantic+rerank-on, semantic+rerank-off).
- [ ] `memory.dev/spec/v1` (or equivalent URL) live with the export format documented.
- [ ] At-a-glance page links to the spec.

#### AI NHI prompt starter — P8

```
Role: Benchmark engineer + technical writer on ai-memory-mcp.

Context: v0.6.3 published a single LongMemEval result: R@5 97.8% / R@10
99.0% / R@20 99.8% on the keyword-only path (per the evidence page). The
reranker-on, reranker-off, and curator-on variants were never published. This
hides the quality range and prevents external comparison against systems that
publish their full configuration.

Concurrently, the v0.6.3.1 patch ships a Memory Portability Spec v1 — a public
contract for the data format so users can switch toolchains.

Goal: Two artifacts.
  A. Three new LongMemEval variant rows (semantic+rerank-on,
     semantic+rerank-off, autonomous+curator-on) with reproducible
     methodology.
  B. memory.dev/spec/v1 (or equivalent on alphaonedev.github.io/ai-memory-mcp/
     spec/v1/) documenting the export format for v0.6.3.1's schema (post-P2).

Files:
  - benchmarks/longmemeval/run_variants.sh (new)
  - benchmarks/longmemeval/results.md (new or extend)
  - benchmarks/longmemeval/methodology.md (new) — hardware, model versions,
    tokenizer, exact ai-memory invocation per variant.
  - docs/spec/v1.md (new) — Portability Spec.
  - docs/architectures/at-a-glance.html (or wherever) — link to spec.

Methodology rigor:
  - Hardware: published reference (Apple M2, 16GB).
  - Model versions: MiniLM-L6-v2 384d (sha256), nomic-embed-text v1.5 768d
    (Ollama tag), cross-encoder/ms-marco-MiniLM-L-6-v2 (HF revision).
  - Tokenizer: cl100k_base.
  - Run order: 3 warmup passes per variant, 5 measurement passes, report
    median.
  - Random seed pinned.

Portability Spec v1 must specify:
  - Export envelope: { schema_version: "v1", source: "ai-memory-v0.6.3.1",
    exported_at, namespaces[], memories[], links[], archived[], agents[],
    entities[], subscriptions[] (without secrets) }
  - Field-by-field semantics for each table.
  - The endianness magic byte from P2.
  - Forward compatibility: importers MUST preserve unknown fields.
  - JSON encoding rules for embeddings (base64 of LE-f32 + magic byte).
  - TOON encoding rules (referencing TOON spec).
  - Round-trip guarantee: export + import produces a byte-equivalent store.

Implementation order:
  1. Re-read the v0.6.3 LongMemEval published result to anchor the
     keyword-only baseline.
  2. Write run_variants.sh that exercises each variant with the right
     ai-memory --features and --tier flags.
  3. Run the variants on the reference machine; collect results.
  4. Draft results.md with a comparison table; reference methodology.md.
  5. Draft docs/spec/v1.md. Refer to the v17 schema (post-P2). Link to the
     migration SQL in the repo for ground truth.
  6. Wire docs/spec/v1.md into GitHub Pages publish.
  7. Add a link from at-a-glance to the spec.

Anti-goals:
  - Do NOT modify recall scoring to chase a higher number. Variants disclose
    the existing range honestly.
  - Do NOT publish a spec version > v1. Spec v2 with multi-implementation
    interop is a v1.0 item.
  - Do NOT include AgenticMem-specific fields in the spec. OSS only.

Acceptance:
  - results.md shows four rows (keyword baseline + 3 variants) with
    reproducible methodology.
  - docs/spec/v1.md is published at the canonical URL.
  - A round-trip test: `ai-memory export | ai-memory import` produces the
    same store (excluding timestamps).

Stop after step 1 (baseline anchored) and present the variant matrix
(model × tier × rerank-state) for review BEFORE running compute.
```

---

## 4. Tier-by-tier validation requirements

After all phases land, each tier needs explicit validation that its claims hold. These are the ship-gate scenarios specific to the v0.6.3.1 release.

### T1 validation — single node, single agent

- [ ] `ai-memory doctor` on a fresh DB exits 0.
- [ ] Capabilities v2 reports honest live state for recall/reranker.
- [ ] `budget_tokens=4096` returns memories within budget; LongMemEval R@5 ≥ 97%.
- [ ] Mixed-dim write rejected after first dim established.
- [ ] HNSW eviction increments counter; doctor surfaces it.
- [ ] Archive → restore preserves embedding, tier, expires_at.

### T2 validation — single node, many agents

- [ ] All T1 checks pass.
- [ ] **G1 inheritance:** parent Approve policy at `org/secure`; write to `org/secure/team-a/agent-1` returns Pending. (Cutline-protected.)
- [ ] Inheritance chain depth-5 enforced.
- [ ] `inherit: false` at child level blocks parent.
- [ ] Capabilities v2 reports `governance.inheritance: "enforced"`.

### T3 validation — multi-node cluster

- [ ] All T2 checks pass.
- [ ] Webhook subscriber receives all 5 event types after exercising store/promote/delete/link/consolidate.
- [ ] `ai-memory doctor --remote` returns valid health report from each peer.
- [ ] Magic-byte endianness header present on all new embeddings; legacy rows readable.
- [ ] Ship-gate Phase 4 (chaos: kill_primary_mid_write × 50) green.

### T4 validation — data-center swarm

- [ ] All T3 checks pass.
- [ ] `--features sal-postgres` build still passes after schema v17.
- [ ] Postgres adapter round-trips v17 schema (ship-gate Phase 3).

### T5 validation — global hive

- v0.6.3.1 does not target T5 capability gaps. Validation is "T4 holds at the substrate level."

---

## 5. Cross-cutting AI NHI prompt starter — full v0.6.3.1 release

Use this when handing off the entire v0.6.3.1 patch release to an AI agent. It coordinates P0–P8 with the right order and dependencies.

```
Role: Senior Rust + ops engineer + technical writer on ai-memory-mcp. You have
write access to the repo, the ship-gate, and the A2A-gate sites.

Context: ai-memory v0.6.3 shipped 2026-04-27 with 1,809 tests, 93.08%
coverage, ship-gate 4/4, A2A-gate 48/48 ironclaw-mtls, all 5 distribution
channels live, and LongMemEval R@5 97.8%. A subsequent source-code audit
identified 22 distinct gaps (G1-G16, R1-R8, public-surface lag) — see
audits/v063-source-code-audit.md and REMEDIATIONv0631.md for full mapping.

v0.6.3.1 is the closure release: every audit gap that doesn't need v0.7's new
substrate (hooks, attestation) lands here. Eight phases, P0-P8, ordered for
dependency-aware concurrency.

You will execute P0 through P8 over a 4-week window (target 19-25 sessions).

Phases (full specs in REMEDIATIONv0631.md):
  P0 - Public-surface currency (ops, no code)              [1-2 sessions]
  P1 - Capabilities v2 honesty                             [2-3 sessions]
  P2 - Data integrity (G4, G5, G6, G13)                    [4-5 sessions]
  P3 - Recall observability (G2, G8, G11)                  [2-3 sessions]
  P4 - Governance inheritance (G1) [CUTLINE-PROTECTED]     [3-4 sessions]
  P5 - Webhook event coverage (G9)                         [1-2 sessions]
  P6 - budget_tokens recall (R1)                           [2-3 sessions]
  P7 - ai-memory doctor (R7)                               [2 sessions]
  P8 - LongMemEval variants + Portability Spec v1          [2-3 sessions]

Dependency graph:
  P0 - independent
  P1 - independent
  P2 - depends on P1 (Cap v2 surfaces dim_violations)
  P3 - depends on P1 (Cap v2 surfaces reranker/recall_mode)
  P4 - independent (CUTLINE - ship even if everything else slips)
  P5 - independent
  P6 - depends on P3 (uses RecallMeta)
  P7 - depends on P1, P2, P3 (reads their surfaces)
  P8 - depends on P2 (spec describes v17 schema)

Concurrency plan:
  Week 1: P0 in parallel with P1.
  Week 2: P2 + P3 + P4 + P5 in parallel.
  Week 3: P6 + P7 + P8.
  Week 4: integration + ship-gate + A2A-gate + release.

Quality gates per phase:
  cargo fmt --check
  cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
  AI_MEMORY_NO_CONFIG=1 cargo test
  cargo audit
  cargo llvm-cov --fail-under-lines 92

Cutline (if compressed to 2 weeks):
  KEEP: P0, P1, P4 (G1), P2 (G4 only).
  DEFER: rest to v0.6.3.2.

Per-phase prompt starters live in REMEDIATIONv0631.md sections P0-P8. For
each phase:
  1. Read the phase spec.
  2. Use the per-phase prompt starter to brief yourself.
  3. Execute through the "Stop after step N" checkpoint.
  4. Present for review.
  5. Continue to completion.
  6. Run quality gate.
  7. Open PR.

Anti-goals (cross-cutting):
  - Do NOT add v0.7 features (hooks, attestation, transcripts). Out of scope.
  - Do NOT change the v0.6.3 capability tiers (keyword/semantic/smart/
    autonomous). Reporting-only changes.
  - Do NOT bump major or minor. This is v0.6.3.1.
  - Do NOT skip ship-gate or A2A-gate at release time.

Acceptance for the release:
  - All 8 phases merged.
  - Ship-gate 4/4 phases green on v0.6.3.1.
  - A2A-gate ironclaw-mtls 48/48 green (or higher if new scenarios added).
  - Capabilities v2 schema published; v1 still served on accept-header.
  - Memory Portability Spec v1 published.
  - LongMemEval variant disclosure published.
  - All 5 distribution channels publish v0.6.3.1.
  - `ai-memory doctor` returns clean on fresh DB.
  - Public-surface landing pages auto-update from summary.json.

Begin with P0. Do not start P1 until P0 is in flight (they're independent
but ops-side P0 should be visible to operators before code lands).
```

---

## 6. Tier-bound prompt starters

For when the focus is a specific architectural tier rather than a phase. Use these for tier-pinned validation cycles.

### T1 prompt starter — single-node, single-agent quality

```
Role: Quality engineer on ai-memory-mcp at T1.

Context: T1 is the reference deployment per
https://alphaonedev.github.io/ai-memory-mcp/architectures-t1.html — one
process, one consumer, zero network. The audit found T1 affected by:
  G2 (HNSW silent eviction at 100k)
  G3 (HNSW in-memory only — restart cost)
  G4 (mixed dims silently tolerated)
  G5 (archive lossy)
  G6 (UNIQUE conflict silent merge)
  G8 (reranker silent fallback)
  G10 (query expansion not piped)
  G11 (embedder silent degrade)
  R1 (budget_tokens missing)
  R7 (doctor missing)

After v0.6.3.1 phases P1-P3, P6, P7 land, T1 should be honest end-to-end.

Goal: Validate T1 post-v0.6.3.1.

Tasks:
  1. Build v0.6.3.1 from the v0631-rc tag.
  2. Run a synthetic load: 1,000 stores, 100 recalls, 10 promotes.
  3. Run `ai-memory doctor --db <path>` — expect exit 0.
  4. Run LongMemEval at budget_tokens=4096 — expect R@5 ≥ 97%.
  5. Force HNSW past 100k entries — expect doctor to surface eviction.
  6. Disable Ollama mid-run — expect recall meta.recall_mode = "keyword_only".
  7. Force reranker init failure — expect recall meta.reranker_used =
     "lexical".
  8. Mixed-dim write — expect typed error.
  9. Archive → restore — expect tier, expires_at, embedding all preserved.

Report: a single-page summary with each check pass/fail. If any fail, file
issues against the v0.6.3.1 PR. Do not gate the release on G3 (HNSW
persistence — that's v0.9).
```

### T2 prompt starter — single-node, many-agents governance

```
Role: Security engineer on ai-memory-mcp at T2.

Context: T2 is the multi-agent deployment per
https://alphaonedev.github.io/ai-memory-mcp/architectures-t2.html — one
process serving ~10 agents across namespaces. The T2 differentiator is
namespace-isolated governance with per-namespace policies.

The audit found one CRITICAL T2 finding: G1 — namespace inheritance is
display-only, not enforced. The architecture page explicitly promises
"Hierarchical policy inheritance (default at `org/`, overridable at
`org/team/`)" and the gate breaks that promise.

After v0.6.3.1 phase P4 lands, T2 should enforce inheritance.

Goal: Validate T2 post-v0.6.3.1.

Tasks:
  1. Build v0.6.3.1.
  2. Set governance policy on `alphaone/secure` to {write: Approve}.
  3. As an agent registered to `alphaone/secure/team-a/agent-1`, attempt
     memory_store. Expect Pending(action_id).
  4. Approve the action; expect commit.
  5. Set `inherit: false` on `alphaone/secure/team-a` with policy
     {write: Any}. Repeat step 3; expect immediate commit.
  6. Build a 5-deep namespace chain with policy at root only. Write at leaf.
     Expect Pending.
  7. Run capability check: governance.inheritance == "enforced".
  8. Run all 8 A2A-gate scenarios on T2 deployment.

Cutline: P4 is cutline-protected. If it didn't land, file a release blocker
immediately.

Report: a one-page T2 audit memo. If clean, recommend the v0.6.3.1 release
to v0.6.3-superseded status.
```

### T3 prompt starter — multi-node cluster validation

```
Role: SRE on ai-memory-mcp at T3.

Context: T3 is the 4-node cluster per
https://alphaonedev.github.io/ai-memory-mcp/architectures-t3.html. Quorum
writes (W=2 of N=3 default), mTLS peer mesh, sync_push fanout, vector-clock
catchup.

The audit found two T3-bites findings:
  G6 (UNIQUE conflict silent merge → partition-rejoin merges silently)
  G9 (webhooks fire on store only → federation event gap)
  G13 (cross-arch endianness in stored embeddings → fleet drift)
  R7 fleet-mode (doctor --remote)

After v0.6.3.1 P2, P5, P7 land, T3 should be tighter.

Goal: Validate T3 post-v0.6.3.1.

Tasks:
  1. Spin up the 4-node DigitalOcean ship-gate harness.
  2. Run ship-gate Phase 1-4. Expect 4/4 green.
  3. Subscribe a webhook to all 5 event types. Exercise store/promote/delete/
     link/consolidate. Expect 5 deliveries with valid HMAC.
  4. Force a partition; on each side write a row with title="X" in
     namespace="org/p". Reconnect. Verify NEITHER side silently merges (P2
     on_conflict=error gate). Verify the conflict is surfaced.
  5. Run `ai-memory doctor --remote https://node-a:9077` from outside the
     cluster. Expect a fleet health report.
  6. Run A2A-gate ironclaw-mtls suite. Expect 48/48 + any new scenarios.
  7. Force a node restart. Verify magic-byte header on all new embeddings;
     legacy rows still readable.

Report: a T3 cluster audit. If clean, the cluster certification block ships
with v0.6.3.1.
```

### T4 / T5 prompt starter — observe-only at v0.6.3.1

```
Role: Architecture analyst on ai-memory-mcp at T4/T5.

Context: T4 (rack-scale) targets v0.7 GA with Postgres. T5 (multi-region)
is v1.0+ vision. v0.6.3.1 does NOT address T4/T5 capability gaps —
attestation (G12), distributed consensus, hardware key custody. Those are
ROADMAP2.md items.

Goal: Verify v0.6.3.1 doesn't regress the T4/T5 substrate.

Tasks:
  1. Build with --features sal-postgres on a v0.6.3.1 checkout.
  2. Run ship-gate Phase 3 (SQLite ↔ Postgres migration). Expect green
     including the v17 schema additions.
  3. Document any new fields in the Postgres adapter that need attention
     in v0.7 GA.
  4. Confirm signature column is still reserved (not used) — v0.7 territory.
  5. File no v0.6.3.1 blockers; file v0.7 follow-ups for any T4/T5-relevant
     observations.

Report: an analyst memo summarizing the T4/T5 posture for the v0.6.3.1
release. Use this to set v0.7 expectations.
```

---

## 7. Pre-flight, gate, and ship checklist

Use this at v0.6.3.1 RC time.

### Pre-flight (per phase)

- [ ] Phase spec read in full
- [ ] Existing files at the cited line numbers re-read (audit was of v0.6.3; some line numbers may shift if other patches landed)
- [ ] AI NHI prompt starter pasted into the agent session
- [ ] "Stop after step N" checkpoint hit; review approved
- [ ] Phase complete; quality gate green
- [ ] PR opened, reviewed, merged

### Ship-gate (release-time)

- [ ] All 8 phases (or cutline subset) merged to main
- [ ] Schema v17 migration tested in both directions (SQLite ↔ Postgres)
- [ ] Capabilities v2 + v1 both servable
- [ ] Ship-gate Phase 1 (functional) — including 5 new G1 inheritance scenarios — green
- [ ] Ship-gate Phase 2 (federation / quorum) — green
- [ ] Ship-gate Phase 3 (migration) — green with v17 schema
- [ ] Ship-gate Phase 4 (chaos) — green
- [ ] A2A-gate ironclaw-mtls 48/48 + any new scenarios — green
- [ ] All 5 distribution channels build cleanly
- [ ] CHANGELOG.md updated
- [ ] ROADMAP2.md "v0.6.3.1" section marked SHIPPED with date
- [ ] Public-surface landing pages auto-update from new summary.json

### Post-ship

- [ ] LongMemEval variant chart published
- [ ] Memory Portability Spec v1 published at canonical URL
- [ ] At-a-glance page links to the spec
- [ ] Architectures pages T1, T2, T3 updated to reflect inheritance enforcement
- [ ] `ai-memory doctor` referenced in README and operations docs
- [ ] v0.6.3 marked SUPERSEDED on test hub
- [ ] v0.7 work begins (next milestone)

---

## 8. Net

ai-memory v0.6.3 is shippable, and it shipped clean. v0.6.3.1 is the closure release that makes the published architectures and capabilities **honest at every tier**. Eight phases, four weeks, twenty audit findings closed (G1–G16 except those legitimately deferred to v0.7+, plus R1 and R7 recovered, plus the public-surface lag fixed).

The single highest-leverage move is **P4 — namespace inheritance enforcement (G1) — cutline-protected.** If the calendar compresses, P4 ships even if seven other phases slip. It's the only audit finding where the published architecture page promises something the code does not deliver in a security-shaped way.

Every phase has a copy-paste AI NHI prompt starter sized so an AI coding agent can pick it up cold, hit a "stop after step N" checkpoint for human review, and finish the work under the existing CODEOWNERS gate. The prompt starters embed the file paths, line numbers, anti-goals, and acceptance criteria so review can focus on judgment rather than archaeology.

After v0.6.3.1: T1 is honest, T2 has real inheritance, T3 has full webhook event coverage, T4 substrate is preserved for v0.7 GA, T5 vision is unchanged. The OSS substrate is correct. The architecture pages stop carrying implicit caveats. The next milestone is v0.7 Trust + A2A Maturity per ROADMAP2.md §7.3.

---

*Document classification: Public-facing. Intended location: `github.com/alphaonedev/ai-memory-mcp/blob/main/REMEDIATIONv0631.md`.*

*Companion: ROADMAP2.md (forward plan). audits/v063-source-code-audit.md (audit detail). architectures.html + T1–T5 (architectural ground truth).*
