# Roadmap — LadybugDB as a SAL adapter (v0.7.1+)

Status: **roadmap** (not shipping in v0.6.0.0).
Date: 2026-04-19
Depends on: SAL trait (#279), `MemoryStore` adapter pattern, migration
tool (#283).

This document is the authoritative plan for integrating
[LadybugDB](https://ladybugdb.com/) into ai-memory. It is
deliberately **not** a 100% transition plan — see §
[Why this is NOT a 100% transition](#why-this-is-not-a-100-transition)
for the architectural reasoning.

## TL;DR

- LadybugDB (the `lbug` Rust crate) ships as a new
  [`MemoryStore`](#sal-contract) adapter: `LadybugStore`, behind
  `--features sal-ladybug`.
- `SqliteStore` remains the default. `PostgresStore` remains the
  scale-out option. `LadybugStore` is the graph-first + hybrid-search
  option.
- Users opt in per deployment via `ai-memory serve --store-url
  ladybug:///path` (v0.7.1 adapter-selection, tracked in
  [RUNBOOK-adapter-selection.md](RUNBOOK-adapter-selection.md)).
- `ai-memory migrate --from sqlite:///… --to ladybug:///…` gives
  existing users a one-shot path in. Idempotent, same pattern as the
  existing sqlite↔postgres migration.
- Decision to promote LadybugDB to **default** for a future major
  release is contingent on the benchmark matrix in § [Phase 4](#phase-4-promotion-decision).

## Why this is NOT a 100% transition

Three reasons, stated bluntly:

1. **ai-memory's value proposition is "AI-agnostic, platform-
   agnostic."** Becoming LadybugDB-specific silently narrows the
   deployment surface. Users on resource-constrained hosts
   (Raspberry Pi, edge devices, embedded MCP clients) may not run
   LadybugDB's columnar runtime at all. SQLite runs everywhere.
   Preserve that baseline.

2. **The SAL trait shipped in #279 is exactly the correct seam** for
   swappable backends. `SqliteStore` and `PostgresStore` already
   coexist behind `MemoryStore`. Adding `LadybugStore` is an adapter,
   not a rewrite.

3. **A 100% transition is a ~4000 LOC rewrite**, not an adapter swap:

   | Subsystem | Current lock-in | Rewrite required |
   |---|---|---|
   | `src/db.rs` (3700+ LOC) | SQLite: FTS5 syntax, WAL pragmas, SQLCipher, json1, row marshalling | Full re-implementation per backend |
   | `src/hnsw.rs` + `instant-distance` | in-process HNSW | Replace with native lbug vector |
   | CLI + MCP + HTTP handlers | Dispatch through `crate::db::` free functions | Route through `dyn MemoryStore` (refactor in progress per RUNBOOK-adapter-selection.md) |
   | Tests + runbooks | Assume SQLite on disk | Re-author against lbug |
   | SQLCipher encryption at rest | Tied to rusqlite+bundled-sqlcipher | Re-solve on lbug |

   The SAL-adapter path amortises this across releases and lets us
   A/B measure before committing.

## What LadybugDB demonstrably buys

Based on the product claims (subject to validation in Phase 3
benchmarks):

| Capability | Current implementation | What `LadybugStore` would bring |
|---|---|---|
| **Graph traversal** (`memory_links`: derived_from, contradicts, supersedes, related_to) | SQL JOINs against a `memory_links` table | Native graph queries; faster at depth >2 |
| **Vector search** | HNSW via `instant-distance` (sal:SqliteStore) or pgvector (sal-postgres) | Native columnar vector index; benchmark vs HNSW |
| **Full-text search** | SQLite FTS5 (battle-tested, 15+ years) | Native FTS engine; benchmark vs FTS5 |
| **Columnar storage** | n/a (SQLite row-oriented) | Analytics over corpus (tag frequency, namespace growth, access-count distributions) |
| **Hybrid graph-vector search** | Two-stage: vector search → filter by graph relation | Single query engine |
| **Schema enforcement** | rusqlite + migrations (v13 current) | Same guarantees with graph typing |

## What LadybugDB does NOT buy

Claims worth checking against our actual workload:

- **"Zero-latency retrieval"**: SQLite is already in-process with ms-scale recall. The bottleneck on most ai-memory deployments is embedding generation (candle) or LLM round-trip (Ollama), **not** the storage layer.
- **"Brain-inspired local-first"**: ai-memory is already local-first. Sync-daemon is peer-to-peer. No cloud dependency. This is a marketing overlap, not a new capability.
- **"Eliminates external infrastructure"**: SQLite has zero external infrastructure. Postgres has some (the `sal-postgres` track). LadybugDB would match SQLite's posture.

## Phased plan

### Phase 0 — Foundation (shipped)

- `MemoryStore` trait + `SqliteStore` + `PostgresStore` adapters (#279).
- `ai-memory migrate --from <url> --to <url>` (#283).
- Adapter-selection runbook defining the v0.7.1 `serve --store-url`
  refactor ([RUNBOOK-adapter-selection.md](RUNBOOK-adapter-selection.md)).

### Phase 1 — `LadybugStore` scaffold (v0.7.1-alpha)

- Add `lbug = { version = "…", optional = true }` to `Cargo.toml`
  behind a pinned SHA (per the turboquant-fork lessons — never
  depend on crates.io HEAD for a young library).
- New feature flag: `sal-ladybug = ["sal", "dep:lbug"]`.
- New module `src/store/ladybug.rs` implementing `MemoryStore`.
  Minimum viable surface: `store`, `get`, `list`, `search`, `delete`.
  `link`, `register_agent`, `verify`, `begin_transaction` land next.
- Unit tests — same shape as `src/store/postgres.rs::tests`:
  trait-surface tests (Capabilities bits), live integration tests
  gated on `AI_MEMORY_TEST_LADYBUG_URL` env var.
- **No migration** yet. No docker-compose fixture yet. No defaults
  change.
- **Exit criterion**: `cargo build --features sal-ladybug` works on
  the three CI targets (ubuntu, macos, windows). Unit tests green.

### Phase 2 — Migration tool support (v0.7.1-beta)

- Extend `ai-memory migrate` URL grammar:
  - `ladybug:///absolute/path` → `LadybugStore`.
  - `ladybug://./relative` supported.
- Idempotent re-run semantics preserved.
- Round-trip tests: sqlite → ladybug, ladybug → sqlite, ladybug →
  postgres, postgres → ladybug. All with namespace-filter +
  `--dry-run` variants.
- **Exit criterion**: 10,000-memory round-trip completes with
  `memories_read == memories_written` and zero errors on all four
  axes.

### Phase 3 — Benchmark matrix (v0.7.1-rc)

The **authoritative decision data** for whether LadybugDB earns a
default-backend promotion. Published as `docs/BACKEND-COMPARISON.md`
with raw CSVs attached.

| Dimension | SQLite (default) | Postgres+pgvector | LadybugDB |
|---|---|---|---|
| Write throughput (memories/sec, single-threaded) | measure | measure | measure |
| Write throughput (8 concurrent writers) | measure | measure | measure |
| Hybrid recall latency p50 / p95 / p99 at 100k corpus | measure | measure | measure |
| Hybrid recall latency at 1M corpus | measure | measure | measure |
| Graph traversal (2-hop `derived_from`) at 100k | measure | measure | measure |
| Graph traversal (3-hop) at 1M | measure | measure | measure |
| FTS query latency p50 at 100k | measure | measure | measure |
| Storage footprint per 1k memories | measure | measure | measure |
| Memory (RSS) under idle | measure | measure | measure |
| Memory (RSS) under autonomous curator load | measure | measure | measure |
| Cold-start time | measure | measure | measure |
| Operational complexity (lines in ADMIN_GUIDE to deploy) | 1 line | ~15 lines (pg setup) | measure |

**Methodology**:

- Fixture: OpenAI-3072-dim embeddings over 1M representative memories
  (same DBpedia sample the TurboQuant paper used).
- Hardware: DigitalOcean `s-4vcpu-16gb` droplet for the main bench;
  Raspberry Pi 5 (8 GB) for the edge-device comparison.
- Each measurement repeated 10× with cold + warm caches.
- Reports raw CSVs + summary tables + `systemd-analyze security`
  scores for each backend's daemon unit.

**Exit criterion**: `docs/BACKEND-COMPARISON.md` merged to
`release/v0.7.1` with every cell populated. No single-backend
recommendation yet — data is the deliverable.

### Phase 4 — Promotion decision

This phase answers: **should LadybugDB become the default backend
in a future major release?**

**Hard prerequisites** (every one must be true; measured from Phase 3
data):

1. **Write throughput**: LadybugDB ≥ SQLite within 20% on
   single-threaded writes. (Autonomous curator writes serially.)
2. **Hybrid recall p99 latency**: LadybugDB ≤ SQLite+HNSW at 1M
   corpus.
3. **Graph traversal 3-hop at 1M**: LadybugDB ≥ 3× SQLite JOIN chain.
4. **Storage footprint**: LadybugDB ≤ 1.5× SQLite for the same
   corpus (columnar should compress, not expand).
5. **Raspberry Pi 5**: LadybugDB deploys cleanly and passes the
   single-node functional phase of `RUNBOOK-digitalocean-testing.md`.
6. **Upstream maturity**: `lbug` crate at ≥ 1.0 semver with at least
   one named-company production deployment outside ai-memory.

**If all 6 hold**: promote to default in the next major release
(e.g. v1.0 or v2.0). Keep `SqliteStore` + `PostgresStore` as
opt-out backends with continued support for at least two major
releases.

**If any fail**: LadybugDB stays a SAL adapter indefinitely.
`SqliteStore` remains default. Publish the failure data and revisit
at the next annual roadmap review.

## Open questions

Tracked for answers before Phase 1 begins:

1. **`lbug` license**: permissive (MIT / Apache-2.0) or copyleft?
   Non-permissive breaks ai-memory's Apache-2.0 downstream posture.
2. **`lbug` platform coverage**: Windows + macOS + Linux on
   x86_64 + aarch64 the floor. Is it there yet?
3. **`lbug` dep tree weight**: adds how many crates to the default
   feature set? Non-optional transitive deps matter for Raspberry Pi
   builds.
4. **Encryption at rest**: can LadybugDB carry an equivalent of
   SQLCipher's PRAGMA-key approach? If not, we'd need a separate
   at-rest encryption story for the LadybugDB adapter.
5. **`mcp-server-ladybug`**: the user's description mentions this
   implementation. How does it compare to our MCP server? Is there
   value in either aligning or learning from it? Or is it a
   separate, standalone tool?

## Maintenance posture (learned from the turboquant scrap)

- **Pin to specific `lbug` SHAs**, not crates.io version ranges.
- **Monthly rebase** from upstream with paper-compliance / correctness
  tests run as part of CI on our side.
- **Upstream-first policy**: bug fixes get PR'd back to `lbug`
  upstream before carrying as local patches.
- **Archive path**: if `lbug` stalls for 6+ months with no upstream
  activity and no ai-memory-critical blocker, **scrap the adapter**
  and publish the scrap rationale (same format as the TurboQuant
  scrap note in the v0.6.0.0 CHANGELOG).

## What is explicitly out of scope

- **Replacing `SqliteStore` entirely**. Default stays SQLite for the
  foreseeable future. The `sql` in SQL is a well-understood debug
  surface — operators can attach `sqlite3` to an ai-memory DB in an
  incident. LadybugDB's columnar format is different.
- **Replacing `PostgresStore`**. Postgres+pgvector is the scale-out
  answer for deployments with existing Postgres operational
  expertise. LadybugDB competes at a different point in the design
  space (embedded-first, graph-first).
- **Dropping HNSW**. `instant-distance` integration is the
  SqliteStore's vector fallback. LadybugStore would replace it only
  inside the LadybugStore adapter.
- **Dropping `--features sqlcipher`**. Encryption-at-rest remains
  available on the SQLite path regardless of Ladybug progress.
- **A "100% transition"**. Not going to happen. The point of
  Phase 0's SAL trait is exactly to prevent this shape of decision.

## Decision log

| Date | Decision | Rationale |
|---|---|---|
| 2026-04-19 | **Adapter, not 100% transition** | SAL trait in #279 is the right seam. AI-agnostic value prop requires multi-backend support. 100% rewrite is ~4000 LOC and delays v0.6.0.0. |
| 2026-04-19 | **Phase 3 benchmarks before any promotion** | Learned from TurboQuant scrap — never commit to a dep swap without measurement. |
| 2026-04-19 | **Pin `lbug` to SHA, not version** | Learned from TurboQuant — young libraries need us to control the update cadence. |

## See also

- [RUNBOOK-adapter-selection.md](RUNBOOK-adapter-selection.md) — the
  v0.7.1 `serve --store-url` refactor this roadmap depends on.
- [SAL-related merge in release/v0.6.0](../CHANGELOG.md) — §
  "v0.7 Storage Abstraction Layer" documents the trait surface
  `LadybugStore` will implement.
- [TurboQuant scrap note](../CHANGELOG.md) — the cautionary tale
  informing the maintenance posture above.
