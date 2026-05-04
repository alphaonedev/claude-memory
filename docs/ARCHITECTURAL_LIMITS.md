# Architectural Limits

ai-memory is SQLite-backed by default. This is the right choice for the
primary use case — a single agent (or small cooperating fleet) on a single
node, offline-first, zero ops. It is the wrong choice for several deployment
shapes, and this page is where those shapes are documented honestly so
bug reports can be closed with a pointer and users can plan accordingly.

The v0.7 Storage Abstraction Layer ([issue #221][221]) is the long-term
answer for everything on this page marked **Structural**. v0.6.0 GA ships
with two SQLite-specific polishes from the list (scope index, WAL
auto-checkpoint) and documents the rest here.

## Legend

- **Structural** — cannot be fixed in SQLite. Requires a different
  backend (Postgres + pgvector, LanceDB, Qdrant, Chroma) via the v0.7 SAL.
- **Polished in v0.6.0 GA** — addressed within SQLite.
- **Workaround** — possible but painful; not a design goal for v0.6.0.

## Limits

### 1. Single writer per database file — **Structural**

SQLite allows one writer at a time by design. WAL mode lets readers pass
the writer, but writers never run concurrently. Our `Arc<Mutex<Connection>>`
daemon compounds this by serializing readers too. A connection pool fixes
the daemon-side serialization within SQLite, but the file-level single-writer
ceiling (~500-2000 writes/sec on NVMe) remains.

**Impact ceiling:** ~1000-2000 writes/sec regardless of hardware.
**Workaround:** switch to Postgres via the v0.7 SAL when you need higher
write throughput; Postgres MVCC gives concurrent writers.
**v0.6.0 GA polish:** none. The connection pool was deferred to v0.7 SAL
because it becomes moot when the user can just pick a different backend.

### 2. Single-node only — **Structural**

No distributed SQLite. "Turso" is a fork, not stock SQLite, and behaves
differently in the places that matter (replication, consistency).

**Impact ceiling:** you hit one box's disk and CPU and that is the entire
budget.
**Workaround:** the v0.7 SAL lands Postgres (vertical scaling via replica
sets), Qdrant (horizontal sharding of the vector side), and LanceDB (S3
object-store backing for horizontal read scaling).

### 3. No synchronous replication / HA — **Structural**

Litestream is the state-of-the-art SQLite replication story and it is
asynchronous — every write replicates to S3 on a delay. On crash you lose
the unreplicated window. There is no multi-master.

**Impact:** RPO (recovery point objective) is measured in seconds to
minutes. For workloads needing zero-data-loss, SQLite cannot deliver.
**Workaround:** Postgres + synchronous replication via pg_replica.

### 4. Shared filesystems (NFS, SMB, FUSE, EFS) are unsafe — **Structural**

SQLite's POSIX advisory locking assumes real local fcntl semantics. Network
filesystems emulate those locks inconsistently and corruption can occur.
The SQLite docs explicitly warn against this configuration. This rules out
most container/Kubernetes shared-volume deployments without a dedicated
PVC per replica — which means you cannot horizontally scale the daemon.

**Impact:** rules out many cloud topologies.
**Workaround:** single-node deployment with local disk, or v0.7 SAL with
a backend that has a native wire protocol (Postgres, Qdrant).

### 5. No native client-server protocol — **Structural**

SQLite is embedded. Remote access goes through our HTTP daemon, which
reintroduces the `Mutex<Connection>` bottleneck. Postgres, MySQL, Qdrant,
pgvector all have real wire protocols.

**Impact:** the HTTP daemon is the bottleneck at fleet scale, not SQLite's
write lock.
**v0.7 SAL:** Postgres adapter removes the daemon-as-bottleneck entirely;
agents connect directly to Postgres over its wire protocol.

### 6. FTS5 does not port — **Structural**

Every keyword-search query is SQLite-specific. Moving to Postgres means
rewriting against `tsvector`; Qdrant uses its own full-text filter model;
Chroma has no FTS at all.

**Impact:** the keyword recall path is not portable today.
**v0.7 SAL:** `MemoryStore::keyword_search` is trait-level; each backend
implements it natively. Chroma's missing FTS is handled by the
`Capabilities::NATIVE_FTS` bit and a Rust-side fallback in the core
layer.

### 7. No CDC / audit stream — **Structural**

Postgres has logical replication and `wal2json`. MySQL has binlog. SQLite
has neither. Changes can be emitted via triggers, but there is no external
consumer protocol.

**Impact:** event-driven pipelines cannot tap changes without bolt-on
triggers.
**v0.7 SAL:** Postgres adapter exposes CDC natively.

### 8. Schema migration DDL is feature-poor — **Workaround**

`ALTER TABLE` can add columns but cannot add `CHECK` constraints, cannot
make columns `NOT NULL` without a default, and cannot drop columns on
older SQLite. `ALTER TABLE` also takes the database write lock.

**Workaround:** we use "create new table + copy + swap" for breaking
schema changes. Works, but each migration is more code than in Postgres.

### 9. `LIMIT … OFFSET` degrades linearly — **Workaround**

SQLite has no cursor pagination built in. A 100k-offset query scans 100k
rows it will throw away. Bites on `list_memories` once a namespace grows
large.

**Impact:** noticeable after ~50k rows per namespace.
**Workaround:** keyset pagination (`WHERE created_at < ?`) — not yet
implemented. Tracked as part of v0.7 `Capabilities::CURSOR_PAGINATION`.

### 10. HNSW vector index is in-process — **Structural (within SQLite)**

SQLite has no native vector index. We hold the HNSW in RAM per daemon
process: ~1.5 GB for 1M memories at 384-dim. Restart rebuilds from
`get_all_embeddings()` — slow for large corpora.

**Impact:** memory footprint and cold-start cost.
**v0.7 SAL:** LanceDB and Qdrant backends have native on-disk vector
indexes; Postgres + pgvector has `ivfflat`/`hnsw` indexes with durable
state.

### 11. `json_extract` in WHERE is not indexed by default — **Polished in v0.6.0 GA**

Visibility filter (`scope`) used to re-evaluate
`COALESCE(json_extract(metadata, '$.scope'), 'private')` per row —
linear in matched-namespace rows.

**Polish (v0.6.0 GA, schema v10):** `scope_idx` virtual generated
column on that exact expression, plus `idx_memories_scope_idx` B-tree.
`visibility_clause` now compares against the column, so the query
planner picks the index. `EXPLAIN QUERY PLAN` on a scope-filtered recall
shows an index seek rather than a scan.

### 12. WAL file grows unbounded without checkpoints — **Polished in v0.6.0 GA**

Under continuous writes SQLite's built-in auto-checkpoint (every 1000
pages) can leave the `-wal` file at hundreds of MB between fires — not
a correctness issue, but a storage-footprint surprise.

**Polish (v0.6.0 GA):** dedicated background task in the daemon calls
`db::checkpoint` on a 10-minute cadence, staggered from GC to avoid
lock bursts. Shutdown still runs a final checkpoint.

### 13. KG link invalidation is eventually consistent across peers — **Documented in v0.6.3**

`memory_kg_invalidate` updates `valid_until` on the local SQLite copy
without quorum-broadcasting the change. Peers learn about the
invalidation asynchronously through the sync-daemon's pull cycle
(default 2-second interval).

Temporal anchoring makes this benign in steady state: a link's
`valid_until` is timestamped, so a peer that learns of the
invalidation 5 seconds late still records the same `valid_until =
T_inv` and queries pinned to `valid_at < T_inv` correctly return the
link as valid. But applications that require strongly-consistent
invalidation (e.g. invalidate then immediately re-query the graph
from a different peer) must wait at least `--interval` seconds, or
read from the writing peer.

Full design rationale + recovery procedures: see
[`ADR-0003`](ADR-0003-kg-invalidation-eventual-consistency.md).

### 14. KG schema v15 is backward-incompatible across the federation — **Documented in v0.6.3**

The temporal-validity columns added to `memory_links` in v0.6.3
(schema migration v15) are NOT wire-compatible with v0.6.2 peers.
A v14-schema peer that receives a v15 push fails the INSERT (unknown
columns) and the row is rejected.

Operators upgrading a federation mesh from v0.6.2 to v0.6.3 must:

1. Drain writes for the upgrade window
2. Bring all peers down (do NOT do a rolling upgrade)
3. Replace the binary on every peer
4. Bring all peers up — migration runs on first open
5. Verify schema_version 15 on every peer before resuming writes

See [`MIGRATION-v0.6.2-to-v0.6.3.md`](MIGRATION-v0.6.2-to-v0.6.3.md)
for the full procedure and
[`ADR-0002`](ADR-0002-kg-schema-v15-backward-incompat.md) for the
design rationale.

## Use-case guidance

| Deployment | Backend | Notes |
|---|---|---|
| Single user, local agent (Claude Code on one laptop) | SQLite | Ideal. Zero ops. Keep. |
| Small team fleet, 1-25 agents, <100k memories | SQLite | v0.6.0 GA works well. |
| Large fleet, 100+ agents, ≥1M memories | Postgres (v0.7) | Structural limits 1, 2, 10 bite. |
| Multi-region / HA / zero-data-loss | Postgres (v0.7) | Structural limit 3 rules out SQLite. |
| Shared filesystem / Kubernetes PVC-per-replica | Postgres or Qdrant (v0.7) | Structural limit 4. |
| Vector-first workload, low metadata | Qdrant or LanceDB (v0.7) | Native ANN beats in-process HNSW. |
| Change-data-capture (CDC) required | Postgres (v0.7) | Structural limit 7. |

## What v0.6.0 GA does *not* fix

Everything marked **Structural** above. These are not bugs, they are
properties of SQLite. File them against [issue #221][221] if they're
blocking your use case — the v0.7 SAL is the answer.

[221]: https://github.com/alphaonedev/ai-memory-mcp/issues/221
