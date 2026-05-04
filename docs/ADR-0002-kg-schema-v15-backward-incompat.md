# ADR-0002 — KG schema v15 is backward-incompatible

Status: **Accepted** — implemented in v0.6.3 (PRs #384, #388–#392).

Date: 2026-04-26
Author: Claude Opus 4.7 (1M context) on behalf of @binary2029
Related: ADR-0001 (Quorum replication), `docs/MIGRATION-v0.6.2-to-v0.6.3.md`

---

## Context

v0.6.3 introduced **schema migration v15** to back the temporal
knowledge graph (Pillar 2 / Streams B–D). The changes:

- Added four columns to `memory_links`: `valid_from`, `valid_until`,
  `observed_by`, `signature` (SQLite + Postgres parity).
- Added three indexes: `idx_links_temporal_src`, `idx_links_temporal_tgt`,
  `idx_links_relation`.
- Added the `entity_aliases` side table.
- Backfilled `valid_from = (SELECT created_at FROM memories ...)` on
  every existing link.

The migration is **idempotent on a single node**: applying it to a
v14-schema SQLite DB leaves the result equivalent to a fresh v15
init. The audit at `01-database.md` confirmed this works for SQLite.

But the federation wire format includes the new columns. A v0.6.2
peer that receives a `memory_links` row from a v0.6.3 peer over
`/api/v1/sync/push` cannot parse the `valid_from` / `valid_until`
fields and rejects the row.

## Decision

We accept that schema v15 is **a backward-incompatible federation
upgrade** and require operators to coordinate the upgrade across
all peers in a quorum mesh. We do NOT:

1. Ship a wire-compatibility shim that strips temporal fields when
   pushing to v0.6.2 peers. The temporal fields are central to the
   v0.6.3 pillar; degrading them would silently drop invalidations.
2. Negotiate a wire version at the start of each sync cycle. This
   was considered (a `version` field in the sync handshake) but
   adds protocol complexity for a one-time cost — operators upgrade
   peers in lockstep once, not per-cycle.
3. Auto-upgrade Postgres deployments. The Postgres adapter remains
   **fresh-init only** in v0.6.3 (audit `01-database.md`); operators
   either stay on SQLite, run the manual `ALTER TABLE` SQL from the
   migration guide, or use the `migrate` subcommand to dump-and-reload.

## Consequences

### Required operator action

When upgrading a federation mesh from v0.6.2 to v0.6.3:

1. **Drain writes** — pause client agents or redirect writes to one
   designated peer for the upgrade window.
2. **Bring all peers down** — do NOT perform a rolling upgrade where
   some peers run v0.6.2 and others run v0.6.3.
3. **Replace binaries** — `cargo install ai-memory --version 0.6.3`
   on every host (or equivalent OS-package step).
4. **Bring all peers up** — the migration runs on first open.
5. **Verify schema_version 15 on every peer** before resuming writes.
6. **Resume writes**.

The migration guide (`docs/MIGRATION-v0.6.2-to-v0.6.3.md`) carries
the full procedure with copy-pasteable commands.

### Failure modes if the operator skips coordination

- **Mixed v14 + v15 mesh:** writes from v15 peers fail INSERT on v14
  peers (unknown columns); v14 peers stay in a rolling
  divergence state until upgraded. Sync-daemon never heals the v14
  peer because every push fails.
- **One peer skipped:** quorum may still meet (W-1 from v15 peers +
  local commit) but the unupgraded peer accumulates schema-mismatch
  errors and falls behind silently. Detected via
  `federation_fanout_dropped_total[id_drift]` metric.

### Recovery

If a v14 peer is left in a v15 mesh:
1. Stop the v14 peer's sync-daemon.
2. Upgrade its binary + run the migration.
3. Restart sync-daemon. The pull cycle catches up the missed window
   from the still-running v15 peers.

No data loss expected — v15 peers retained the writes that v14
rejected; the rejected pushes are reapplied as new pulls after
upgrade.

## Future work

- **v0.7 Layer 2b (attested sender_id)** may require another wire-
  level change. ADR for that landing will reference this one as
  precedent for "lockstep peer upgrade" as the standard procedure
  for backward-incompatible KG schema bumps.
- **In-place Postgres migration** — fresh-init-only in v0.6.3 is a
  known adoption blocker for operators running Postgres clusters.
  v0.7 should ship a proper Postgres migration tool comparable to
  the SQLite path. Tracked separately.
