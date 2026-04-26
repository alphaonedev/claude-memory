# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] — v0.6.3 (Patch 4)

### Added

- **Hierarchical namespace taxonomy (Pillar 1 / Stream A)** — new
  `memory_get_taxonomy` MCP tool plus REST mirror at
  `GET /api/v1/taxonomy`. Walks live (non-expired) memories grouped by
  `namespace`, splits on `/`, and folds them into a `TaxonomyNode` tree.
  Each node carries `count` (memories at exactly this namespace) and
  `subtree_count` (count plus every descendant the depth limit allowed
  us to expand); the response envelope adds `total_count` (an
  independent aggregation that stays honest even when `limit` drops
  rows from the walk) and a `truncated` flag. Parameters:
  `namespace_prefix` (optional, accepts trailing `/`),
  `depth` (default 8 = `MAX_NAMESPACE_DEPTH`, clamped),
  `limit` (default 1000, hard ceiling 10000 — densest namespaces win
  when truncated). Closes the "flat blob" perception gap from charter
  §"The Demo That Sells It" (charter lines 218–230) and unblocks the
  taxonomy demo CLI surface deferred to a later iteration. Charter
  §"Stream A — Hierarchy", lines 320–326.

- **Temporal-validity KG schema (Stream B foundation)** — SQLite schema
  bumps to v15 (`src/db.rs::migrate`). `memory_links` gains four nullable
  temporal columns — `valid_from`, `valid_until`, `observed_by` (TEXT),
  and `signature` (BLOB; placeholder for v0.7 attested identity). On
  upgrade, existing links are backfilled: `valid_from` is set to the
  source memory's `created_at` (charter pre-flight default — defensive
  null avoidance). Three temporal indexes are created for the upcoming
  recursive-CTE traversal in `memory_kg_query` / `memory_kg_timeline`:
  `idx_links_temporal_src` (source_id, valid_from, valid_until),
  `idx_links_temporal_tgt` (target_id, valid_from, valid_until), and
  `idx_links_relation` (relation, valid_from). New `entity_aliases`
  side table (entity_id, alias, created_at; PK on entity_id+alias)
  with `idx_entity_aliases_alias` lookup index unblocks the upcoming
  Stream C entity-registry tools. The Postgres declarative schema
  (`src/store/postgres_schema.sql`) is mirrored for fresh-init parity;
  existing PG installs do not auto-gain the new columns since the PG
  store layer is still WIP (an explicit ALTER migration lands when
  `link()` is wired up there). Pure additive — no existing query
  breaks. Charter §"Critical Schema Reference", lines 686–723.

- **Entity registry (Pillar 2 / Stream B)** — `memory_entity_register`
  + `memory_entity_get_by_alias` MCP tools (count 38 → 40) plus the
  matching HTTP surface (`POST /api/v1/entities`,
  `GET /api/v1/entities/by_alias`, with 201 / 200 / 409 status
  discipline and `X-Agent-Id` honoured). Entities are long-tier
  memories tagged `entity` with `metadata.kind = "entity"`; aliases
  live in the v15 `entity_aliases` side table. Registration is
  idempotent on `(canonical_name, namespace)` — re-registering reuses
  the entity_id and merges new aliases via `INSERT OR IGNORE`. A
  non-entity memory occupying the same `(title, namespace)` returns a
  hard error rather than letting the upsert path silently overwrite
  unrelated content. Resolver returns the most-recently-created
  entity when no namespace filter is supplied; ignores stray
  `entity_aliases` rows that point at non-entity memories. Builds on
  the v15 schema (#384). Charter §"Stream B — KG Schema + Entity
  Model", lines 369–375.

- **`memory_kg_timeline` (Pillar 2 / Stream C)** — entity-anchored
  chronological view powering the `ai-memory kg-timeline` headline
  demo. `db::kg_timeline()` queries `memory_links` ordered by
  `valid_from ASC` (tie-break `created_at`) with optional inclusive
  `since` / `until` filters; limit clamps to `[1, 1000]`, default
  200. `db::create_link()` now stamps `valid_from = created_at` on
  every insert so newly created links are visible to the timeline
  without a later sweep, closing the forward gap left by the v15
  backfill of legacy rows. `memory_kg_timeline` MCP tool (count
  40 → 41) plus `GET /api/v1/kg/timeline?source_id=…&since=…
  &until=…&limit=…`. Returns `KgTimelineEvent` carrying `target_id`,
  `relation`, validity window, `observed_by`, and the target's
  `title` / `namespace`. Charter §"Stream C — KG Query Layer",
  lines 377–383.

- **`memory_kg_invalidate` (Pillar 2 / Stream C)** — second tool of
  the KG-traversal triplet. Marks a KG link as superseded by setting
  its `valid_until` column so a contradicting fact can invalidate
  the prior assertion without deleting the row, preserving the
  timeline. The link is identified by its composite key
  `(source_id, target_id, relation)` since `memory_links` has no
  separate id; `valid_until` defaults to wall-clock now when
  omitted. `db::invalidate_link()` returns
  `Option<InvalidateResult>` — `None` when the triple does not
  match, `Some` with the value now stored and `previous_valid_until`
  so callers can distinguish a fresh supersession from an idempotent
  retry. `memory_kg_invalidate` MCP tool (count 41 → 42) plus HTTP.
  Schema does not yet carry an audit column for the supersession
  `reason`; that arrives with v0.7 attestation. Charter §"Stream C —
  KG Query Layer", lines 377–383.

- **`memory_kg_query` depth=1 (Pillar 2 / Stream C)** — outbound
  "expand neighbors" first slice. `memory_kg_query` MCP tool (count
  42 → 43) plus HTTP. `db::kg_query()` ships with constants
  `KG_QUERY_DEFAULT_LIMIT = 200`, `KG_QUERY_MAX_LIMIT = 1000`, and
  `KG_QUERY_MAX_SUPPORTED_DEPTH = 1`; callers passing `max_depth=2`
  get a clean error rather than a silent truncation, so the API
  contract is stable from day one — the recursive-CTE multi-hop
  follow-up just lifts the ceiling without changing the surface.
  Filters per the charter spec: `valid_at` (RFC3339, only links
  valid at that instant); `allowed_agents` (only links observed by
  an agent in the set; **empty list returns zero rows by design** —
  callers signaling "no agents trusted" must get an empty traversal,
  not the unfiltered fallback); `limit` clamped to `[1, 1000]`.
  Charter §"Stream C — KG Query Layer", lines 377–383.

- **`memory_kg_query` depth 2..=5 (Pillar 2 / Stream C)** — lifts
  `KG_QUERY_MAX_SUPPORTED_DEPTH` from 1 to 5, matching the published
  `memory_kg_query (depth ≤ 5)` 250 ms p95 / 500 ms p99 budget in
  `PERFORMANCE.md`. Replaces the depth=1 JOIN with a recursive CTE
  that re-applies the temporal / agent filter on every hop and
  prunes cycles via the accumulated `path`; each row's `depth` +
  `path` now reflect the actual chain (e.g. depth=2 →
  `src->mid->target`). API contract is unchanged — depth=1 collapses
  to the original time-ordered single-hop result, and the
  over-ceiling MCP/HTTP error path (422 with `max_depth=N exceeds
  supported depth=5`) is preserved. Closes the Stream C
  `memory_kg_query` slice; traversals at depth 2..=5 are now correct
  under temporal-validity and observed-by filtering. Charter
  §"Stream C — KG Query Layer", lines 377–383.

- **`memory_check_duplicate` (Pillar 2 / Stream D)** — pre-write
  near-duplicate check across DB / MCP / HTTP. `db::check_duplicate`
  performs a cosine scan over live embedded memories with the
  threshold clamped at `DUPLICATE_THRESHOLD_MIN = 0.5` (so permissive
  callers can't dress unrelated content as a merge candidate) and
  default `DUPLICATE_THRESHOLD_DEFAULT = 0.85` (tuned for the
  MiniLM-L6-v2 embedder — near-paraphrases land ≥ 0.88, loosely
  related content sits well below). `memory_check_duplicate` MCP
  tool (count 37 → 38) returns the nearest-neighbor cosine, the
  above-threshold boolean, and an optional `suggested_merge` target.
  HTTP `POST /api/v1/check_duplicate` mirrors the MCP surface and
  embeds *before* taking the DB lock (issue #219 pattern). Charter
  §"Stream D — Duplicate Check", lines 384–386.

- **`ai-memory bench` scaffold (Pillar 3 / Stream E)** — first slice
  of perf instrumentation. New CLI subcommand + `src/bench.rs`
  runner so operators (and the `bench.yml` CI guard / Stream F) can
  verify the published `PERFORMANCE.md` budgets. Covers the three
  embedding-free hot-path operations: `memory_store` (no embedding)
  / 20 ms p95, `memory_search` (FTS5) / 100 ms p95, and
  `memory_recall` (hot, depth=1) / 50 ms p95. Each invocation seeds
  a disposable `:memory:` SQLite DB so the operator's main DB is
  untouched. Reports p50 / p95 / p99 in either a human table or
  `--json`. Exit code is non-zero when any p95 exceeds its target
  by more than the documented 10% tolerance — so the same binary
  slots into the CI guard once Stream F lands. `PERFORMANCE.md`
  status table now distinguishes "scaffold landed" from "Stream E
  follow-up" so partial coverage isn't silent. Charter §"Stream E —
  Performance Instrumentation", lines 388–393.

- **Performance budgets published** — new `PERFORMANCE.md` at the repo
  root carries the authoritative p95/p99 latency contract for every
  hot-path operation (verbatim from the v0.6.3 grand-slam charter):
  `memory_session_start` hook, `memory_recall` hot/cold,
  `memory_store` with/without embedding, `memory_search`,
  `memory_check_duplicate`, `memory_kg_query` (depth ≤ 3 / ≤ 5),
  `memory_kg_timeline`, `memory_get_taxonomy`, `curator cycle`, and
  `federation ack`. Documents the **>10% p95 breach fails CI**
  threshold (p99 informational until the v0.6.3 soak window closes),
  the Apple M4 / 32 GB / NVMe SSD reference hardware baseline (with a
  note on Linux x86_64 CI parity), and a status table flagging the
  bench tool (Stream E) and `bench.yml` workflow (Stream F) as still
  in-flight. Closes Pillar 3 / Stream F doc deliverable from the
  v0.6.3 charter.

- **`bench.yml` CI guard (Pillar 3 / Stream F)** — new
  `.github/workflows/bench.yml` runs `ai-memory bench` on every pull
  request and trunk push (`main`, `develop`, `release/**`) plus on
  manual `workflow_dispatch`. The job builds the release binary on
  `ubuntu-latest` (the latency reference per `PERFORMANCE.md`),
  streams the bench table into the workflow run summary, and uploads
  a `bench-results` artifact (`bench-results.json` +
  `bench-table.txt`) for downstream tooling. The `ai-memory bench`
  binary already exits non-zero when any operation's measured p95
  exceeds its target by more than the published 10% tolerance, so
  the workflow fails on regression without additional gating logic.
  Closes the last Stream F deliverable from charter §"Stream F —
  Performance Budgets + CI Guard"; budgets are now continuously
  enforced against trunk and PRs.

- **`ai-memory bench` KG depth=3 + depth=5 coverage (Pillar 3 / Stream E)**
  — `memory_kg_query` is now exercised at the deepest hop of both
  documented budget buckets: depth=3 against the "depth ≤ 3" 100 ms
  p95 row and depth=5 against the "depth ≤ 5" 250 ms tail-case row in
  `PERFORMANCE.md`. The runner seeds a second in-process fixture (50
  chains × 5 hops each = 300 memories + 250 links) so the recursive
  CTE actually traverses three / five hops per query rather than
  collapsing to a single hop on the existing fan-out fixture. Local M4
  measurements: depth=3 p95 ~0.6 ms, depth=5 p95 ~0.7 ms — both PASS,
  both well inside the 10% tolerance enforced by `bench.yml`. No new
  dependencies. Completes the KG half of Stream E; embedding-bound
  paths still need a fixture decision and remain tracked separately.

- **`ai-memory bench` KG coverage (Pillar 3 / Stream E)** —
  `memory_kg_query` (depth=1) and `memory_kg_timeline` are now driven
  by the `bench` subcommand against the same in-memory disposable
  SQLite database used by the embedding-free operations. The runner
  seeds an in-process KG fixture (50 source memories × 4 outbound
  links each, every link `valid_from`-stamped so `kg_timeline` sees
  them) and reports p50/p95/p99 against the 100 ms p95 budgets
  published in `PERFORMANCE.md`. Local M4 measurements: `kg_query`
  p95 ~0.7 ms, `kg_timeline` p95 ~0.1 ms — both PASS, both well
  inside the 10% tolerance enforced by the `bench.yml` CI guard.
  No new dependencies. Closes the KG half of the iter-0017 follow-up
  ask; embedding-bound paths still need a fixture decision and are
  tracked separately.

- **Per-tool MCP tracing spans (Pillar 3 / Stream E)** — every
  `tools/call` dispatch now runs inside an `info`-level
  `mcp_tool_call` span carrying the tool name and JSON-RPC id. After
  the handler returns, an `ok` event records `elapsed_ms`; an
  `Err` outcome emits a `warn` event with the error message so
  on-call dashboards can alert on per-tool error rate. The MCP server
  entrypoint (`run_mcp_server`) installs a `tracing_subscriber::fmt`
  subscriber pinned to `stderr` (stdio JSON-RPC owns stdout) honoring
  `RUST_LOG`; `try_init` makes it a no-op when another command in the
  same process already initialised tracing. Foundation for the v0.6.3
  charter §"Stream E — Performance Instrumentation" ask;
  paired with the `ai-memory bench` scaffold to give exporters
  per-tool latency attribution against the published `PERFORMANCE.md`
  budgets.

### Fixed

- **[#358]** mTLS allowlist parser now tolerates inline trailing `#`
  comments after a fingerprint
  (`load_fingerprint_allowlist`, `src/main.rs`). Previously, a line like
  `sha256:abc…def  # node-1` was parsed whole and failed the 64-hex-char
  length check (`got 74`), aborting `ai-memory serve` on startup. Full-line
  `#` comments and the Ultrareview #338 strict character-set check
  (rejects embedded whitespace inside the hex run) are preserved. Doc
  update: `docs/ADMIN_GUIDE.md` now explicitly calls out trailing-comment
  tolerance. Encountered in the a2a-gate mTLS matrix; the gate-side
  generator fix in `ai-memory-ai2ai-gate#35` already worked around it for
  v0.6.2 — this is the parser-side resolution.

### Tests

- **[#401]** RAII `ChildGuard` fixes mTLS test daemon-leak on assert
  panic.
  `tests/integration.rs::test_serve_mtls_fingerprint_allowlist_accepts_only_known_peer`
  was leaking `target/debug/ai-memory … serve` child processes
  whenever any of its 4 asserts panicked between spawn and the
  manual `kill()` at the bottom — `std::process::Child` has no
  kill-on-drop on Unix. Adds a generic `ChildGuard { child:
  Option<Child>, cleanup_paths: Vec<PathBuf> }` alongside the
  existing `DaemonGuard`, with an unwind-safe `Drop` that kills,
  reaps, and unlinks; refactors the mTLS test to wrap both spawned
  children. End-user impact is zero (production `serve` deployments
  via systemd / launchd / Docker reap children correctly), but the
  campaign runner had been accumulating ~28 GB of orphaned daemons
  across 7 reparented PIDs during the v0.6.3 dev sprint.

## [v0.6.2] — 2026-04-24 — A2A-CERTIFIED

First release to carry the a2a-gate **consecutive-green streak 3/3**
certification. Three consecutive full-testbook passes across six
homogeneous cells (ironclaw + hermes × off/tls/mtls on DigitalOcean,
and openclaw × off on a local Docker mesh) validate that A2A
scenarios against ai-memory v0.6.2 are green end-to-end on
`release/v0.6.2 @ 3e018d6`.

**Evidence** — every scenario artifact is committed alongside the
releasing branch of the a2a-gate repo:
<https://alphaonedev.github.io/ai-memory-ai2ai-gate/runs/>

### Fixed — federation fanout correctness (a2a-gate v3r22–r30)

- **[#325]** `create_link` fanout — `POST /api/v1/links` broadcasts
  the new link to every peer via quorum write. Scenario-11 of the
  a2a-gate harness exercised this: charlie couldn't see an M1→M2
  link written on alice's node. `SyncPushBody` grows a
  `links: Vec<MemoryLink>` field applied via `db::create_link` on
  peers; duplicates are idempotent via the existing
  `(source_id, target_id, relation)` unique index. New
  `federation::broadcast_link_quorum`. Delete-link fanout deferred
  to v0.7 CRDT-lite tombstones.
- **[#326]** `consolidate` fanout — `POST /api/v1/consolidate`
  broadcasts the new consolidated memory AND the source-id
  deletions in a single sync_push call. Scenario-5 exposed the
  gap: peer nodes never saw the consolidated memory, so
  `metadata.consolidated_from_agents` read as `"[]"`. New
  `federation::broadcast_consolidate_quorum`.
- **[#327]** Embedder-failure visibility on `ai-memory serve` —
  HuggingFace-Hub fetch failure now logs at `ERROR` with an
  `⚠️ EMBEDDER LOAD FAILED` marker and a remediation pointer.
  `/api/v1/health` grows `embedder_ready: bool` +
  `federation_enabled: bool` fields so harnesses can assert
  semantic-tier readiness before scenarios run.
- **[#363]** List cap 200 → 1000 + pending-action fanout +
  namespace_meta fanout (S34 / S35 / S40). Closed the three
  fanout gaps surfaced by v3r22.
- **[#364]** `clear_namespace_standard` fanout symmetry follow-up
  to #363 — the clear path was missing from `SyncPushBody`;
  scenario-35 on peer-nodes saw stale standards after a clear on
  the leader.
- **[#366]** HTTP `/api/v1/recall` now uses hybrid semantic when
  the embedder is loaded. Scenario-18 previously black-holed
  because the endpoint fell through to FTS-only even with a live
  embedder.
- **[#367]** Relax semantic cosine threshold 0.3 → 0.2 in
  `recall_hybrid`. Scenario-18 caught a miss at 0.25–0.29 cosine
  for legitimately-related content; the lower threshold preserves
  top-K recall without introducing noise (blended score still
  gated by `fts.rank + …` component).
- **[#368]** S40 fanout retry — `post_and_classify` retries once
  on `AckOutcome::Fail` with a 250 ms backoff. `Idempotency-Key`
  already present on `sync_push` makes a partial-apply race
  dedupe to a no-op on the peer via `insert_if_newer`. RCA:
  v3r26 hermes-tls scenario-40 saw `node-2 499/500 bulk rows`
  post-quorum because the detached per-peer POST had transiently
  failed; no retry, no catchup.
- **[#369]** S40 `bulk_create` terminal catchup batch per peer.
  After the per-row quorum drains, the leader sends ONE batched
  `sync_push` per peer with every committed row. Peer-side
  `insert_if_newer` dedupes already-applied rows; rows dropped by
  the detached path land now. O(1) extra POST per peer vs O(N)
  retries per row. Proven to close the gap on v3r28 after retry
  alone was insufficient on v3r27 (ironclaw-off still dropped one
  row despite the retry — sustained SQLite-mutex contention
  during a 500-row burst can drop two consecutive POSTs).

### Evidence & reproducibility

The a2a-gate repository carries the full certification evidence:

- **Runs dashboard** —
  <https://alphaonedev.github.io/ai-memory-ai2ai-gate/runs/>
- **AI NHI insights** (tri-audience analysis) —
  <https://alphaonedev.github.io/ai-memory-ai2ai-gate/insights/>
- **Local Docker mesh reproducibility spec** —
  <https://alphaonedev.github.io/ai-memory-ai2ai-gate/local-docker-mesh/>

Per-campaign evidence pages under `runs/` carry scenario-level
JSON, stderr logs, baseline attestation, F3 peer-replication
canary, and a campaign.meta.json provenance trace. The DO
campaigns (v3r28 / v3r29 / v3r30) used `release/v0.6.2 @ 3e018d6`
with `ai_memory_source_build=true`; the local-docker campaigns
(r1 / r2 / r3) used the same commit via a committed release
binary.

### Certification matrix

| | off | tls | mtls |
|---|---|---|---|
| **ironclaw (DO)** | ✅ v3r30 35/35 | ✅ v3r30 35/35 | ✅ v3r30 37/37 |
| **hermes (DO)** | ✅ v3r30 35/35 | ✅ v3r30 35/35 | ✅ v3r30 37/37 |
| **openclaw (local-docker)** | ✅ r3 35/35 | ⏸ Phase 3 | ⏸ Phase 3 |

Total: **214 passing scenarios** across six cells on the final
certification run (v3r30 DO + local-docker r3).

## [Unreleased] — v0.6.1 + v0.7 tracks

### Fixed — v0.6.0 pre-tag SAL blocker punchlist (#293)

Five correctness blockers surfaced by the v0.6.0 code-review (meta
issue [#293](https://github.com/alphaonedev/ai-memory-mcp/issues/293)),
all closed before the tag:

- **[#294]** SAL upsert key mismatch — aligned Postgres adapter to
  `ON CONFLICT (title, namespace)` matching SQLite's documented
  contract. Added `UNIQUE INDEX memories_title_ns_uidx` to
  `postgres_schema.sql`.
- **[#295]** `metadata.agent_id` immutability — Postgres UPSERT and
  UPDATE now preserve the original `agent_id` via `jsonb_set` CASE
  clause, mirroring SQLite's `json_set` SQL-layer guard. Task 1.2
  NHI invariant is now enforced on both adapters.
- **[#296]** Tier-downgrade protection on Postgres UPDATE — added
  `tier_rank()` SQL function and `GREATEST(tier_rank(...))`
  precedence so `Long → *` and `Mid → Short` are refused at the
  SQL layer, matching SQLite.
- **[#297]** Postgres schema parity — added 6 tables + generated
  `scope_idx` column (memory_links, archived_memories,
  namespace_meta, pending_actions, sync_state, subscriptions) so
  cross-backend migration is no longer lossy beyond the memories
  table.
- **[#298]** Migration cursor data loss — the prior
  `created_at`-based pagination silently dropped low-priority
  memories under `priority DESC` list ordering. Replaced with a
  single-call `MAX_ROWS=1M` migrate that refuses loudly when
  saturated. Streaming migrate for corpora >1M rows tracked for
  v0.7 with `MemoryStore::list_all`.

New regression tests (behind `AI_MEMORY_TEST_POSTGRES_URL`):
`upserts_by_title_namespace_not_id`, `upsert_preserves_agent_id`,
`update_refuses_tier_downgrade`. Plus `migrate_sqlite_to_sqlite_roundtrip`
tightened to assert single-call semantics.

### Removed — TurboQuant embedding compression scrapped

TurboQuant (Google Research, arXiv 2504.19874) was evaluated as an
embedding-compression path for ai-memory (PRs #284 and #287). Both
closed unmerged. The `alphaonedev/turboquant` fork was archived.
Decision rationale: the ~2× embedding storage reduction at 4
bit-width is irrelevant at ai-memory's target scale (<100k memories
per deployment); beyond that, Postgres + pgvector (#279) is the right
answer. The fork-maintenance + heavy-transitive-deps burden (ort,
tokenizers, safetensors, burn) was not justified by the marginal
gain. Real compression wins live elsewhere: Ollama KV compression
(#288 runbook) for inference memory, Postgres + pgvector for native
vector storage at scale, SQLCipher at rest (shipped) for data-at-rest
protection.

### Added — world-class documentation sprint

Seven new authoritative docs close the reference-material gaps in
the existing `docs/` tree:

- **`docs/README.md`** — navigation hub grouping every doc by audience
  (end users, admins, developers, design decisions, SDKs).
- **`docs/QUICKSTART.md`** — first memory stored + recalled in under
  5 minutes across three paths (CLI, MCP with Claude Code / Cursor /
  Codex, HTTP daemon).
- **`docs/CLI_REFERENCE.md`** — every subcommand, flag, and
  environment variable the `ai-memory` binary exposes. Auto-synced
  to `src/main.rs` clap definitions.
- **`docs/API_REFERENCE.md`** — every HTTP endpoint the daemon
  exposes, with payload shapes, query params, status codes, and
  `curl` recipes. 24+ endpoints.
- **`docs/GLOSSARY.md`** — every concept (agent, tier, scope,
  curator, quorum, SAL, …) with single-paragraph definitions and
  links to authoritative docs.
- **`docs/TROUBLESHOOTING.md`** — common errors (startup, MCP,
  autonomy, HTTP, sync, performance, governance) with root-cause
  analysis and fixes.
- **`docs/SECURITY.md`** — complete threat model, trust boundaries,
  auth stack (API key + mTLS Layer 1/2/2b), SQLCipher at rest,
  SSRF-hardened webhook dispatch, responsible disclosure process.

Existing docs (`USER_GUIDE.md`, `ADMIN_GUIDE.md`, `DEVELOPER_GUIDE.md`,
`INSTALL.md`, `PHASE-1.md`, `AI_DEVELOPER_*.md`, `ENGINEERING_STANDARDS.md`,
`ARCHITECTURAL_LIMITS.md`, `ADR-0001-quorum-replication.md`,
`RUNBOOK-*.md`) cross-linked from `docs/README.md` for discovery.

### Added — v0.7 Storage Abstraction Layer (Track B PR 1)

- **Storage Abstraction Layer (SAL) — `MemoryStore` trait + `SqliteStore`
  + `PostgresStore`** — preview surface for v0.7. Gated behind
  `--features sal` (trait + sqlite adapter) and `--features sal-postgres`
  (adds the Postgres + pgvector backend). Default builds unchanged.
  Trait design carries over from the red-team-hardened #222 proposal:
  typed `StoreError` with `#[non_exhaustive]`, `CallerContext` on every
  mutator, optional `Transaction` handle, `verify()` contract, advertised
  `Capabilities` bitflags (NATIVE_VECTOR, FULLTEXT, DURABLE, etc.).
- **Postgres adapter ships with**:
  - `src/store/postgres_schema.sql` — idempotent bootstrap creating the
    `memories` table with a `vector(384)` column, pgvector `hnsw` index
    for cosine NN search, `gin` FTS + tags + metadata indexes.
  - `packaging/docker-compose.postgres.yml` — `pgvector/pgvector:pg16`
    fixture for integration tests. Hardened container
    (`cap_drop: [ALL]`, `no-new-privileges`, tmpfs for `/tmp`).
  - Live integration tests in `src/store/postgres.rs` that skip when
    `AI_MEMORY_TEST_POSTGRES_URL` is unset — keeps default `cargo test`
    offline while giving CI a straightforward opt-in path.
  - Unit-level tests: capability bits, RFC3339 parse helpers, schema
    constants.

### Added — v0.7 quorum replication primitives (Track C PR 1)

- **ADR-0001 — Quorum replication + chaos-testing methodology**
  (`docs/ADR-0001-quorum-replication.md`). Full design doc covering the
  W-of-N write-quorum model, failure modes, chaos-fault classes, and
  the implementation phasing. Explicitly states that v0.7 will NOT
  publish a "<0.01% loss" probability — instead it will publish a
  convergence-bound report per chaos campaign.
- **Quorum-write primitives** (`src/replication.rs`) — `QuorumPolicy`
  (N / W / deadlines / clock-skew threshold), `AckTracker` (collects
  local commit + peer acks, surfaces timeouts + id-drift), typed
  `QuorumError`. Pure-logic, I/O-free so unit tests don't need a live
  peer mesh.
- **12 unit tests** covering: single-node degenerate case,
  majority-default, W clamping, peer ack deduplication, deadline
  expiry reporting Unreachable vs Timeout, id-drift handling,
  Error trait participation.

### Added — v0.6.1 curator daemon (Track A)

### Added
- **Autonomous curator daemon** — new `ai-memory curator` subcommand with
  `--once` (single sweep + JSON report) and `--daemon` (continuous loop,
  interval configurable via `--interval-secs`, clamped to `[60, 86400]`).
  Invokes `auto_tag` + `detect_contradiction` on memories that lack an
  `auto_tags` metadata key, persisting results on success. Dry-run mode
  emits the same report without touching any row. Hard operation cap
  per cycle (`--max-ops`, default 100) prevents runaway LLM usage.
  Complements the synchronous post-store hooks shipped in v0.6.0.0
  (#265) — the curator catches memories stored before hooks were enabled,
  or when the LLM was offline, or that become interesting only after
  more context accumulates.
- **Curator systemd unit** — `packaging/systemd/ai-memory-curator.service`
  with the same sandbox posture as the main daemon
  (`ProtectSystem=strict`, empty `CapabilityBoundingSet`,
  `MemoryDenyWriteExecute`, `@system-service` syscall filter).
- **Curator Prometheus metrics** — `ai_memory_curator_cycles_total`,
  `ai_memory_curator_operations_total{kind,result}`,
  `ai_memory_curator_cycle_duration_seconds{dry_run}`.

### Added — full autonomy loop (earning the "100% autonomous" claim)

Builds on Track A's curator with the four passes required to make the
"100% autonomous" claim honest:

- **Autonomous consolidation** — the curator scans each namespace for
  near-duplicate memories (Jaccard keyword overlap ≥ 0.55 on a
  token-length-≥3 bag), clusters up to 8 members per group, calls
  `LLM.summarize_memories`, and commits the consolidated memory via
  the existing `db::consolidate` transaction. Source memories are
  archived, not lost.
- **Autonomous forgetting of superseded memories** — when a memory's
  `metadata.confirmed_contradictions` points at a newer, equal- or
  higher-confidence memory, the curator archives the stale one.
  Confidence + freshness BOTH required — never forgets on detection
  alone.
- **Priority feedback** — memories with `access_count ≥ 10` and a
  recall in the last 7 days get priority +1 (cap 10); memories cold
  for 30+ days drop priority -1 (floor 1). Arithmetic only; no LLM.
- **Rollback log** — every autonomous action (consolidate, forget,
  priority-adjust) writes a `RollbackEntry` memory into
  `_curator/rollback/<ts>` carrying the pre-action snapshot. Reversible
  via `ai-memory curator --rollback <id>` or `--rollback-last N`.
  Once reversed, the log memory is tagged `_reversed` — the history
  itself is preserved as an audit trail.
- **Self-report** — at the end of every cycle the curator writes its
  own `CuratorReport` as a memory in `_curator/reports/<ts>`. Agents
  can recall "what did the curator do yesterday" using the ordinary
  `memory_recall` path.

### Testing — end-to-end autonomy coverage

- `AutonomyLlm` trait introduced as the narrow LLM surface the passes
  need; `OllamaClient` impls it in prod, `StubLlm` stubs it in tests.
- 10 unit tests in `src/autonomy.rs` including a full
  `full_autonomy_cycle_end_to_end` that seeds duplicates + a
  superseded pair, runs `run_autonomy_passes`, and asserts that
  clusters were formed, memories forgotten, rollback entries written,
  and the rollback-log namespace populated.
- `reverse_consolidation_restores_originals` verifies the undo path
  by consolidating two memories, rolling back, and asserting both
  originals are back and the merged memory is gone.

### Honest-claim note

v0.6.1 earns the **"fully-autonomous curator loop"** claim: the
system can tag, consolidate, forget, rebalance priority, report on
itself, and reverse any of its own actions — without human input.
It does **not** yet claim multi-agent autonomy across a federation
(that's Track C) or cross-backend autonomy (that's Track B).
"100% autonomous" without those caveats would still be overclaiming.

### Added — cross-backend migration (Track B PR 2)

- **`ai-memory migrate --from <url> --to <url>`** CLI subcommand,
  gated behind `--features sal`. Supported URL shapes:
  - `sqlite:///absolute/path.db` / `sqlite://./relative.db` → `SqliteStore`
  - `postgres://user:pass@host:port/db` → `PostgresStore`
    (only under `--features sal-postgres`)
- Reads pages via `MemoryStore::list`, writes via `MemoryStore::store`.
  **Idempotent on re-run** — source ids are preserved verbatim and
  both adapters upsert on id.
- `--batch N` (1..10 000, default 1000), `--namespace <ns>` filter,
  `--dry-run`, `--json` for machine-readable reports.
- **6 unit tests**: sqlite URL parsing, unknown-scheme rejection,
  sqlite→sqlite full-roundtrip, dry-run writes nothing, idempotent
  re-run, namespace filter.
- Pagination strategy: slides `until` window backwards with dedup by
  id — handles identical `created_at` timestamps that break naïve
  `since`-cursor paging on SQLite.

### What's still out of scope for v0.7-alpha

Explicitly deferred to v0.7.1 (noted in `src/migrate.rs` docblock):

- **Daemon-level adapter selection** (`ai-memory serve --store-url
  postgres://…`) — requires refactoring `handlers.rs` from
  `crate::db::` free functions to dispatch through
  `Box<dyn MemoryStore>`. That's a big change and belongs in its
  own PR.
- **Live dual-write** — reverse migration (pg → sqlite) works using
  the same command but there is no always-on replication between
  heterogenous backends yet.
- **Schema rewriting** — both adapters currently agree on the
  `Memory` shape so no field mapping is needed.

### Cross-backend-autonomy claim now earned

v0.7-alpha earns: **"one-shot migration between SQLite and
Postgres/pgvector, bidirectional, idempotent"**.

Still honest caveats:
- A production deployment running `ai-memory serve` against Postgres
  as the live store needs v0.7.1's adapter-selection refactor.
- The migration is file-level point-in-time. For zero-downtime cutover
  you still need to stop writes on the source, migrate, and restart
  against the destination — documented in the module docblock.

### Added — federation autonomy (Track C PR 2)

- **Quorum writes wired into the HTTP daemon** (`src/federation.rs`).
  `ai-memory serve --quorum-writes N --quorum-peers <url,url,…>` fans
  out every successful write to each peer's `/api/v1/sync/push` and
  returns OK only after the local commit + `W - 1` peer acks land
  within `--quorum-timeout-ms`. Insufficient acks → `503` with body
  `{"error":"quorum_not_met","got":X,"needed":Y,"reason":…}` and
  `Retry-After: 2`. Local write is **not** rolled back on quorum
  failure — the sync-daemon's eventual-consistency loop catches
  stragglers up (per ADR-0001 § Model).
- **Opt-in + default-off** — daemons without `--quorum-writes`
  behave byte-for-byte identical to v0.6.0. Zero impact on
  non-federated deployments.
- **Optional mTLS for federation traffic** — `--quorum-client-cert`
  + `--quorum-client-key` feed the outbound reqwest client an mTLS
  identity so peer acks can be authenticated end-to-end.
- **Chaos harness** — `packaging/chaos/run-chaos.sh` spawns a
  three-node local fixture, issues a configurable burst of writes,
  and injects one of four fault classes (`kill_primary_mid_write`,
  `partition_minority`, `drop_random_acks`, `clock_skew_peer`).
  Emits a JSONL convergence-bound report per cycle — the data
  shape ADR-0001 commits to publishing instead of a loss probability.

### Testing

- **7 async mock-peer integration tests** in `src/federation.rs`
  using real ephemeral-port axum servers.
- Full suite on default features: 289 unit + 158 integration tests
  still green. fmt + clippy pedantic green.

### Added — LadybugDB roadmap

- **`docs/ROADMAP-ladybug.md`** — authoritative plan for integrating
  LadybugDB (the `lbug` Rust crate) as a new `MemoryStore` SAL
  adapter alongside `SqliteStore` and `PostgresStore`. Deliberately
  **not** a 100% transition — the document explains why (AI-agnostic
  value prop, SAL trait is the right seam, ~4000 LOC rewrite is
  wrong shape). Phased plan: scaffold → migration tool support →
  benchmark matrix → promotion decision gated on 6 hard
  prerequisites. Maintenance posture (pinned SHA, monthly rebase,
  upstream-first policy, scrap criteria) informed by the TurboQuant
  scrap. Not shipping in v0.6.0.0; v0.7.1+ track.

### Added — Ollama KV-cache tuning runbook

- **`docs/RUNBOOK-ollama-kv-tuning.md`** — operator-facing runbook
  for enabling `OLLAMA_KV_CACHE_TYPE=q4_0` + `OLLAMA_FLASH_ATTENTION=1`
  on Ollama. Delivers 2–4× KV-cache memory reduction on every
  ai-memory LLM path with near-lossless quality. Zero ai-memory
  code changes.

### "100% autonomous AI" claim earned

Shipping together in v0.6.0.0:

- Autonomous curator loop (tag / consolidate / forget / priority /
  rollback / self-report) per Track A + A-2.
- Multi-agent federation with W-of-N quorum writes per Track C + C-2.
- Cross-backend portability (SQLite ↔ Postgres+pgvector) per Track
  B + B-2.
- Autonomous hooks firing on every successful `memory_store`.

Remaining caveats (documented in runbooks, not overclaims):

- Real chaos campaigns against a production-shaped deployment:
  `docs/RUNBOOK-chaos-campaign.md`.
- Week-long curator soak against a production corpus:
  `docs/RUNBOOK-curator-soak.md`.
- Daemon-level adapter selection (`serve --store-url postgres://…`):
  `docs/RUNBOOK-adapter-selection.md` — v0.7.1 follow-up.
- Attested `sender_agent_id` from mTLS cert identity — v0.7 Layer
  2b primitives shipped (#285); handler wiring follow-up.

## [0.6.0] — 2026-04-19 — Phase 1 complete + v0.6.0.0 sprint

Phase 1 baseline (Tasks 1.1–1.12 from alpha train) plus the v0.6.0.0 sprint
additions covering opt-in LLM autonomy hooks, decay-aware recall, multi-agent
messaging primitives, at-rest encryption, ops surfaces, and SDK scaffolds.

Defer-outs from this release (not shipped in 0.6.0):

- **Autonomous curator daemon** — continuous background consolidation / GC
  driven by LLM decisions. Deferred to v0.6.1. v0.6.0 ships only the
  opt-in post-store hooks (synchronous, store path only).
- **Multi-node replication + chaos testing** — durability claims beyond
  single-node VACUUM INTO snapshots + optional peer sync are out of scope
  for v0.6.0. No loss-probability target is published.
- **Storage abstraction layer (Postgres / pgvector adapter)** — remains a
  v0.7 track. v0.6.0 is SQLite-only; the SAL preview on `feat/sal-trait-redesign`
  stays private/feature-gated until v0.7 extraction.

### Added — v0.6.0.0 sprint (autonomy hooks + multi-agent + at-rest + ops + SDKs)

**Autonomy / recall**
- **Time-decay half-life on recall scoring** — per-tier exponential decay
  multiplier on the hybrid-recall score blend. Default half-lives: short
  7 d, mid 30 d, long 365 d. Configurable via `[scoring]` in `config.toml`;
  `legacy_scoring = true` disables decay for A/B comparison and regression
  rollback. Half-lives clamped to `[0.1, 36500]` days.
- **Contextual recall (conversation-token bias)** — `memory_recall` accepts
  an optional `context_tokens: array<string>`. When supplied, the primary
  query embedding is fused 70/30 with an embedding of the joined context
  tokens, biasing recall toward memories that match both the explicit
  query AND nearby conversation topics. CLI: `--context-tokens tok1,tok2`.
- **Post-store LLM autonomy hooks** — opt-in synchronous hooks that fire
  `llm::auto_tag` + `llm::detect_contradiction` on every successful
  `memory_store`. Results persist into `metadata.auto_tags` and
  `metadata.confirmed_contradictions`. Enabled via
  `AI_MEMORY_AUTONOMOUS_HOOKS=1` env var or `autonomous_hooks = true` in
  config. Off by default (adds Ollama round-trip latency). Skipped for
  content under 50 bytes, when no LLM is wired, and for `_`-prefixed
  internal namespaces.
**Multi-agent primitives**
- **Agent-to-agent notify + inbox** — `memory_notify(target, title, payload)`
  + `memory_inbox([agent_id, unread_only])` MCP tools. Messages are
  ordinary memories in the reserved `_messages/<target>` namespace;
  sender identity stamped in metadata; `access_count == 0` is the
  conventional unread marker. No new schema.
- **Webhook subscribe / unsubscribe / list** — `memory_subscribe` +
  `memory_unsubscribe` + `memory_list_subscriptions` MCP tools. Events
  fire on `memory_store` (v0.6.1 extends to delete/promote/link) and
  POST an HMAC-SHA256-signed JSON payload to subscriber URLs
  (`X-Ai-Memory-Signature: sha256=<hex>`). SSRF-hardened — private-range
  IPs rejected, https required for non-loopback hosts. Migration v13
  adds the `subscriptions` table.
**At-rest encryption**
- **Optional SQLCipher encryption at rest** — new cargo feature
  `sqlcipher` swaps `rusqlite` to the
  `bundled-sqlcipher-vendored-openssl` feature. Default builds are
  byte-for-byte unchanged. Operators who want encryption build with
  `cargo build --no-default-features --features sqlcipher` and supply
  `--db-passphrase-file <path>` at startup. Passphrase never appears
  in the process list or shell history.

**Ops**
- **Prometheus `/metrics` endpoint** (and `/api/v1/metrics`) exposes
  `ai_memory_store_total`, `ai_memory_recall_total`,
  `ai_memory_recall_latency_seconds`, `ai_memory_autonomy_hook_total`,
  `ai_memory_contradiction_detected_total`,
  `ai_memory_webhook_dispatched_total`,
  `ai_memory_webhook_failed_total`, `ai_memory_memories`,
  `ai_memory_hnsw_size`, `ai_memory_subscriptions_active`. Pure Rust,
  no new transitive C deps.
- **Hardened systemd units** under `packaging/systemd/` —
  `ai-memory.service`, `ai-memory-sync.service`,
  `ai-memory-backup.service`, `ai-memory-backup.timer` with README.
  Full sandbox (`ProtectSystem=strict`, `MemoryDenyWriteExecute=yes`,
  `SystemCallFilter=@system-service`, `CapabilityBoundingSet=` empty,
  `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`). Target
  `systemd-analyze security` exposure score <5.0.
- **Backup / restore CLI** — `ai-memory backup --to <dir> [--keep N]`
  writes a hot-backup-safe SQLite `VACUUM INTO` snapshot plus a
  sha256 manifest. `ai-memory restore --from <path>` verifies the
  manifest before replacing the current DB; previous DB is moved
  aside to `<db>.pre-restore-<ts>.db` as a safety net. Paired with
  the hourly `ai-memory-backup.timer` systemd unit.

**SDKs**
- **TypeScript SDK scaffold** under `sdk/typescript/` —
  `@alphaone/ai-memory` (v0.6.0-alpha.0), strict TS, undici-based
  fetch, covers all current + v0.6.0.0 target endpoints (18+ methods),
  Jest tests guarded by `AI_MEMORY_TEST_DAEMON` env var. Includes
  HMAC-SHA256 webhook verifier. Not yet published to npm.
- **Python SDK scaffold** under `sdk/python/` — `ai-memory`
  (v0.6.0-alpha.0), sync (`AiMemoryClient`) + async
  (`AsyncAiMemoryClient`) clients via `httpx`, Pydantic v2 models
  (15/15 Memory fields), exception hierarchy, HMAC-SHA256 webhook
  verifier. Not yet published to PyPI.

### v0.6.0 GA disclosures (unchanged from pre-sprint baseline)

The following items are **MANDATORY DISCLOSURES** for the v0.6.0 release.
Operators upgrading from v0.5.4.x MUST read this section before deploying.

The following items are **MANDATORY DISCLOSURES** for the v0.6.0 GA release.
Operators upgrading from v0.5.4.x MUST read this section before deploying.

### Breaking changes

- **Consensus governance now requires agent pre-registration** (issue #234).
  The fix for security issue #216 (one caller satisfying `Consensus(N)` with
  N spoofed agent_ids) added an `is_registered_agent()` gate. Existing
  `consensus:N` policies become **indefinitely-locked** unless approver
  agents are registered first via `ai-memory agents register --agent-id <id>
  --agent-type <type>`.

  Migration: register all consensus approvers before upgrading. Example:

  ```bash
  ai-memory agents register --agent-id alice --agent-type human
  ai-memory agents register --agent-id bob   --agent-type human
  ai-memory agents register --agent-id carol --agent-type human
  ```

### Security disclosures (peer-mesh sync)

- **Sync endpoints are unauthenticated when TLS is not enabled** (issue #231).
  `POST /api/v1/sync/push` and `GET /api/v1/sync/since` accept all callers
  when `serve` runs without `--tls-cert + --tls-key`. Production peer-mesh
  deployments **MUST** set `--tls-cert + --tls-key + --mtls-allowlist`.
  See `docs/ADMIN_GUIDE.md` § Peer-mesh security.

- **sync-daemon does no server-cert verification without --client-cert**
  (issue #232). The daemon uses `danger_accept_invalid_certs(true)` when
  `--client-cert` is not provided — any server cert is accepted. For
  untrusted networks, ALWAYS use mTLS in both directions.

- **Any valid mTLS peer can dump the full database** (issue #239). By design,
  the trust boundary is the mTLS cert. Sync endpoints bypass per-memory
  visibility filtering. **Allowlist only peers you fully trust.** Per-namespace
  / per-scope sync filtering is a Phase 5 feature.

- **Body-claimed `sender_agent_id` is not yet attested to the cert CN/SAN**
  (issue #238). mTLS gates network access but the receiving handler accepts
  `sender_agent_id` from the body without checking the cert identity. A peer
  with a valid cert can claim any agent_id. Tracked as Layer 2b for v0.7.

### Schema migration

- v0.5.4.6 → v0.6.0 runs six additive migrations (v7 through v12). All are
  idempotent, transactional, and default-safe. Worst-case lock on a 10M-row
  database: 1–3 seconds during v10 (scope_idx index build). Schedule a brief
  maintenance window for large databases.

### Surface gaps tracked for v0.6.1

- Namespace standards / governance config is currently **MCP-only** (issue
  #236). HTTP and CLI surfaces will land in v0.6.1.
- `--agent-type` accepts only 6 hardcoded values (issue #235). Workaround:
  use `system` for custom agents, or wait for v0.6.1.

## [0.6.0-alpha.2] — 2026-04-16 — Phase 1 Track A complete + release-plumbing reconciliation

Supersedes **0.6.0-alpha.1** (2026-04-16, same day — partial publish). alpha.1
shipped the Task 1.3 feature to crates.io, Ubuntu PPA, Homebrew, and GitHub
Release binaries, but Docker (GHCR) and Fedora COPR failed due to a pre-existing
divergence between `main` and `release/v0.6.0`:

- Dockerfile pinned to `rust:1.87-slim` while code uses let-chains stabilized in
  1.88 (fixed on main in #187, never back-merged)
- Fedora COPR workflow `sed` blindly injected SemVer pre-release strings into
  RPM `Version:` field, which forbids `-`

alpha.2 back-merges `main` → `release/v0.6.0` (commits from `ce8fd47` through
`36747b2`, including RUSTSEC-2026-0098/0099 fixes), bumps `rust-version` to 1.88
(the honest MSRV), updates `time` 0.3.45 → 0.3.47 (RUSTSEC-2026-0009 DoS), and
patches the COPR workflow to split SemVer pre-release versions into `Version:` +
`Release:` pairs per Fedora packaging guidelines. No feature changes vs alpha.1.

alpha.1 will be **yanked from crates.io** once alpha.2 publishes successfully.

## [0.6.0-alpha.1] — 2026-04-16 — Phase 1 Track A complete (PARTIAL — yanked, superseded by alpha.2)

First cut of the v0.6.0 release train. Integration branch for Phase 1 tasks 1.3–1.12
plus the already-landed foundation work (1.1, 1.2). Pre-release; API is not yet stable.
Successive alphas will be tagged at each track completion (A/B/C/D per
[docs/PHASE-1.md](docs/PHASE-1.md) §Dependency Graph).

### Added — Task 1.1 (schema metadata foundation)

- **`metadata` JSON column** on `memories` and `archived_memories` tables, default `'{}'`.
  Schema migration to v7. All CRUD paths preserve metadata.
- **`Memory.metadata: serde_json::Value`** field with serde defaults.
- **`CreateMemory.metadata`**, **`UpdateMemory.metadata`** — MCP, HTTP, and CLI all accept
  arbitrary JSON metadata on store/update.
- **TOON format** renders `metadata` column inline.

### Added — Task 1.2 (Agent Identity in Metadata, NHI-hardened) — [#193]

- **`metadata.agent_id`** on every stored memory, resolved via a defense-in-depth
  precedence chain (explicit flag / body / MCP param → `AI_MEMORY_AGENT_ID` env →
  MCP `initialize.clientInfo.name` → `host:<host>:pid-<pid>-<uuid8>` →
  `anonymous:pid-<pid>-<uuid8>`).
- **HTTP `X-Agent-Id` request header** honored when no body `agent_id` is supplied;
  per-request `anonymous:req-<uuid8>` synthesized otherwise, with `WARN` log line.
- **`--agent-id` global CLI flag** (also reads `AI_MEMORY_AGENT_ID` env var).
- **`--agent-id` filter** on `list` and `search` (CLI, MCP tool param, HTTP query param).
- **Immutability**: `metadata.agent_id` is preserved across UPDATE, UPSERT dedup,
  import, sync, consolidate, and MCP `memory_update`. Enforced at both SQL level
  (`json_set` CASE clauses in `db::insert` and `db::insert_if_newer`) and caller
  level (`identity::preserve_agent_id` in every path that writes metadata).
- **Validation**: `^[A-Za-z0-9_\-:@./]{1,128}$` — permits prefixed / scoped / SPIFFE
  forms, rejects whitespace, null bytes, control chars, shell metacharacters.
- **New module** `src/identity.rs` (17 unit tests): precedence chain, process
  discriminator (`OnceLock<pid-<pid>-<uuid8>>`), component sanitization, HTTP
  resolution, provenance preservation.
- **`gethostname = "0.5"`** added as dependency (minimal, no transitive deps).
- **28 new tests** (20+ beyond spec minimum of 4): 17 unit + 2 validator + 9 integration.

### Security — red-team findings fixed during Task 1.2 review

- **T-3 (HIGH)**: MCP `memory_update` could rewrite `metadata.agent_id` on an existing
  memory, bypassing the documented immutability invariant. Fixed in commit `b228dcc`
  by wiring `identity::preserve_agent_id` into `handle_update`. Regression test
  `test_mcp_update_preserves_agent_id`.
- **GAP 1 (HIGH)**: `cmd_import` blindly trusted `metadata.agent_id` in input JSON,
  allowing an attacker-crafted file to forge any agent identity. Fixed in `356b448`:
  restamps with caller's id by default; `--trust-source` flag opts into legitimate
  backup-restore; original claim preserved as `imported_from_agent_id`. `cmd_sync`
  gets the same treatment on `pull` and `merge` paths.
- **GAP 2 (MEDIUM)**: `db::consolidate` merged source metadata with last-write-wins
  semantics on `agent_id`, nondeterministically dropping attribution and giving the
  consolidator no record. Fixed in `356b448`: consolidator's id is authoritative;
  all source authors preserved in `metadata.consolidated_from_agents` array.
  HTTP `ConsolidateBody` gains optional `agent_id` field plus `X-Agent-Id` header.
- **GAP 3 (LOW)**: `cmd_mine` produced memories with empty metadata, orphaning them
  from every agent_id filter. Fixed in `356b448`: caller's `agent_id` +
  `mined_from` source tag injected into every mined memory.
- **Defense-in-depth**: `db::insert_if_newer` (sync `merge` path) gains the same
  SQL-level `json_set` preservation clause as `db::insert`.

### Documentation — Phase 1.5 governance — [#194]

- **Governance §2.1 + §2.1.1**: new `Supervised off-host agents` approved class with
  7 binding pre-conditions (heartbeat, dead-man's switch, rate limit, lock-aware
  operation, instance-disambiguating attribution, etc.).
- **Governance §3.4.3.1**: concurrency lock primitive (short-tier `ai-memory` entry
  as lock, 15-min TTL, race-loser-yields semantics, stale-lock human escalation).
- **Governance §3.4.4.1 / §3.4.4.2**: audit-memory retention policy (immutable,
  non-consolidatable, append-only) + volume control at scale.
- **Governance new §3.5** (7 sub-sections): multi-agent coordination — branch
  ownership, handoff procedure, stale-branch GC, inter-agent conflict resolution,
  §3.4 SOP serialization, humans-in-CLI vs supervised off-host coordination,
  single-agent operation default.
- **Governance §5.4**: sole-approver policy applies uniformly to every approved
  agent class.
- **Workflow §8.5.1**: multi-agent operation cross-reference + lock acquisition
  discipline.

### Added — Task 1.3 (Agent Registration)

- **`_agents` reserved namespace** holding one long-tier memory per registered
  agent (`title = "agent:<agent_id>"`, `metadata.agent_type` +
  `metadata.capabilities` + `metadata.registered_at` + `metadata.last_seen_at`).
- **MCP tools**: `memory_agent_register`, `memory_agent_list` (brings tool count
  to **28**).
- **HTTP endpoints**: `POST /api/v1/agents`, `GET /api/v1/agents` (brings
  endpoint count to **26**).
- **CLI**: `ai-memory agents register --agent-id … --agent-type … [--capabilities …]`
  and `ai-memory agents list` (default sub-command).
- **`VALID_AGENT_TYPES`** closed set: `ai:claude-opus-4.6`, `ai:claude-opus-4.7`,
  `ai:codex-5.4`, `ai:grok-4.2`, `human`, `system`. Enforced by
  `validate_agent_type`.
- **Re-registration semantics**: upsert refreshes `agent_type`, `capabilities`,
  `last_seen_at`; preserves `registered_at` and `metadata.agent_id`
  (rides existing immutability SQL clause).
- **Trust model unchanged**: `agent_id` is still *claimed, not attested*. Future
  work will pair registration with provable attestation.
- **6 new integration tests**: register+list, duplicate-preserves-registered-at,
  invalid-type-rejected, invalid-id-rejected, namespace-isolation (no leak into
  `global`), and raw MCP JSON-RPC register/list roundtrip.

### Pending — remaining Phase 1 tasks to land in this release train

- Task 1.4 — Hierarchical Namespace Paths — depends on 1.1 ✓
- Task 1.5 — Visibility Rules — depends on 1.4
- Task 1.6 — N-Level Rule Inheritance — depends on 1.4
- Task 1.7 — Vertical Promotion — depends on 1.4
- Task 1.8 — Governance Metadata — depends on 1.1 ✓
- Task 1.9 — Governance Roles — depends on 1.8
- Task 1.10 — Approval Workflow — depends on 1.9
- Task 1.11 — Budget-Aware Recall — depends on 1.1 ✓
- Task 1.12 — Hierarchy-Aware Recall — depends on 1.4 + 1.11

### Release engineering

- Branched from `develop` @ `ee6cf9a` on 2026-04-16; all Phase 1 work now lands on `release/v0.6.0`.
- Successive alphas (`v0.6.0-alpha.N`) tagged at each track completion; `v0.6.0-rc.1`
  at feature-complete; `v0.6.0` GA when Phase 1 is done and external review window
  closes.
- `main` remains frozen at v0.5.4-patch.6 until v0.6.0 GA — no more 0.5.4 patches.

## [0.5.4-patch.4] — 2026-04-13

### Added

- **Three-level rule layering**: global (`*`) + parent + namespace standards, auto-prepended to recall and session_start. Max depth 5, cycle-safe.
- **Cross-namespace standards**: A standard memory from any namespace can be set as the standard for any other namespace. One policy, many projects.
- **Auto-detect parent by `-` prefix**: `set_standard("ai-memory-tests", id)` auto-discovers `ai-memory` as parent if it has a standard set. No explicit `parent` parameter needed.
- **Filesystem path awareness**: On `session_start`, walks from cwd up to home directory, checks if parent directory names have namespace standards, auto-registers parent chain. OS-agnostic via `PathBuf` and `dirs` crate.
- **`parent` parameter on `memory_namespace_set_standard`**: Explicit parent declaration for rule layering.
- Schema migration v6: `parent_namespace` column on `namespace_meta`

### Changed

- `inject_namespace_standard` resolves full parent chain: global → grandparent → parent → namespace
- Response returns `"standard"` (1 level) or `"standards"` array (multiple levels)
- TOON format: `standards[id|title|content]:` section renders all levels

## [0.5.4-patch.3] — 2026-04-12

### Added

- **Namespace standards**: 3 new MCP tools (`memory_namespace_set_standard`, `memory_namespace_get_standard`, `memory_namespace_clear_standard`) — 26 MCP tools total. Set a memory as the enforced standard/policy for a namespace; auto-prepended to recall and session_start results when scoped to that namespace.
- **Auto-prepend**: `handle_recall` and `handle_session_start` automatically prepend the namespace standard as a separate `"standard"` field when namespace is specified. Deduplicated from results. Count excludes standard.
- **TOON standard section**: TOON format renders namespace standard as a separate `standard[id|title|content]` section before memories.
- Schema migration v5: `namespace_meta` table
- 2 new integration tests: `test_mcp_namespace_standard_auto_prepend`, `test_namespace_standard_cascade_on_delete`

### Fixed

- **Shell `validate_id()` gap**: Interactive REPL `get` and `delete` commands now call `validate_id()`.
- **HNSW stale entry on dedup update**: `handle_store` dedup path now calls `idx.remove()` before `idx.insert()`.
- **Cascade cleanup**: `db::delete` removes `namespace_meta` rows referencing the deleted memory. `db::gc` cleans orphaned `namespace_meta` rows after expiring memories.
- **Consolidate warning**: `handle_consolidate` warns if any source memory is a namespace standard, prompting re-set to the new consolidated memory ID.

## [0.5.4-patch.2] — 2026-04-12

### Fixed

- **Tier downgrade protection**: `update()` now rejects tier downgrades (long→mid, long→short, mid→short) with a clear error message; prevents accidental data loss from TTL being added to permanent memories
- **Embedding regeneration on content update**: MCP `memory_update` now regenerates embedding vector and updates HNSW index when title or content changes, preventing stale semantic recall results
- **Consolidated memory embedding**: MCP `memory_consolidate` now generates embedding for the new consolidated memory at creation time and removes old entries from HNSW index, instead of relying on backfill
- **Self-contradiction exclusion**: CLI and MCP store now exclude the actual memory ID from `potential_contradictions` on upsert, fixing cosmetic self-referencing bug
- **Atomic CLI promote**: Removed non-atomic raw SQL `UPDATE` in `cmd_promote`; `db::update()` with `Some("")` already clears `expires_at` correctly
- **MCP `validate_id()` defense-in-depth**: Added `validate_id()` to `handle_get`, `handle_update`, `handle_delete`, `handle_promote`, `handle_get_links`, `handle_archive_restore`, `handle_auto_tag`, `handle_detect_contradiction`
- **CLI `validate_id()` defense-in-depth**: Added `validate_id()` to `cmd_get`, `cmd_update`, `cmd_delete`, `cmd_promote`

### Added

- `Tier::rank()` method for numeric tier comparison (Short=0, Mid=1, Long=2)
- 5 new unit tests: `tier_rank_ordering`, `update_rejects_tier_downgrade_long_to_short`, `update_rejects_tier_downgrade_long_to_mid`, `update_allows_tier_upgrade_short_to_long`, `update_allows_same_tier`
- 6 new integration tests: `test_cli_validate_id_rejects_invalid`, `test_tier_downgrade_rejected`, `test_tier_upgrade_allowed`, `test_duplicate_title_no_self_contradiction`, `test_promote_clears_expires_at`, `test_version_flag_patch2`

### Test Coverage

| Metric | Count |
|--------|-------|
| Unit tests | 139 |
| Integration tests | 49 |
| **Total** | **188** |
| Modules with tests | 15/15 |

## [0.5.4-patch.1] — 2026-04-12

### Fixed

- `--version` / `-V` flag missing — added `version` to `#[command]` attribute
- CLI `update` rejected past `expires_at` — changed to format-only validation, matching MCP behavior
- `archive_restore` tier promotion — release binary now includes `'long'` hardcoded in INSERT SQL

## [0.5.4] — 2026-04-12

### Added

- **Configurable TTL per tier**: `[ttl]` section in config.toml with 5 overrides: `short_ttl_secs`, `mid_ttl_secs`, `long_ttl_secs`, `short_extend_secs`, `mid_extend_secs`. Set to 0 to disable expiry.
- **Archive before GC deletion**: Expired memories archived to `archived_memories` table before deletion (default: `true`). Configurable via `archive_on_gc` in config.toml.
- 4 new MCP tools: `memory_archive_list`, `memory_archive_restore`, `memory_archive_purge`, `memory_archive_stats` (21 total)
- 4 new HTTP endpoints: `GET/DELETE /api/v1/archive`, `POST /api/v1/archive/{id}/restore`, `GET /api/v1/archive/stats` (24 total)
- `archive` CLI subcommand with `list`, `restore`, `purge`, `stats` actions (26 total commands)
- Schema migration v4: `archived_memories` table with indexes
- `TtlConfig` and `ResolvedTtl` types in config.rs for type-safe TTL resolution
- TTL values clamped to 10-year maximum to prevent integer overflow
- Negative `older_than_days` rejected in archive purge
- Archive restore checks for active ID collision (prevents silent overwrite)
- `validate_id()` on all archive restore endpoints (HTTP, MCP, CLI)

### Changed

- `db::update()` returns `(bool, bool)` — `(found, content_changed)` — for embedding regeneration
- `db::touch()` accepts configurable `short_extend` / `mid_extend` parameters
- `db::gc()` accepts `archive: bool` parameter
- `db::recall()` and `db::recall_hybrid()` accept configurable extend values
- All `gc_if_needed` callers respect `archive_on_gc` config setting
- Update facility: tier downgrade protection, title collision detection, embedding regeneration on content change

### Fixed

- Embeddings not regenerated on content update via `memory_update` (MCP + dedup store path)
- Tier downgrade not protected in update path (long never downgrades, mid never to short)
- Title+namespace collision on update returned opaque error (now returns 409 CONFLICT)
- MCP and CLI update handlers missing `validate_id()` call
- Negative TTL extension values now clamped to 0

## [0.5.2] — 2026-04-08

### Added

- Ubuntu PPA: `sudo add-apt-repository ppa:jbridger2021/ppa && sudo apt install ai-memory`
- Fedora COPR: `sudo dnf copr enable alpha-one-ai/ai-memory && sudo dnf install ai-memory`
- CI workflows for automated PPA and COPR uploads on tag push
- debian/ packaging directory (control, rules, changelog, copyright)
- RPM spec file (ai-memory.spec) for COPR builds
- OpenClaw as 9th supported AI platform across all docs
- Animated architecture SVG and benchmark SVG in README
- Fedora/RHEL COPR and Ubuntu PPA install cards on GitHub Pages (8 install methods)

### Changed

- GitHub Pages professionalized: condensed hero, 13→7 nav links, 7→4 stats
- Install method count updated to 8 across all docs

## [0.5.1] — 2026-04-08

### Added

- Docker image auto-published to GitHub Container Registry (ghcr.io) on tag push
- `server.json` manifest for Official MCP Registry (modelcontextprotocol/registry)
- CONTRIBUTING.md, CHANGELOG.md, CODE_OF_CONDUCT.md
- Open Graph and Twitter Card meta tags on GitHub Pages
- Scope tables for all 9 AI platform tabs on GitHub Pages
- `mine` command documented across all docs (USER_GUIDE, ADMIN_GUIDE, DEVELOPER_GUIDE, index.html)
- Error code reference in DEVELOPER_GUIDE (NOT_FOUND, VALIDATION_FAILED, DATABASE_ERROR, CONFLICT)
- config.toml reference section in ADMIN_GUIDE
- Store command flags (`--source`, `--expires-at`, `--ttl-secs`) documented in README

### Changed

- Dockerfile: Rust 1.82 → 1.86, added build-essential, added benches/ copy
- Dockerfile: version label 0.4.0 → 0.5.0
- CI workflow: added Docker (GHCR) job triggered on tag push
- Claude Code MCP config: corrected from `~/.claude/.mcp.json` to three-scope model (`~/.claude.json`, `.mcp.json`, project-local)
- All 8 AI platform configs: added Windows paths, env var syntax, scope tables
- Hybrid recall blend weights: corrected docs from 50/50 & 85/15 to 60/40 (matches code)
- Default tier: corrected docs from "keyword" to "semantic" (matches code)
- Test count: corrected from 167 to 161 (118 unit + 43 integration)
- Module count: corrected from 14 to 15 (added mine.rs)
- CLI command count: corrected from 24 to 25 (added mine)

### Fixed

- Dockerfile build failure: missing benches/ directory, outdated Rust version, missing C++ compiler

## [0.5.0] — 2026-04-08

### Added

- MCP server with 17 tools for AI-native memory management
- HTTP API with 20 endpoints for external integration
- CLI with 25 commands for local operation and scripting
- 4 feature tiers (Core, Standard, Advanced, Enterprise) for flexible deployment
- TOON format for structured, topology-aware memory representation
- Hybrid recall engine combining semantic search, keyword matching, and graph traversal
- Multi-node sync for distributed memory across instances
- Auto-consolidation to merge and deduplicate related memories
- `mine` command for importing memories from conversation history
- LongMemEval benchmark support achieving 97.8% Recall@5

### Changed

- Upgraded memory storage layer for improved write throughput
- Refined relevance scoring in hybrid recall for better precision
- Improved CLI output formatting and error messages

### Fixed

- Resolved race condition during concurrent memory writes
- Fixed encoding issue with non-ASCII content in TOON format
- Corrected sync conflict resolution when timestamps are identical

## [0.4.0]

### Added

- Initial MCP server implementation with core tool set
- Basic memory storage and retrieval
- CLI foundation with essential commands
- Semantic search over stored memories
- SQLite-backed persistent storage

### Changed

- Migrated internal data model to support richer metadata

### Fixed

- Fixed crash on empty query input
- Resolved file descriptor leak in long-running server mode

## [0.3.0]

### Added

- Embedding-based semantic search
- Memory tagging and filtering
- Configuration file support

### Changed

- Switched to async I/O for server operations

### Fixed

- Fixed memory leak during large batch imports

## [0.2.0]

### Added

- Persistent storage backend
- Basic CLI for memory CRUD operations
- JSON export and import

### Fixed

- Fixed incorrect timestamp handling across time zones

## [0.1.0]

### Added

- Initial prototype with in-memory storage
- Core data model for memory entries
- Basic search functionality

[0.5.2]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alphaonedev/ai-memory-mcp/releases/tag/v0.1.0
