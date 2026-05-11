# Runbook — v0.7.0 adapter selection (`serve --store-url postgres://…`)

> **UPDATED 2026-05-09.** Per operator directive issued during the v0.7.0 A2A
> certification window, the adapter-selection refactor previously deferred
> to v0.7.1 **lands in v0.7.0** as Wave 3 of the v0.7.0 A2A campaign expanded
> scope. See `docs/v0.7.0/release-notes.md` for the full Wave 1-4 plan and
> [`docs/postgres-age-guide.md`](postgres-age-guide.md) for the operator
> how-to. The body of this runbook is preserved as design context — the
> "deferred" framing is historical; treat the roadmap below as the v0.7.0
> implementation plan.

Status: **landing in v0.7.0** (Wave 3 of v0.7.0 expanded scope).
Original scoping date: 2026-04-19 (v0.7.1 deferred).
Re-scoped: 2026-05-09 (folded into v0.7.0).
Depends on: #279 SAL trait, #283 migration tool, plus Wave 1 schema-init CLI
and Wave 2 schema parity v15→v28 (both also v0.7.0 expanded scope).

This runbook tracks the "daemon-level adapter selection" caveat that
v0.7-alpha originally carried into v0.7.1. The v0.7-alpha ship:

- Shipped the full `MemoryStore` trait + `SqliteStore` + `PostgresStore`
  adapters (#279).
- Shipped the one-shot `ai-memory migrate --from sqlite://… --to
  postgres://…` tool (#283).
- Did **not** support `ai-memory serve --store-url postgres://…`.

The v0.7.0 A2A campaign (`ai-memory-a2a-v0.7.0`) re-opened that scope
because the in-tree SAL contract test passed cleanly (20/20) and the
remaining gap was the daemon-level adapter selection plus a small set of
SQL/CLI surfaces — small enough to land within the v0.7.0 cert window
rather than carve out a v0.7.1 micro-release.

## What was blocking (preserved as design context)

`src/handlers.rs` (~2,000 LOC) dispatched writes, reads, recalls, and
sync directly through `crate::db::` free functions that assumed a
`Mutex<rusqlite::Connection>`. To flip `serve` onto `PostgresStore`,
every call site must route through `Box<dyn MemoryStore>` instead.

Specifically:

- **Write path** — `db::insert` / `db::update` / `db::delete` →
  `store.store()` / `store.update()` / `store.delete()`. ~15 sites.
- **Read path** — `db::get` / `db::list` / `db::search` →
  `store.get()` / `store.list()` / `store.search()`. ~12 sites.
- **Recall path** — `db::recall_hybrid` is SQLite-specific (FTS5 +
  HNSW in-process). Postgres needs a pgvector-backed equivalent. The
  SAL trait does NOT currently have a `recall` method; that's a
  follow-up trait extension (Wave 3).
- **Migrations** — `db::migrate` is SQLite-specific. Postgres has its
  own idempotent bootstrap in `src/store/postgres_schema.sql`.
- **Sync** — `sync_push` + `sync_since` touch memories directly; need
  trait methods.

## v0.7.0 implementation plan (Wave 1-3)

The expanded scope splits the v0.7.0 work into three implementation
waves, each verified independently before the next begins:

### Wave 1 — surgical postgres+AGE fixes (in flight 2026-05-09)

Three parallel streams close the F6 (#646) gap inventory without
touching adapter selection:

- **Stream A — SQL surfaces + LINKS + recall parity** (`src/store/postgres.rs`,
  `src/store/postgres_schema.sql`, `src/migrate.rs`, `src/store/sqlite.rs`,
  `src/store/mod.rs`, `tests/sal_contract.rs`,
  `tests/migrate_links_roundtrip.rs`, `tests/recall_scoring_parity.rs`).
  Implements `PostgresStore::link()` and `PostgresStore::register_agent()`
  to retire two `UnsupportedCapability` errors; backfills the four
  recall scoring factors postgres was missing (`access_count`,
  `confidence`, `tier_bonus`, `recency`); ports `migrate.rs` to walk
  `from.list_links()` so KG migrations carry edges.
- **Stream B — `ai-memory schema-init` CLI verb** (`src/cli/schema_init.rs`,
  `src/cli/mod.rs`, `src/daemon_runtime.rs`, `src/main.rs`,
  `tests/cli_schema_init.rs`). New verb that probes a `--store-url`,
  runs the postgres bootstrap, and (if AGE is detected) primes the
  graph projection. Idempotent on rerun.
- **Stream C — AGE 1.5 + PG 16 cypher-binding fix**
  (`tests/age_cte_equivalence.rs`). Test-side only — production code
  uses query-string interpolation through the AGE `cypher()` SQL
  function and never hit the binding quirk; the equivalence harness
  did. Fix unlocks the parity test suite on AGE 1.5.0.

### Wave 2 — postgres schema parity v15 → v28

Port the 13 SQLite migrations the postgres adapter is currently
missing (per `docs/coverage.md` schema-parity gap table):

| Migration | Feature gated |
|-----------|---------------|
| v16 | governance inheritance (cross-agent inherit=true) |
| v17 | webhook subscriptions |
| v18 | audit log chain |
| v19 | transcripts |
| v20 | signed events |
| v21 | agent quotas |
| v22 | link `attest_level` column |
| v23 | A2A correlation table |
| v24 | smart-load veto state |
| v25 | KG temporal-index v2 |
| v26 | tier-promotion metadata |
| v27 | subscription DLQ |
| v28 | `consolidated_from_agents` array |

Each migration ports as an idempotent `CREATE TABLE/INDEX IF NOT
EXISTS` + `ALTER TABLE … ADD COLUMN IF NOT EXISTS` pair following the
SQLite migration's intent. Tests in `tests/postgres_schema_parity.rs`
snapshot the full DDL set against the SQLite v28 truth fixture and
fail if either side drifts.

### Wave 3 — `ai-memory serve --store-url postgres://`

The original v0.7.1 design points (preserved below) become the v0.7.0
Wave 3 plan with one revision: the SAL `recall` extension lands as
part of Wave 1 (Stream A's recall scoring parity fix already touches
the trait shape), so Wave 3 reduces to handler routing + `AppState`
field plumbing.

1. **Trait extension** — `MemoryStore::recall_hybrid`,
   `MemoryStore::sync_push_batch`, `MemoryStore::sync_since` all live
   on the trait by end of Wave 1.
2. **SqliteStore continues to wrap `crate::db::`**. No behavior change
   for the default build.
3. **New AppState field** `store: Arc<dyn MemoryStore>`. Existing
   `db: Db` stays during the migration for back-compat.
4. **New serve flag** `--store-url <url>`. When set and
   `--features sal[-postgres]` is enabled, build the store adapter
   and use it. When unset, default to `SqliteStore::open(db_path)`.
5. **Feature-gate strictly**. Operators on the default build must
   see zero behavior change.

### Wave 4 — live A2A on postgres

Re-run the v0.7.0 A2A campaign (the `ai-memory-a2a-v0.7.0` repo) with
both droplets pointed at a shared postgres+AGE backend instead of
per-droplet sqlite. S70-S76 flip from "PASS via Path B in-tree
validators" to "PASS via live daemon-on-postgres" — that's the cert
acceptance criterion for the v0.7.0 expanded scope.

## Test plan for v0.7.0 (Waves 1-3)

- `tests/sal_contract.rs` (Wave 1, Stream A) — full SAL trait surface,
  20+ tests covering link, register_agent, recall scoring 6-factor.
- `tests/migrate_links_roundtrip.rs` (Wave 1, Stream A) — sqlite KG
  with N edges → postgres → sqlite preserves all N edges + relation
  types.
- `tests/recall_scoring_parity.rs` (Wave 1, Stream A) — same query
  against sqlite and postgres returns the same top-K with the same
  6-factor score breakdown (within FP tolerance).
- `tests/cli_schema_init.rs` (Wave 1, Stream B) — `ai-memory
  schema-init --store-url postgres://…` exits 0, creates schema
  v28, rerun is idempotent.
- `tests/age_cte_equivalence.rs` (Wave 1, Stream C) — both backends
  agree on `kg_query` / `kg_timeline` / `kg_invalidate` / `find_paths`
  outputs across a 100-entity / 500-edge KG.
- `tests/postgres_schema_parity.rs` (Wave 2) — DDL fingerprint
  against the SQLite v28 truth.
- `tests/serve_postgres_e2e.rs` (Wave 3) — spin `ai-memory serve
  --store-url postgres://…` against the docker-compose fixture and
  run `test_mcp_tools_list` + `test_cli_store_and_recall` through
  the HTTP API.
- v0.7.0 A2A campaign re-run (Wave 4) — S70-S76 flip from Path B to
  live-daemon-on-postgres.

## Status — landing in v0.7.0

- **Wave 1** — in flight as of 2026-05-09 (3 parallel streams under
  PR #643 / `round-2-fixes`). Expected commit prefix on
  `round-2-fixes`: `<wave-1-sha>`.
- **Wave 2** — pending Wave 1 merge.
- **Wave 3** — pending Wave 2 merge.
- **Wave 4** — pending Wave 3 + green A2A re-run on live postgres.

Tracking: master issue [#637](https://github.com/alphaonedev/ai-memory-mcp/issues/637),
F6 follow-through [#646](https://github.com/alphaonedev/ai-memory-mcp/issues/646),
PR [#643](https://github.com/alphaonedev/ai-memory-mcp/pull/643), expanded
scope tracker (filed alongside this runbook update).

## Explicitly out of scope for v0.7.0 (deferred to v0.7.1+)

- Live heterogeneous sync (SQLite ↔ Postgres bidirectional replication
  in production). The Wave 3 work supports a postgres-backed daemon
  and a sqlite-backed daemon as separate deployments; cross-backend
  *peering* is v0.7.1 at earliest.
- Migration between mismatched `Memory` shapes (schema rewriting
  beyond the v15→v28 ports).
- A store-agnostic recall benchmark publishing a single number. The
  in-process HNSW of `SqliteStore` and the pgvector HNSW of
  `PostgresStore` have different performance profiles; a like-for-like
  benchmark needs more care and a dedicated harness.

## CHANGELOG line for v0.7.0

> v0.7.0 ships postgres + Apache AGE as a **first-class storage
> backend**. `ai-memory serve --store-url postgres://…` is supported
> for live daemon use. The full schema (v28) is portable across
> SQLite and Postgres. KG features (`kg_query`, `kg_timeline`,
> `kg_invalidate`, `find_paths`) work identically on both backends —
> via pgvector + AGE Cypher on Postgres, FTS5 + HNSW + recursive-CTE
> on SQLite. See `docs/postgres-age-guide.md` and
> `docs/migration-v0.7.0-postgres.md`.
