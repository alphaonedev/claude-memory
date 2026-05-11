# Migration guide — sqlite → PostgreSQL on ai-memory v0.7.0

> **Audience.** Operators currently running ai-memory on sqlite who
> want to switch to PostgreSQL + Apache AGE on v0.7.0. The reverse
> direction (postgres → sqlite) is also supported and uses the same
> tool.
>
> See also: [`postgres-age-guide.md`](postgres-age-guide.md) for the
> postgres+AGE install and configuration prerequisites; this guide
> assumes those steps are already done.

## Why migrate

Stay on sqlite if all of these are true:

- Single-tenant single-daemon workstation deployment.
- Corpus comfortably fits in RAM (<1M memories typical).
- No multi-droplet A2A topology.
- AGE Cypher KG features are not in your hot path.

Migrate to postgres+AGE if any of these are true:

- Multi-tenant or multi-daemon (HTTP API behind a load balancer; or
  two `ai-memory serve` processes sharing a store).
- Corpus larger than RAM, or growing fast (10M+ memories).
- KG-heavy workloads — `find_paths` at depth ≥ 5 on a 1k+ entity
  graph; `kg_query` / `kg_timeline` in the recall hot path.
- Multi-droplet A2A federation where shared state is required (the
  v0.7.0 A2A campaign Wave 4 deployment shape).

The v0.7.0 SAL trait makes the backend choice an **operational**
decision, not a code one — every MCP / HTTP / CLI surface behaves
identically on either backend.

## Schema parity status (v0.7.0)

As of v0.7.0 (Wave 2 schema-parity port), both backends sit at
**schema_version=28**. The 13 migrations the v0.7-alpha postgres
adapter was missing (v16 → v28 — governance inheritance, webhook
subscriptions, audit chain, transcripts, signed events, agent
quotas, link `attest_level`, A2A correlation, smart-load veto,
KG temporal-index v2, tier-promotion metadata, subscription DLQ,
`consolidated_from_agents` array) are all ported.

If you migrated from sqlite to postgres on v0.7-alpha, your postgres
db is at v15. Re-run the migration tool against v0.7.0 (see
"In-place v15 → v28" below) before pointing a v0.7.0 daemon at it.

## Pre-flight checklist

Before you start:

- **Backup the sqlite db.** Either copy the file (with the daemon
  stopped — sqlite WAL means a hot copy can be inconsistent) or
  use `sqlite3 memory.db ".backup memory.bak.db"`. Either way, write
  the backup somewhere that survives a failed migration.
- **Stop any running `ai-memory` processes** that have the sqlite
  file open — daemon, MCP server, sync daemon, curator daemon. The
  migration tool requires exclusive access to the source.
- **Provision the postgres database** per `postgres-age-guide.md`
  ("Prerequisites" + "Database setup"). The target db must exist;
  the tool does not create it.
- **Note your current sqlite schema version:**
  ```bash
  sqlite3 ~/.local/share/ai-memory/memory.db \
    "SELECT user_version FROM pragma_user_version;"
  ```
  If this is **less than 28**, upgrade first: start `ai-memory serve`
  briefly against the sqlite db (it auto-migrates on connect),
  then stop it. The postgres side won't accept a partial migration.

## Step 1 — Bootstrap the postgres schema

```bash
ai-memory schema-init \
  --store-url postgres://aimemory:PASSWORD@HOST:5432/aimemory
```

Idempotent on rerun. Exit code 0 + `schema-init complete:
schema_version=28, kg_backend=AGE` is the success signal.

## Step 2 — Dry-run the migration

```bash
ai-memory migrate \
  --from sqlite:///$HOME/.local/share/ai-memory/memory.db \
  --to   postgres://aimemory:PASSWORD@HOST:5432/aimemory \
  --dry-run
```

The dry-run reports:

- Source row counts per table (memories, memory_links, namespaces,
  signed_events, transcripts, subscriptions, …).
- Target row counts (should be 0 for a fresh schema-init).
- Estimated migration time (back-of-envelope: ~5k rows / sec on a
  modern dev laptop; pgvector HNSW build time scales with corpus
  size, dominates the post-import phase).
- Schema parity check — confirms both sides are at v28.

Read the report. Investigate any "WARN" line before proceeding.

## Step 3 — Real migration

```bash
ai-memory migrate \
  --from sqlite:///$HOME/.local/share/ai-memory/memory.db \
  --to   postgres://aimemory:PASSWORD@HOST:5432/aimemory
```

What it does:

1. Opens both stores via the SAL trait (sqlite read-only, postgres
   read-write).
2. For each table in dependency order: walks `from.list_*()`, writes
   via `to.store_*()` in transactional batches (default 1000 rows
   per commit).
3. Walks `from.list_links()` (Wave 1 Stream A wire-up) and writes
   each via `to.store_link()`. Edge migration was missing in
   v0.7-alpha — v0.7.0 closes that gap.
4. Builds the pgvector HNSW index (deferred until after bulk insert
   to avoid per-row HNSW maintenance cost).
5. Primes the AGE projection with the migrated entity + memory
   nodes and edges.
6. Reports a per-table delta + a sha256 fingerprint of the canonical
   ordered content of every memory row (used for verification in
   step 4).

Idempotent on rerun: re-running the same migrate command UPSERTs
through `(id)` and is a no-op when the source hasn't changed.

## Step 4 — Verification

```bash
# Row-count parity.
sqlite3 ~/.local/share/ai-memory/memory.db \
  "SELECT 'memories', COUNT(*) FROM memories
   UNION ALL SELECT 'memory_links', COUNT(*) FROM memory_links
   UNION ALL SELECT 'namespaces', COUNT(*) FROM namespaces
   UNION ALL SELECT 'signed_events', COUNT(*) FROM signed_events
   UNION ALL SELECT 'memory_transcripts', COUNT(*) FROM memory_transcripts;"

psql 'postgres://aimemory:PASSWORD@HOST:5432/aimemory' -c "
  SELECT 'memories' AS tbl, COUNT(*) FROM memories
  UNION ALL SELECT 'memory_links', COUNT(*) FROM memory_links
  UNION ALL SELECT 'namespaces', COUNT(*) FROM namespaces
  UNION ALL SELECT 'signed_events', COUNT(*) FROM signed_events
  UNION ALL SELECT 'memory_transcripts', COUNT(*) FROM memory_transcripts;"
```

Counts must match. If `memory_links` is short on the postgres side,
you migrated against a pre-Wave-1 binary — re-run with the v0.7.0
binary that has Stream A's `migrate.rs` link-walk.

```bash
# Schema parity.
psql 'postgres://aimemory:PASSWORD@HOST:5432/aimemory' \
  -tAc "SELECT version FROM _ai_memory_schema_version ORDER BY version DESC LIMIT 1;"
# → 28
```

```bash
# Content fingerprint.
ai-memory verify-migration \
  --from sqlite:///$HOME/.local/share/ai-memory/memory.db \
  --to   postgres://aimemory:PASSWORD@HOST:5432/aimemory
```

`verify-migration` is provided by the migration tool and computes a
sha256 over the canonical-ordered contents of both stores. Identical
hashes ⇒ verified migration. Mismatched hashes ⇒ stop the cutover and
file an issue with the per-table count delta from above.

## Step 5 — Cutover

Once verification passes:

```bash
# (a) Stop the sqlite-backed daemon if it's still running.
sudo systemctl stop ai-memory   # or your service manager

# (b) Reconfigure for postgres. Recommended: systemd drop-in.
sudo systemctl edit ai-memory <<EOF
[Service]
Environment="AI_MEMORY_STORE_URL=postgres://aimemory:PASSWORD@HOST:5432/aimemory"
EOF

# (c) Start the daemon. It picks up the postgres URL from the env.
sudo systemctl start ai-memory
sudo systemctl status ai-memory   # confirm "store backend: PostgresStore"
```

Verify the live MCP / HTTP path:

```bash
curl -s http://localhost:9077/api/v1/capabilities | jq '.kg_backend, .store_backend'
# → "Age"
# → "PostgresStore"

ai-memory recall "smoke test" --json | jq '.mode, .memories | length'
# Should return your existing memories. If you get 0, double-check
# the URL — you may have started the daemon against an empty postgres.
```

## Reverse migration (postgres → sqlite)

The migration tool is bidirectional. If you need to fall back:

```bash
ai-memory migrate \
  --from postgres://aimemory:PASSWORD@HOST:5432/aimemory \
  --to   sqlite:///$HOME/.local/share/ai-memory/memory.db
```

Same dry-run / verify dance. Useful for:

- Taking a sqlite snapshot of a postgres prod store for offline
  analysis or backup.
- Rolling back a postgres deployment if a postgres-only regression
  surfaces (the migration is lossless either direction at v0.7.0
  schema parity).

## In-place v15 → v28 (postgres → postgres on the same host)

If you're upgrading an existing v0.7-alpha postgres db (schema v15)
to v0.7.0's v28 parity:

```bash
ai-memory schema-init \
  --store-url postgres://aimemory:PASSWORD@HOST:5432/aimemory \
  --upgrade
```

`schema-init --upgrade` walks the v15 → v28 ports idempotently.
Existing data is preserved; only DDL changes. The migration tool's
`--in-place` mode is the moral equivalent — pick whichever fits your
workflow.

## Rollback strategy

The cleanest rollback path:

1. **Keep the sqlite backup from the pre-flight checklist.** The
   migrate tool does not delete the source — your sqlite file is
   untouched after the migration, by design.
2. **Verify the sqlite file is still good** before disposing of it:
   ```bash
   sqlite3 memory.bak.db "PRAGMA integrity_check;"
   # → ok
   ```
3. **If you need to roll back** within the first 24-48h after cutover:
   - Stop the postgres-backed daemon.
   - Reverse-migrate any new writes that landed on postgres after
     cutover (use `--since <ISO8601>` on the migrate tool to pick up
     only the delta).
   - Restart the daemon pointing at the sqlite file.

If the postgres deployment is stable for >1 week, the rollback
window has effectively closed — the postgres store has accumulated
material new state, and you'd want a fresh forward-migration to
sqlite (which the tool supports) rather than a rollback.

## Known gotchas

### Embeddings carry over verbatim

The migration tool transports the embedding bytes as-is. The model
and dimension are recorded on each memory; the postgres side's
pgvector HNSW gets the same vectors the sqlite side's HNSW had. No
re-embedding required.

### FTS5 → tsvector swap

sqlite uses FTS5 for keyword search; postgres uses native `tsvector`
+ GIN index. The migration tool builds the postgres tsvector index
during the import phase. The two FTS rankers are **not** byte-identical —
you may see top-K shuffles in keyword-only queries between sqlite and
postgres for ties. The recall parity test (`tests/recall_scoring_parity.rs`)
asserts the **score** stays equal within FP tolerance, but the
underlying rankers differ slightly.

### AGE projection rebuild on first daemon start

If you migrate without the AGE projection priming step (e.g. you ran
the v0.7-alpha migrate tool, or you ran `schema-init --skip-age`),
the first time a v0.7.0 daemon connects with AGE enabled it will
prime the projection lazily on the first `kg_query`. That first
query may take 10-60 seconds on a large KG — subsequent queries are
fast. Pre-prime by running `ai-memory schema-init --upgrade` once.

### Auth secrets in URLs

If you put `aimemory:PASSWORD@…` directly in `--store-url`, the URL
appears in `ps` output and (on some shells) in `~/.bash_history`.
Prefer the `AI_MEMORY_STORE_URL` env var — set it via systemd
`EnvironmentFile=` from a `chmod 0600` file. The daemon reads the
env var and never logs the URL plaintext.

## References

- v0.7.0 release notes: [`v0.7.0/release-notes.md`](v0.7.0/release-notes.md)
- Postgres+AGE operator guide: [`postgres-age-guide.md`](postgres-age-guide.md)
- Adapter-selection design: [`RUNBOOK-adapter-selection.md`](RUNBOOK-adapter-selection.md)
- Schema parity gap (pre-Wave-2 reference): the table in
  `ai-memory-a2a-v0.7.0/docs/coverage.md` "Schema parity gap"
  section (now closed in v0.7.0).
- Migration tool source: `src/migrate.rs` + `tests/migrate_*` fixtures.
