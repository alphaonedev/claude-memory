# ADR-0003 — `memory_kg_invalidate` is eventually consistent across the federation

Status: **Accepted** — implemented in v0.6.3 (PR #390).

Date: 2026-04-26
Author: Claude Opus 4.7 (1M context) on behalf of @binary2029
Related: ADR-0001 (Quorum replication), ADR-0002 (Schema v15)

---

## Context

`memory_kg_invalidate` (`POST /api/v1/kg/invalidate`) marks a
knowledge-graph link as superseded by setting its `valid_until`
column. The link is **NOT deleted** — historical queries pinned to
`valid_at < valid_until` still see it; only "current state" queries
filter it out.

The audit at `05-federation.md` flagged a deliberate-but-noteworthy
design choice: **the invalidate path does NOT call
`broadcast_store_quorum`.** It updates the local SQLite copy and
returns success to the caller without waiting for any peer ack. Peers
learn about the invalidation asynchronously through the sync-daemon's
pull cycle (default 2-second interval).

Memory writes (store / update / archive / promote) DO take the
quorum-broadcast path. Memory_link mutations (link create + invalidate)
do not.

## Decision

KG link invalidations remain **eventually consistent**, NOT
strongly-consistent. We accept this asymmetry between memory mutations
(quorum-broadcast) and link mutations (sync-daemon-driven) because:

### 1. Temporal anchoring makes "late propagation" semantically benign

A link's `valid_until` is timestamped. A peer that learns of the
invalidation 5 seconds late still records `valid_until = T_inv`; a
historical query pinned to `valid_at = T < T_inv` correctly returns
the link as valid; a current-state query at `T >= T_inv` correctly
excludes it. There is no observable inconsistency from the
application's perspective — only a propagation lag.

Compare with memory store: a peer that misses a memory entirely
returns no row, which IS an observable inconsistency. Hence the
asymmetry is principled.

### 2. Quorum-broadcasting every link mutation is too expensive

A typical campaign or curator cycle invalidates dozens to hundreds of
links per minute as the temporal graph evolves. Quorum-broadcasting
each one with the same deadline + ack-collection machinery as memory
writes would multiply federation traffic without proportional
correctness gain (see #1).

### 3. The sync-daemon already replicates `memory_links`

The pull-side handler (`/api/v1/sync/since`) emits the full link rows
including `valid_from` / `valid_until`. The sync-daemon's pull cycle
(default 2s) catches invalidations within one cycle in steady state.

## Consequences

### Operator-visible behavior

A query against peer A immediately after `memory_kg_invalidate` on
peer B may return the now-invalid link until peer A's sync-daemon
pulls from peer B. The lag is bounded by `--interval` (default 2s).

For applications that require **strongly-consistent invalidation**
(e.g. a contradiction-detection workflow that immediately re-queries
the graph after invalidation), the operator must:

1. Run the invalidate against every peer in the mesh in turn, OR
2. Wait at least `max(--interval)` seconds between the invalidate
   and the dependent read, OR
3. Read from the same peer that wrote the invalidation.

This is documented in:

- `docs/MIGRATION-v0.6.2-to-v0.6.3.md` (operator guide, "KG link
  invalidation is eventually consistent" section)
- `docs/USER_GUIDE.md` (`memory_kg_invalidate` tool reference,
  federation note callout)
- `docs/API_REFERENCE.md` (`POST /api/v1/kg/invalidate` endpoint)

### Failure modes

- **Sync-daemon partition:** a peer cut off from the writer never
  learns of invalidations until the partition heals. This is the
  same failure mode as memory writes under the same partition;
  handled by the same recovery path (pull cycle resumes after
  partition heals).
- **Peer crashes mid-invalidate-cycle:** the writer succeeded
  locally; the peer that crashed will pull the invalidation on next
  sync-daemon cycle after restart.
- **Concurrent invalidations of the same link:** last-write-wins on
  `valid_until`. Two peers invalidating the same link with different
  timestamps will eventually converge to whichever peer's value
  ended up most recently in the sync stream. The
  `previous_valid_until` field returned by `memory_kg_invalidate`
  surfaces overwrites for monitoring.

### Tested behavior

- `src/db.rs::tests::invalidate_link_overwrites_existing_valid_until_and_reports_prior`
  pins last-write-wins semantics at the local-storage layer.
- `tests/integration.rs::test_sync_daemon_mesh_propagates_memory_between_peers`
  validates that the sync-daemon pull cycle reaches peers within the
  documented window for memory mutations. The test was updated in
  PR-rc1 to use a `ChildGuard` RAII wrapper (same fix shipped in
  #401 for the mTLS test).

## Future work — v0.7 candidates

If application demand surfaces for strongly-consistent invalidation,
two paths remain available:

1. **Add a `--quorum-invalidate` CLI flag** that opts in to the
   memory-mutation broadcast path on a per-call basis. Default would
   stay async. Operators of high-stakes workflows could request
   strong consistency where it matters.
2. **Promote link mutations to first-class quorum-broadcast** in
   v0.7 alongside the attested-sender_id work. This would unify the
   correctness model at the cost of the federation traffic increase
   noted above. Worth revisiting after Phase 2 testing data
   quantifies the actual lag distribution under realistic load.
