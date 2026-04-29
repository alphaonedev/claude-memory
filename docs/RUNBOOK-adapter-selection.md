# Runbook — v0.7.1 adapter selection (`serve --store-url postgres://…`)

Status: **deferred (design scoped, implementation not yet started)**.
Date: 2026-04-19
Depends on: #279 SAL trait, #283 migration tool.

This runbook tracks the "daemon-level adapter selection" caveat from
the v0.7-alpha release. The v0.7-alpha ship:

- Ships the full `MemoryStore` trait + `SqliteStore` + `PostgresStore`
  adapters (#279).
- Ships the one-shot `ai-memory migrate --from sqlite://… --to
  postgres://…` tool (#283).
- Does **not** yet support `ai-memory serve --store-url postgres://…`.

## What's blocking

`src/handlers.rs` (~2 000 LOC) dispatches writes, reads, recalls,
and sync directly through `crate::db::` free functions that assume
a `Mutex<rusqlite::Connection>`. To flip `serve` onto `PostgresStore`
every call site needs to route through `Box<dyn MemoryStore>`
instead.

Specifically:

- **Write path** — `db::insert` / `db::update` / `db::delete` →
  `store.store()` / `store.update()` / `store.delete()`. ~15 sites.
- **Read path** — `db::get` / `db::list` / `db::search` →
  `store.get()` / `store.list()` / `store.search()`. ~12 sites.
- **Recall path** — `db::recall_hybrid` is SQLite-specific (FTS5 +
  HNSW in-process). Postgres needs pgvector-backed equivalent. The
  SAL trait does NOT currently have a `recall` method; that's a
  follow-up trait extension.
- **Migrations** — `db::migrate` is SQLite-specific. Postgres has
  its own idempotent bootstrap in `src/store/postgres_schema.sql`.
- **Sync** — `sync_push` + `sync_since` touch memories directly; need
  trait methods.

## Design notes

1. **Trait extension first**. Add `MemoryStore::recall_hybrid`,
   `MemoryStore::sync_push_batch`, `MemoryStore::sync_since`.
   Otherwise the handlers still need the concrete backend.
2. **SqliteStore continues to wrap `crate::db::`**. No behavior
   change for the default build.
3. **New AppState field** `store: Arc<dyn MemoryStore>`. Existing
   `db: Db` stays during the migration for back-compat.
4. **New serve flag** `--store-url <url>`. When set and
   `--features sal[-postgres]` is enabled, build the store adapter
   and use it. When unset, default to `SqliteStore::open(db_path)`.
5. **Feature-gate strictly**. Operators on the default build must
   see zero behavior change.

## Test plan for v0.7.1

- Add `MemoryStore::recall_hybrid` to the trait with a default that
  composes `search` + local cosine (no HNSW). Adapters with native
  vector (`Capabilities::NATIVE_VECTOR`) override with a real
  implementation.
- Add `MemoryStore::sync_push_batch` + `sync_since`.
- Port 3 handlers per PR (one for each of write / read / recall /
  sync) to the trait surface. Keep PRs small enough to review.
- Add an integration test that spins `ai-memory serve
  --store-url postgres://…` against the docker-compose fixture and
  runs `test_mcp_tools_list` + `test_cli_store_and_recall` through
  the HTTP API.

## Explicitly out of scope for v0.7.1

- Live heterogenous sync (SQLite ↔ Postgres replication). That's
  v0.7.2 at earliest.
- Migration between mismatched `Memory` shapes (schema rewriting).
- A store-agnostic recall benchmark. The in-process HNSW of the
  SqliteStore path and the pgvector HNSW of the PostgresStore path
  have different performance profiles; a like-for-like benchmark
  needs more care.

## Honest status line for CHANGELOG

Until v0.7.1 ships, the CHANGELOG carries:

> v0.7-alpha ships one-shot migration between SQLite and
> Postgres/pgvector. Running `ai-memory serve` against Postgres as
> the **live** store requires v0.7.1's adapter-selection refactor —
> deferred.

That's the honest line. `--store-url` is NOT shipping in v0.7-alpha.
