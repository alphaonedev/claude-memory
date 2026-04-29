# Migrating from ai-memory v0.6.2 to v0.6.3

**Audience:** operators upgrading running v0.6.2 deployments to v0.6.3.
**Risk profile:** schema changes are additive and idempotent on **SQLite**.
On **Postgres** the v0.6.3 adapter is fresh-init only — see "Postgres
upgrade path" below.

> **TL;DR (SQLite):** stop the daemon, replace the binary, start the
> daemon. The migration runs on first open. No downtime expected on a
> single node; quorum-mesh deployments require coordinated upgrade
> (see "Federation upgrade order").

---

## What changed in v0.6.3

Three pillars (all charter-aligned, all functionally complete):

1. **Hierarchical namespace taxonomy (Pillar 1 / Stream A)** — new
   `memory_get_taxonomy` MCP tool + `GET /api/v1/taxonomy` HTTP route.
   Existing flat namespaces continue to work unchanged; a namespace
   that contains a `/` (e.g. `alphaone/engineering/platform`) is
   automatically treated as a hierarchical path with parent walks.

2. **Temporal-validity knowledge graph (Pillar 2 / Streams B–D)** —
   `memory_links` table gains four columns (`valid_from`,
   `valid_until`, `observed_by`, `signature`), an `entity_aliases`
   side table, and seven new MCP tools: `memory_kg_query`,
   `memory_kg_timeline`, `memory_kg_invalidate`,
   `memory_entity_register`, `memory_entity_get_by_alias`,
   `memory_check_duplicate`, plus the taxonomy tool above.

3. **Performance budgets (Pillar 3 / Streams E–F)** — `tracing` spans
   on every MCP tool, an `ai-memory bench` subcommand, and a
   `bench.yml` GitHub Actions workflow that fails any PR whose p95
   exceeds the published budget by more than 10 %.

Full deliverable inventory: see `CHANGELOG.md` `[Unreleased] — v0.6.3`
section.

---

## SQLite upgrade

### Pre-flight (recommended)

```sh
# 1. Snapshot the live DB
sqlite3 /path/to/ai-memory.db ".backup /path/to/ai-memory-pre-v063.db"

# 2. Confirm current schema version (expect 14)
sqlite3 /path/to/ai-memory.db "SELECT version FROM schema_version ORDER BY version DESC LIMIT 1;"
```

### Upgrade

```sh
# 1. Stop the daemon (or the MCP host that owns it)
launchctl bootout gui/$(id -u)/com.alphaonedev.ai-memory   # macOS / launchd
systemctl --user stop ai-memory                             # Linux / systemd

# 2. Install v0.6.3
cargo install ai-memory --version 0.6.3
# OR
brew upgrade ai-memory
# OR
apt-get install ai-memory=0.6.3

# 3. Start the daemon — the schema-v15 migration runs on first open
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.alphaonedev.ai-memory.plist
systemctl --user start ai-memory

# 4. Verify
sqlite3 /path/to/ai-memory.db "SELECT version FROM schema_version ORDER BY version DESC LIMIT 1;"
# Expect: 15
```

### What the v15 migration does

- `ALTER TABLE memory_links ADD COLUMN valid_from TEXT;`
- `ALTER TABLE memory_links ADD COLUMN valid_until TEXT;`
- `ALTER TABLE memory_links ADD COLUMN observed_by TEXT;`
- `ALTER TABLE memory_links ADD COLUMN signature BLOB;`
- `CREATE INDEX IF NOT EXISTS idx_links_temporal_src ON memory_links(source_id, valid_from, valid_until);`
- `CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt ON memory_links(target_id, valid_from, valid_until);`
- `CREATE INDEX IF NOT EXISTS idx_links_relation ON memory_links(relation, valid_from);`
- `UPDATE memory_links SET valid_from = (SELECT created_at FROM memories WHERE id = source_id) WHERE valid_from IS NULL;`
- `CREATE TABLE IF NOT EXISTS entity_aliases (entity_id TEXT NOT NULL, alias TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (entity_id, alias));`
- `CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias ON entity_aliases(alias);`

The migration is **idempotent** — restarting the daemon twice in a row
is safe.

### Rollback

If you need to revert to v0.6.2 after upgrading:

```sh
# 1. Stop v0.6.3
launchctl bootout gui/$(id -u)/com.alphaonedev.ai-memory

# 2. Restore the pre-upgrade snapshot
cp /path/to/ai-memory-pre-v063.db /path/to/ai-memory.db

# 3. Reinstall v0.6.2
cargo install ai-memory --version 0.6.2 --force

# 4. Start
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.alphaonedev.ai-memory.plist
```

The new columns added by the migration are nullable, so a v0.6.2
binary CAN read a v15 database — but it will not honor `valid_until`
filtering or use `entity_aliases`. Restoring the pre-upgrade snapshot
is the recommended rollback path.

---

## Postgres upgrade path

**Important:** the Postgres backend in v0.6.3 is **fresh-init only**.
It does NOT auto-migrate an existing v0.6.2 schema. If you have a
running Postgres deployment, you have three choices:

### Option 1 — Stay on SQLite (recommended for single-node)

The SQLite backend has full v0.6.3 feature support. If your deployment
fits on one node, switching back is a `--db file:///path.db` flag
change.

### Option 2 — Manual Postgres migration

Apply the equivalent SQL to your existing Postgres database:

```sql
ALTER TABLE memory_links ADD COLUMN IF NOT EXISTS valid_from TEXT;
ALTER TABLE memory_links ADD COLUMN IF NOT EXISTS valid_until TEXT;
ALTER TABLE memory_links ADD COLUMN IF NOT EXISTS observed_by TEXT;
ALTER TABLE memory_links ADD COLUMN IF NOT EXISTS signature BYTEA;

CREATE INDEX IF NOT EXISTS idx_links_temporal_src
    ON memory_links(source_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt
    ON memory_links(target_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_relation
    ON memory_links(relation, valid_from);

UPDATE memory_links
   SET valid_from = (
       SELECT created_at FROM memories WHERE id = memory_links.source_id
   )
 WHERE valid_from IS NULL;

CREATE TABLE IF NOT EXISTS entity_aliases (
    entity_id  TEXT NOT NULL,
    alias      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (entity_id, alias)
);
CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
    ON entity_aliases(alias);
```

Run these inside a transaction. The `signature` column uses `BYTEA`
on Postgres (vs `BLOB` on SQLite) — both store opaque byte strings.

### Option 3 — Dump and reload via the migration tool

```sh
# 1. Dump v0.6.2 Postgres data to SQLite
ai-memory migrate \
    --from postgres://user@host/ai_memory \
    --to file:///tmp/ai-memory-staging.db

# 2. Initialise a fresh v0.6.3 Postgres schema
psql -h newhost -d ai_memory_v063 -f src/store/postgres_schema.sql

# 3. Reverse-migrate
ai-memory migrate \
    --from file:///tmp/ai-memory-staging.db \
    --to postgres://user@newhost/ai_memory_v063
```

The `migrate` subcommand performs page-streamed upsert-on-id transfer
and is idempotent. Test on a staging copy first.

---

## Federation upgrade order

The v15 schema is **backward-incompatible at the wire level**. A v0.6.2
peer cannot parse `valid_from` / `valid_until` columns in the
`memory_links` JSON pushed by a v0.6.3 peer. Mixing v14-schema and
v15-schema peers in the same quorum mesh will break replication.

### Recommended sequence

1. **Drain** — stop accepting writes for the upgrade window. Either
   pause your client agents, or redirect writes to one designated peer.
2. **Upgrade peers in lockstep** — bring all peers down, replace
   binaries, bring them back up. Do NOT perform a rolling upgrade
   where some peers are v0.6.3 and others remain v0.6.2.
3. **Verify schema_version on every peer** before resuming writes:
   ```sh
   for host in peer-a peer-b peer-c; do
       ssh $host "sqlite3 /var/lib/ai-memory.db 'SELECT MAX(version) FROM schema_version;'"
   done
   ```
   Expect `15` from every peer.
4. **Resume** — clients can now write again. The sync-daemon catches
   up on any drift accrued during the drain window.

### KG link invalidation is eventually consistent

`memory_kg_invalidate` updates `valid_until` on the local SQLite copy
**without** broadcasting via the quorum-write path. Peers learn about
the invalidation asynchronously through the sync-daemon's pull cycle.
This is correct by design (link invalidations are time-anchored, so
late replication is observable as "this link became invalid at time
T") but operators should know that:

- A query against peer A immediately after `memory_kg_invalidate` may
  return the now-invalid link until peer A's sync-daemon pulls from
  the writer.
- For strongly-consistent invalidation semantics, run the invalidate
  against every peer in turn (or wait `--interval` seconds between
  the write and the read).

This may be tightened in v0.7 with quorum-broadcast invalidations.

---

## Operator-visible API changes

### New MCP tools (zero breaking changes to existing tools)

| Tool | Stream | Purpose |
|---|---|---|
| `memory_get_taxonomy` | A | Walk live memories grouped by namespace into a hierarchical tree |
| `memory_kg_query` | C | Recursive CTE traversal with depth 1..=5, temporal/agent filters |
| `memory_kg_timeline` | C | Ordered fact timeline for an entity (valid_from-anchored) |
| `memory_kg_invalidate` | C | UPDATE `valid_until` on a link to mark it superseded (does NOT delete) |
| `memory_entity_register` | B | Register entity-as-typed-memory with aliases |
| `memory_entity_get_by_alias` | B | Resolve an alias to its canonical entity |
| `memory_check_duplicate` | D | Embedding cosine-similarity duplicate detection |

See `docs/USER_GUIDE.md` for parameter tables and example requests.
See `docs/API_REFERENCE.md` for the matching HTTP endpoints.

### New HTTP endpoints

```
GET    /api/v1/taxonomy
POST   /api/v1/check_duplicate
POST   /api/v1/entities
GET    /api/v1/entities/by_alias
GET    /api/v1/kg/timeline
POST   /api/v1/kg/invalidate
POST   /api/v1/kg/query
```

### Performance budgets

`PERFORMANCE.md` at the repo root documents 13 hot-path budgets with
p95 and p99 targets. The `bench.yml` workflow fails any PR whose
measured p95 exceeds the budget by more than 10 %. p99 targets are
informational until the v0.6.3 soak window closes.

The `ai-memory bench` subcommand runs the same workload locally:

```sh
ai-memory bench                      # human-readable table
ai-memory bench --json               # machine-parseable JSON
ai-memory bench --iterations 1000    # custom sample size
```

---

## Validation checklist

After upgrading, confirm:

- [ ] `ai-memory --version` reports `0.6.3`
- [ ] `sqlite3 ... 'SELECT MAX(version) FROM schema_version;'` returns `15`
- [ ] `ai-memory bench` completes without exit-code-non-zero
- [ ] MCP `tools/list` includes the seven new tool names above
- [ ] `curl -s http://localhost:PORT/api/v1/health` returns `200 OK`
- [ ] Existing memories are recallable (`ai-memory recall "any test phrase"`)
- [ ] `ai-memory taxonomy` returns the namespace tree without error
- [ ] (Federation) every peer reports schema_version 15

If any of the above fail, restore the pre-upgrade snapshot and file an
issue with the failing check + relevant logs.

---

## Where to ask for help

- File an issue: <https://github.com/alphaonedev/ai-memory-mcp/issues>
- Read the troubleshooting guide: `docs/TROUBLESHOOTING.md`
- Read the architectural limits: `docs/ARCHITECTURAL_LIMITS.md`
