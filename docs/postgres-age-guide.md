# PostgreSQL + Apache AGE operator guide (ai-memory v0.7.0)

> **Audience.** Operators running `ai-memory` who want PostgreSQL as the
> live storage backend, with Apache AGE for graph queries and pgvector
> for semantic recall. **As-of v0.7.0**, postgres+AGE is a first-class
> backend — `ai-memory serve --store-url postgres://…` is the supported
> production deployment shape.
>
> If you only want sqlite, you don't need any of this — the default
> `ai-memory serve` continues to work exactly as it did in v0.6.x. The
> postgres path is opt-in.
>
> See also: [`migration-v0.7.0-postgres.md`](migration-v0.7.0-postgres.md)
> for the sqlite → postgres migration runbook, and
> [`RUNBOOK-adapter-selection.md`](RUNBOOK-adapter-selection.md) for
> the adapter-selection design notes.

## Why postgres+AGE

ai-memory's sqlite path is fast, simple, and has zero operational
overhead — for a single workstation or a single daemon, it is the right
choice. Switch to postgres+AGE when one or more of these is true:

- **Multi-tenant scale.** A single sqlite file behind a single
  `Mutex<Connection>` becomes the bottleneck once you cross ~5
  concurrent writers. Postgres' MVCC removes that ceiling.
- **Larger than RAM.** sqlite + HNSW keeps the full vector index in
  memory. pgvector's HNSW lives on disk and pages on demand —
  practical for 10M+ memory corpora.
- **AGE Cypher KG.** ai-memory's KG operations (`kg_query`,
  `kg_timeline`, `kg_invalidate`, `find_paths`) compile to native
  Cypher on AGE, which beats the sqlite recursive-CTE fallback by
  ≥30% at depth=5 on the canonical 1k-entity / 5k-edge corpus
  (S76 perf gate). The deeper the graph, the wider the gap.
- **Multi-daemon A2A.** Two or more `ai-memory serve` processes
  sharing the same store. Postgres is the supported topology;
  sqlite-over-NFS is not.

The two backends have **schema parity at v28** as of v0.7.0 — every
feature that works on sqlite works on postgres.

## Prerequisites

| Component | Version | Notes |
|---|---|---|
| PostgreSQL | 16.x (16.4+ recommended) | 15.x works for the SAL adapter but Apache AGE 1.5.x targets PG 16. |
| Apache AGE | 1.5.0 | Built from source against your PG 16. |
| pgvector | 0.7.x or 0.8.x | 0.8 is preferred (faster HNSW); 0.7 is fine. |
| ai-memory | v0.7.0 with `--features sal-postgres` | The sql-postgres feature is **off by default** to keep the no-postgres build hermetic. |

> **AGE 1.5 + PG 16 cypher-binding compat.** AGE 1.5.0 has a known
> quirk where parameter binding against `cypher()` plus PostgreSQL 16's
> stricter `prepare`-path causes a "could not find parameter $N"
> error in some bind shapes. ai-memory's production code interpolates
> parameters into the Cypher string through the AGE-recommended
> `cypher()` SQL-function form and never hits this — but if you write
> your own SQL probes, prefer the SQL-function form. Wave 1 Stream C
> fixed our equivalence test harness to use the safe form; production
> code never needed the fix.

## Install — Ubuntu 24.04 example

```bash
# 1. PostgreSQL 16 from PGDG.
sudo apt install -y curl ca-certificates gnupg lsb-release
sudo install -d /usr/share/postgresql-common/pgdg
sudo curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
     -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc
echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] \
     https://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" \
     | sudo tee /etc/apt/sources.list.d/pgdg.list
sudo apt update
sudo apt install -y postgresql-16 postgresql-server-dev-16 \
                    postgresql-contrib-16 build-essential bison flex git

# 2. pgvector 0.8.0 from the upstream release tag.
git clone --depth 1 --branch v0.8.0 https://github.com/pgvector/pgvector.git
cd pgvector
sudo make USE_PGXS=1 PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config install
cd ..

# 3. Apache AGE 1.5.0 from source against PG 16.
git clone --depth 1 --branch PG16/v1.5.0 https://github.com/apache/age.git
cd age
sudo make PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config install
cd ..

# 4. Restart postgres to pick up the shared libraries.
sudo systemctl restart postgresql@16-main
```

For RHEL / Fedora / Amazon Linux: replace the `apt` lines with the
PGDG yum repo equivalents and ensure `postgresql16-devel` /
`postgresql16-contrib` are installed before building AGE.

## Database setup

```bash
sudo -u postgres psql <<'SQL'
CREATE ROLE aimemory WITH LOGIN PASSWORD 'changeme-please';
CREATE DATABASE aimemory OWNER aimemory;
\c aimemory
CREATE EXTENSION IF NOT EXISTS age;
CREATE EXTENSION IF NOT EXISTS vector;
GRANT USAGE ON SCHEMA ag_catalog TO aimemory;
GRANT ALL ON ALL TABLES IN SCHEMA public TO aimemory;
ALTER DATABASE aimemory SET search_path = ag_catalog, "$user", public;
SQL
```

Notes:

- `aimemory` is the role the daemon runs as. Pick a strong password and
  store it in your secret manager — the daemon reads it from the
  `--store-url` URL or the `AI_MEMORY_STORE_URL` env var.
- `ag_catalog` must be on the search path so AGE's `cypher()` SQL
  function resolves without a schema prefix on every call.
- The role only needs `USAGE` on `ag_catalog` (read of the AGE
  function definitions); the AGE projection objects ai-memory creates
  live in the `aimemory` schema by default.

## Bootstrap the schema

`ai-memory schema-init` (Wave 1 Stream B deliverable) is the supported
way to bootstrap a fresh postgres backend:

```bash
ai-memory schema-init --store-url postgres://aimemory:changeme-please@localhost:5432/aimemory
```

What it does:

1. Connects to the `--store-url`.
2. Probes for `age` and `vector` extensions; refuses to proceed with a
   helpful error if either is missing.
3. Runs `src/store/postgres_schema.sql` (idempotent — `CREATE TABLE
   IF NOT EXISTS` throughout) up to the v28 schema marker.
4. If AGE is present, creates the AGE graph (`ai_memory_kg`) and
   primes the projection labels (entity, memory) and edge types
   (related_to, supersedes, contradicts, derived_from).
5. Records `schema_version=28` in the `_ai_memory_schema_version`
   table.

Idempotent on rerun — safe to invoke from a deploy script. Exit code
0 on success, 2 on missing prerequisites, 1 on transient connection
error.

If you'd rather not install the AGE projection (e.g. you don't need
KG queries), run `ai-memory schema-init --skip-age` and the recursive
CTE fallback will be used for `kg_query`/`kg_timeline`/etc.

> **Pre-Wave-1 fallback.** Until Wave 1 Stream B's commit lands, you
> can bootstrap by running the migration tool against a fresh empty
> postgres (which uses the same `INIT_SCHEMA` path internally) — see
> `migration-v0.7.0-postgres.md` for that workflow.

## Daemon configuration

Three ways to point the daemon at postgres, in precedence order:

1. **CLI flag (preferred for systemd):**
   ```bash
   ai-memory serve --store-url postgres://aimemory:PASSWORD@HOST:5432/aimemory
   ```
2. **Environment variable:**
   ```bash
   export AI_MEMORY_STORE_URL='postgres://aimemory:PASSWORD@HOST:5432/aimemory'
   ai-memory serve
   ```
3. **Config file** (`~/.config/ai-memory/config.toml`):
   ```toml
   [store]
   url = "postgres://aimemory:PASSWORD@HOST:5432/aimemory"
   ```

When `--store-url` is **unset**, the daemon falls back to its sqlite
path (`AI_MEMORY_DB` or `~/.local/share/ai-memory/memory.db`). This
preserves byte-for-byte v0.6.x behavior on the default build.

The daemon logs the resolved backend at startup:

```
INFO  ai_memory: store backend: PostgresStore
INFO  ai_memory: postgres schema: v28 (parity with sqlite)
INFO  ai_memory: KG backend: AGE 1.5.0 (Cypher path active)
```

If AGE is absent, the second line says `KG backend: CTE (recursive
fallback — install Apache AGE for ≥30% kg_query speedup)`.

## Operator surfaces that "just work" identically on both backends

The whole point of the SAL trait is that no caller needs to know
which backend is mounted. As of v0.7.0 (Wave 1+2 schema parity), the
following all behave identically across sqlite and postgres:

- `memory_store` / `memory_recall` / `memory_search` / `memory_list` /
  `memory_get` / `memory_delete` / `memory_promote` / `memory_link`
- `memory_consolidate` / `memory_check_duplicate` /
  `memory_detect_contradiction` / `memory_expand_query`
- `memory_kg_query` / `memory_kg_timeline` / `memory_kg_invalidate`
  / `memory_find_paths`
- `memory_subscribe` / `memory_list_subscriptions` /
  `memory_subscription_dlq_list` / `memory_subscription_replay`
- `memory_agent_register` / `memory_agent_list`
- `memory_namespace_get_standard` / `memory_namespace_set_standard`
  / `memory_namespace_clear_standard`
- `memory_archive_list` / `memory_archive_restore` /
  `memory_archive_purge`
- `memory_quota_status` / `memory_pending_list` /
  `memory_pending_approve` / `memory_pending_reject`
- The full audit chain (`signed_events`, `audit verify`, restart
  continuity).

The recall **score breakdown** is the same 6-factor formula on both
backends:

```
score = semantic_weight * cosine
      + (1 - semantic_weight) * fts_norm
      + priority * 0.05
      + access_count * 0.01
      + confidence * 0.20
      + tier_bonus
      + recency_factor
```

(Wave 1 Stream A's `tests/recall_scoring_parity.rs` pins this
contract — same query, same top-K, same per-factor breakdown
within FP tolerance.)

## Performance notes

### pgvector HNSW

The postgres adapter creates an HNSW index on `memories.embedding`
during `schema-init`:

```sql
CREATE INDEX IF NOT EXISTS memories_embedding_hnsw
    ON memories USING hnsw (embedding vector_cosine_ops);
```

Default tuning is the pgvector default (`m=16`, `ef_construction=64`).
For corpora >1M memories, raising `ef_construction` to 128 at index
build time and `hnsw.ef_search` to 80 at query time is the standard
recommendation; the v0.7.0 release does not yet expose these as
`ai-memory schema-init` flags — set them via SQL post-bootstrap.

### AGE Cypher vs CTE fallback

The four KG operations dispatch on the `KgBackend` tag the postgres
adapter probes at connect time:

| Op | AGE 1.5 (Cypher) | CTE fallback | Speedup at depth=5 |
|---|---|---|---|
| `kg_query` | `MATCH (a)-[*1..d]->(b) WHERE a.id = $1` | recursive `WITH` join | ≥30% (S76 gate) |
| `kg_timeline` | `MATCH ... WHERE valid_from < $1 AND (valid_until IS NULL OR valid_until > $1)` | recursive temporal join | ≥30% |
| `kg_invalidate` | `MATCH ... SET valid_until = $1` | `UPDATE memory_links` | parity |
| `find_paths` | `MATCH p = shortestPath((a)-[*1..d]->(b))` | recursive CTE with cycle detection | 2-5× at depth=5+ |

The S76 perf gate fires if AGE is reported as engaged but the AGE p95
is **not** at least 30% faster than CTE p95 on the canonical 1k-entity
/ 5k-edge corpus. That gate is honest about the AGE-vs-CTE comparison
on the **same** postgres host — comparing AGE-on-postgres to
CTE-on-sqlite is a different question and not the speedup we claim.

### Connection pool

The postgres adapter uses sqlx's connection pool. The v0.7.0 default
is min=2, max=16, idle-timeout=10min. For high-fanout multi-tenant
deployments, raise `max` to 32-64; for single-daemon deployments the
defaults are appropriate. Pool size is exposed via `AI_MEMORY_PG_POOL_MAX`
and `AI_MEMORY_PG_POOL_MIN` env vars.

## Troubleshooting

### "extension age is not installed"

`ai-memory schema-init` exits 2 if AGE isn't present. Either install
AGE (see "Install" above) or pass `--skip-age` to bootstrap with the
CTE fallback. The `vector` extension is **not** optional — pgvector
is required for embeddings.

### "schema_version=15 detected, expected 28"

You're pointing at a v0.7-alpha postgres database. Run `ai-memory
migrate --from postgres://… --to postgres://… --in-place` to apply
the v15→v28 migrations idempotently. (See
`migration-v0.7.0-postgres.md` for the full migration guide.)

### "could not find parameter $N" against a Cypher query

This is the AGE 1.5 + PG 16 binding quirk mentioned above. ai-memory's
production code never hits it — if you see it, you're running a
custom psql probe. Use the AGE-recommended SQL-function shape:

```sql
SELECT * FROM cypher('ai_memory_kg', $$
  MATCH (a)-[r:RELATED_TO]->(b)
  WHERE a.id = $node_id
  RETURN b
$$, $$ {"node_id": "abc123"} $$) AS (b agtype);
```

### "permission denied for schema ag_catalog"

The `aimemory` role needs `USAGE` on `ag_catalog`. The "Database
setup" section above grants it; if you bootstrapped by hand, run
`GRANT USAGE ON SCHEMA ag_catalog TO aimemory;` as the postgres
superuser.

### Recall scores differ between sqlite and postgres

If you observe this, file an issue and reference
`tests/recall_scoring_parity.rs` (Wave 1 Stream A). The contract is
that the same query returns the same top-K with the same per-factor
score breakdown within FP tolerance. Drift is a regression — the
parity test is the gate that prevents it.

## What's in scope vs out of scope (v0.7.0)

| | sqlite | postgres |
|---|---|---|
| Live daemon | ✓ (default) | ✓ (Wave 3) |
| Schema parity | v28 | v28 (Wave 2) |
| `link()` | ✓ | ✓ (Wave 1 Stream A) |
| `register_agent()` | ✓ | ✓ (Wave 1 Stream A) |
| Recall 6-factor scoring | ✓ | ✓ (Wave 1 Stream A) |
| `kg_query` / `kg_timeline` / `kg_invalidate` / `find_paths` | CTE | AGE Cypher (CTE fallback) |
| Migration tool both directions | ✓ | ✓ |
| `schema-init` CLI | n/a (auto-create) | ✓ (Wave 1 Stream B) |
| Cross-backend live replication | ✗ | ✗ (v0.7.1+) |

## References

- Apache AGE 1.5 docs: https://age.apache.org/
- pgvector 0.8 docs: https://github.com/pgvector/pgvector
- ai-memory v0.7.0 release notes: [`v0.7.0/release-notes.md`](v0.7.0/release-notes.md)
- A2A campaign Pages: https://alphaonedev.github.io/ai-memory-a2a-v0.7.0/
- Adapter-selection design: [`RUNBOOK-adapter-selection.md`](RUNBOOK-adapter-selection.md)
- Migration runbook: [`migration-v0.7.0-postgres.md`](migration-v0.7.0-postgres.md)
- F6 issue (in-flight Wave 1 closure): https://github.com/alphaonedev/ai-memory-mcp/issues/646
