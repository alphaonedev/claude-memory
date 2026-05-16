# Per-agent daily quotas (K8)

v0.7.0 ships per-agent daily quotas plus a structured read surface
(`memory_quota_status` MCP tool / `POST /api/v1/quota/status` HTTP
route). Default is "track, surface on demand, do not deny" — operators
opt in to enforcement via the `enforce` mode on the agent_quotas row.

- **Code paths:** [`src/quotas.rs`](../src/quotas.rs),
  [`src/handlers/http.rs`](../src/handlers/http.rs) `quota_status_handler`,
  [`src/mcp/tools/quota_status.rs`](../src/mcp/tools/quota_status.rs).
- **Schema:** [`migrations/sqlite/0022_v07_agent_quotas.sql`](../migrations/sqlite/0022_v07_agent_quotas.sql).
- **HTTP route:** `POST /api/v1/quota/status` (see
  [`docs/API_REFERENCE.md` §"v0.7.0 net-new endpoints"](API_REFERENCE.md#v070-net-new-endpoints)).
- **MCP tool:** `memory_quota_status` in the Power family.

## Row shape

```
CREATE TABLE agent_quotas (
  agent_id       TEXT NOT NULL,
  date_utc       TEXT NOT NULL,            -- YYYY-MM-DD
  writes_count   INTEGER NOT NULL DEFAULT 0,
  bytes_written  INTEGER NOT NULL DEFAULT 0,
  writes_limit   INTEGER,                  -- NULL = unlimited
  bytes_limit    INTEGER,                  -- NULL = unlimited
  enforce        INTEGER NOT NULL DEFAULT 0,   -- 0 = advisory, 1 = enforce
  PRIMARY KEY (agent_id, date_utc)
);
```

A row is auto-inserted on first `memory_quota_status` call for an
agent that has never been seen — `writes_limit = NULL` and
`bytes_limit = NULL` mean "no cap" by default. Operators set caps via
direct SQL (substrate-level write; not an MCP-mutable surface).

## Daily reset

A background task at midnight UTC rotates the date partition.
Cumulative state for prior days is preserved (operator-side
observability), but `writes_count` and `bytes_written` reset to 0 for
the new `date_utc`. Pinned by
[`tests/k8_daily_reset.rs`](../tests/k8_daily_reset.rs).

## Enforcement semantics

When `enforce = 1`:

- A `memory_store` write that would push `writes_count + 1 > writes_limit`
  is refused at the substrate boundary with `QuotaExceeded`.
- A `memory_store` write whose serialized payload would push
  `bytes_written + payload_size > bytes_limit` is refused with
  `QuotaExceeded`.
- Refusals carry the surfaced quota row so the caller can render
  "you have used X/Y for today, reset at UTC midnight" without a
  second round-trip.

When `enforce = 0` (default), the substrate increments the counters
but never refuses on quota. Pinned by
[`tests/k8_quota_enforcement.rs`](../tests/k8_quota_enforcement.rs).

## MCP tool wire shape

Request:

```jsonc
{
  "tool": "memory_quota_status",
  "args": {
    "agent_id": "ai:claude-opus@host:pid-12345"   // optional; defaults to caller
  }
}
```

Response:

```jsonc
{
  "agent_id": "ai:claude-opus@host:pid-12345",
  "date_utc": "2026-05-15",
  "writes_count": 142,
  "bytes_written": 487331,
  "writes_limit": 1000,
  "bytes_limit": 10485760,
  "enforce": true,
  "remaining_writes": 858,
  "remaining_bytes": 9998429,
  "resets_at": "2026-05-16T00:00:00Z"
}
```

## HTTP wire shape

```bash
curl -X POST -H "Content-Type: application/json" \
  -H "X-API-Key: $API_KEY" \
  -H "X-Agent-Id: ai:claude-opus@host:pid-12345" \
  http://127.0.0.1:9077/api/v1/quota/status \
  -d '{}'
```

Same response shape as the MCP tool.

## Operator workflow

1. **Observe.** Default install tracks counters but does not enforce.
   Watch `memory_quota_status` for the agents with the heaviest write
   volume.
2. **Set caps.** When you decide on a per-agent budget, set
   `writes_limit` / `bytes_limit` via SQL (the operator-only mutation
   surface — there is no MCP write path on `agent_quotas` by design).
3. **Flip enforcement.** `UPDATE agent_quotas SET enforce = 1 WHERE
   agent_id = '…' AND date_utc = '…'`. Or set a default by adding the
   row to your config bootstrap.
4. **Watch refusals** at `RUST_LOG=ai_memory::quotas=debug` — every
   refused write logs the row that caused the cap.

## Test coverage

- [`tests/k8_quota_status_tool.rs`](../tests/k8_quota_status_tool.rs)
  — MCP tool wire shape + auto-insert behavior.
- [`tests/k8_quota_enforcement.rs`](../tests/k8_quota_enforcement.rs)
  — refusal semantics under `enforce = 1`.
- [`tests/k8_daily_reset.rs`](../tests/k8_daily_reset.rs) — midnight
  UTC partition rotation.

See also: [`docs/MIGRATION_v0.7.md` §"K8 quota tool"](MIGRATION_v0.7.md),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: K8 quota status tool"](internal/v070-feature-inventory.md).
