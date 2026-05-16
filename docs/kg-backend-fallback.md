# Knowledge-graph backend fallback (AGE → CTE)

> v0.7.0 fold-A2A1.3 (#700). Scenarios cleared: S45, S46, S65, S82.

The Postgres SAL adapter runs the four knowledge-graph endpoints
(`memory_kg_query`, `memory_kg_timeline`, `memory_kg_invalidate`,
`memory_find_paths`) through one of two backends:

| Backend | When it runs                                       | Source of truth        |
|---------|----------------------------------------------------|------------------------|
| `age`   | `pg_extension` reports the `age` extension at boot | `memory_graph` projection (property graph) |
| `cte`   | Always available on vanilla Postgres               | `memory_links` relational table |

The relational `memory_links` table is the durable source of truth in
both cases — every AGE write is mirrored back to it. The AGE branch
exists purely as the ~30% speedup path advertised in ROADMAP2 §7.4.4.

## Fallback contract

Two failure modes for the AGE branch:

1. **Boot-time absence.** `detect_kg_backend` probes `pg_extension`
   when the adapter connects. If the extension is missing — or the
   probe itself fails because the role can't read the catalog —
   `kg_backend` resolves to `Cte` and the dispatchers route exclusively
   to the relational walk for the lifetime of that adapter. Operators
   see this once at startup as an `info` line:

   ```
   INFO store::postgres connected — kg_backend=cte AGE extension not detected (proceeding with recursive CTE)
   ```

2. **Runtime AGE failure.** AGE was present at boot but a per-request
   cypher call fails: extension dropped between boot and now,
   projection missing or stale, `ag_catalog` permissions changed, or a
   transient pool/serialisation error. Before fold-A2A1.3 each of these
   surfaced as `StoreError::BackendUnavailable`, rendered by the
   handlers as a hard 503 on all four KG endpoints. After
   fold-A2A1.3 the dispatcher catches the AGE-side failure, emits a
   structured warning, and re-issues the request through the CTE
   branch. The HTTP/MCP caller sees a successful response — same shape
   as the AGE branch — and the operator sees the degraded-mode event
   in the daemon log.

The fallback is **per-request**, not latched: every subsequent KG call
tries AGE first. If the operator restores the extension, the next
request returns to the cypher path automatically. No daemon restart is
required. This is the "auto-retry on next request; don't crash daemon"
discipline.

### What gets logged

When the dispatcher falls back, it emits a `tracing::warn!` event on
the `store::postgres::kg` target with these structured fields:

| Field          | Value                                                 |
|----------------|-------------------------------------------------------|
| `op`           | `kg_query` / `kg_timeline` / `kg_invalidate` / `find_paths` |
| `source_id`    | The traversal start id                                |
| `target_id`    | Only for `find_paths` (its second id argument)        |
| `backend`      | `age`                                                 |
| `fallback`     | `cte`                                                 |
| `error`        | The underlying `StoreError` (rendered)                |

Example message body:

```
AGE backend unreachable; falling back to CTE for kg_query=<mem-abc-001>
```

A repeated stream of these warnings means AGE is durably down —
operator should investigate and restore. A single isolated warning may
just be a transient pool blip; the next request will retry AGE.

## Operator checks

### Quick health probe (psql)

A one-liner against the live database confirms whether the AGE
extension is present and whether a basic cypher call succeeds:

```sql
-- 1. Extension installed?
SELECT extname, extversion FROM pg_extension WHERE extname = 'age';

-- 2. Cypher round-trip works?
LOAD 'age';
SET search_path = ag_catalog, "$user", public;
SELECT * FROM cypher('memory_graph', $$ MATCH (n) RETURN count(n) $$) AS (n agtype);
```

The first query returns one row when AGE is installed. The second
fails with a clear error message when the extension is gone, when
`memory_graph` is missing, or when the role lacks `USAGE` on
`ag_catalog`.

### What to check when the warning fires

| Warning rate                | Likely cause                                              | Action                                   |
|-----------------------------|-----------------------------------------------------------|------------------------------------------|
| One isolated warning        | Transient pool error, query timeout                       | Watch for a follow-up — auto-retried     |
| Sustained on every request  | Extension dropped or graph projection lost                | `CREATE EXTENSION age` + `SELECT create_graph('memory_graph')` |
| Sustained on a single op    | Per-op projection drift (e.g. `find_paths` walker)        | Rebuild the projection from `memory_links` |
| Sustained at startup        | Role lacks permission on `ag_catalog`                     | Grant `USAGE` on `ag_catalog` + `pg_catalog` to the daemon role |

When the daemon is restarted with AGE still missing, the boot-time
probe resolves `kg_backend` to `Cte` and the runtime fallback never
fires — the warning stream stops by definition. The four endpoints
keep working off the relational table.

## Test coverage

Two test files pin the contract:

- **`tests/age_cte_equivalence.rs`** — AGE vs CTE *answer*
  equivalence: identical fixture, identical results.
- **`tests/kg_age_fallback.rs`** — runtime *fallback* behaviour:
  boots with AGE up, captures the baseline, then `DROP EXTENSION age
  CASCADE`s under the live store and asserts the dispatcher's
  AGE-routed call still returns the baseline answer (because it fell
  back to CTE). Requires `AI_MEMORY_TEST_AGE_URL` with a role that has
  `CREATE`/`DROP EXTENSION` privilege.

Both gates skip cleanly on machines without Postgres + AGE
configured.
