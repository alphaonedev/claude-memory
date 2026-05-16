# Per-agent daily quotas (K8)

v0.7.0 ships per-agent daily quotas plus a structured read surface
(`memory_quota_status` MCP tool / quota status HTTP route). The
substrate is always-on: the K8 `check_and_record` call sits inline on
the `memory_store` and `memory_link` write paths and atomically
refuses any write that would breach the agent's compiled-default or
operator-tuned ceiling. There is no "advisory mode" — the safe
posture is enforced by default, and the compiled defaults are
deliberately generous so the substrate is invisible to small-scale
operations.

- **Code paths:** [`src/quotas.rs`](../src/quotas.rs),
  [`src/handlers/http.rs`](../src/handlers/http.rs) quota status handler,
  [`src/mcp/tools/quota_status.rs`](../src/mcp/tools/quota_status.rs).
- **Schema:** [`migrations/sqlite/0022_v07_agent_quotas.sql`](../migrations/sqlite/0022_v07_agent_quotas.sql).
- **MCP tool:** `memory_quota_status` in the Power family
  ([`src/mcp/registry.rs:1307`](../src/mcp/registry.rs)).

## Row shape

```sql
CREATE TABLE agent_quotas (
  agent_id                TEXT PRIMARY KEY,
  max_memories_per_day    INTEGER NOT NULL DEFAULT 1000,
  max_storage_bytes       INTEGER NOT NULL DEFAULT 104857600,   -- 100 MiB
  max_links_per_day       INTEGER NOT NULL DEFAULT 5000,
  current_memories_today  INTEGER NOT NULL DEFAULT 0,
  current_storage_bytes   INTEGER NOT NULL DEFAULT 0,
  current_links_today     INTEGER NOT NULL DEFAULT 0,
  day_started_at          TEXT NOT NULL,
  created_at              TEXT NOT NULL,
  updated_at              TEXT NOT NULL
);
```

A row is auto-inserted on the agent's first write (or first
`memory_quota_status` call) at compiled defaults. Operators set
per-agent caps via direct SQL — the K8 substrate is operator-mutated,
not MCP-mutated, by design (a malicious agent must not be able to
raise its own ceiling).

Compiled defaults
([`src/quotas.rs:31-40`](../src/quotas.rs)):

| Const | Value | Counter |
|---|---|---|
| `DEFAULT_MAX_MEMORIES_PER_DAY` | 1,000 | `current_memories_today` (resets at UTC midnight) |
| `DEFAULT_MAX_STORAGE_BYTES` | 100 MiB | `current_storage_bytes` (lifetime; does NOT reset) |
| `DEFAULT_MAX_LINKS_PER_DAY` | 5,000 | `current_links_today` (resets at UTC midnight) |

`max_storage_bytes` is a **lifetime** cap. The two `*_today` counters
are daily. The storage cap counts `len(title) + len(content) +
len(metadata)` per stored memory.

## Daily reset

`reset_daily` ([`src/quotas.rs:570`](../src/quotas.rs)) runs every UTC
midnight from the K8 sweep loop wired into
`daemon_runtime::bootstrap_serve`. It zeroes
`current_memories_today` and `current_links_today` on every row whose
`day_started_at` is no longer today, and bumps `day_started_at` to
the new bucket. `current_storage_bytes` is never zeroed.

Inline roll-over: `check_and_record`
([`src/quotas.rs:313`](../src/quotas.rs)) also performs an inline
daily-bucket roll inside the `BEGIN IMMEDIATE` transaction so the
per-write quota stays honest even if the sweeper hasn't fired yet.
Pinned by [`tests/k8_daily_reset.rs`](../tests/k8_daily_reset.rs).

## Enforcement semantics

Every `memory_store` / `memory_link` write calls `check_and_record`
([`src/quotas.rs:313`](../src/quotas.rs)) inside a `BEGIN IMMEDIATE`
SQLite transaction. The transaction acquires a `RESERVED` lock on the
database at the start, serializing every other would-be writer until
COMMIT/ROLLBACK — this is the SQLite analogue of
`SELECT ... FOR UPDATE` and closes the H12 race finding (a concurrent
write could otherwise pass the check and then both increment the
counter past the cap).

The three refusal shapes ([`src/quotas.rs:64-86`](../src/quotas.rs)):

- `QuotaLimit::MemoriesPerDay` — the pending memory_store would push
  `current_memories_today + 1 > max_memories_per_day`.
- `QuotaLimit::StorageBytes` — the pending memory_store's
  `(title + content + metadata)` byte length would push
  `current_storage_bytes + bytes > max_storage_bytes`.
- `QuotaLimit::LinksPerDay` — the pending link_create would push
  `current_links_today + 1 > max_links_per_day`.

Refusals raise `QuotaError`
([`src/quotas.rs:91`](../src/quotas.rs)) which the MCP / HTTP layers
map to the `QUOTA_EXCEEDED` diagnostic. The error envelope carries
`agent_id`, `limit` (lower-snake-case name), `current`, `max` — the
caller can render "you have used X/Y for today, reset at UTC midnight"
without a second round-trip. Pinned by
[`tests/k8_quota_enforcement.rs`](../tests/k8_quota_enforcement.rs).

If a downstream write fails *after* `check_and_record` succeeds,
`refund_op` ([`src/quotas.rs:456`](../src/quotas.rs)) reverses the
increment. The two-phase pattern: `check_and_record(...)?;
op(...)?;` and on op-failure, `refund_op(...)`.

## MCP tool wire shape

Request ([`src/mcp/tools/quota_status.rs`](../src/mcp/tools/quota_status.rs)):

```jsonc
{
  "tool": "memory_quota_status",
  "args": {
    "agent_id": "ai:claude-opus@host:pid-12345"   // optional
  }
}
```

When `agent_id` is provided, the handler returns a single envelope
(auto-inserting a default row if absent):

```jsonc
{
  "agent_id": "ai:claude-opus@host:pid-12345",
  "quota": {
    "agent_id": "ai:claude-opus@host:pid-12345",
    "max_memories_per_day": 1000,
    "max_storage_bytes": 104857600,
    "max_links_per_day": 5000,
    "current_memories_today": 142,
    "current_storage_bytes": 487331,
    "current_links_today": 21,
    "day_started_at": "2026-05-15",
    "created_at": "2026-05-15T03:14:22Z",
    "updated_at": "2026-05-15T09:18:07Z"
  }
}
```

When `agent_id` is omitted, the handler returns every row in the
substrate, sorted by `agent_id` ASC:

```jsonc
{ "count": 7, "quotas": [ /* QuotaStatus rows */ ] }
```

## HTTP wire shape

```bash
curl -X POST -H "Content-Type: application/json" \
  -H "X-API-Key: $API_KEY" \
  -H "X-Agent-Id: ai:claude-opus@host:pid-12345" \
  http://127.0.0.1:9077/api/v1/quota/status \
  -d '{}'
```

Same envelope as the MCP tool.

## Operator workflow

1. **Default install does nothing visible.** Compiled defaults are
   generous (1000 memories/day, 100 MiB lifetime, 5000 links/day). A
   small-to-medium operator never needs to touch the table.
2. **Observe.** Poll `memory_quota_status` weekly to see which agents
   are heaviest writers. Sort by `current_memories_today` /
   `current_storage_bytes` desc.
3. **Tighten** when a deployment has known scale targets. SQL:
   ```sql
   UPDATE agent_quotas
   SET max_memories_per_day = 100,
       max_storage_bytes    = 10 * 1024 * 1024,   -- 10 MiB
       max_links_per_day    = 500,
       updated_at           = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
   WHERE agent_id = 'ai:experimental-agent@host';
   ```
4. **Confirm** with `memory_quota_status` (single-agent form).
5. **Watch refusals** at `RUST_LOG=ai_memory::quotas=debug` — every
   refused write logs the agent_id, limit, current, and max.

## Tuning guidance

| Deployment shape | `max_memories_per_day` | `max_storage_bytes` | `max_links_per_day` | Rationale |
|---|---|---|---|---|
| Personal / single-operator | 1000 (default) | 100 MiB (default) | 5000 (default) | Compiled defaults; quota is a backstop, not a fence. |
| Small team (≤10 agents) | 1000-5000 | 100 MiB - 1 GiB | 5000-10000 | Bump per agent that hits the default ceiling regularly. |
| Multi-tenant SaaS | 100 per non-admin agent | 10 MiB per non-admin agent | 500 per non-admin agent | Tight by default; raise per-customer via SQL on contract upgrade. |
| Regulated tenant | per-agent SLO | per-agent SLO | per-agent SLO | Pin every quota row to the agent's contractual limit; alert on usage > 90%. |

**`max_storage_bytes` is lifetime.** Operators who want a rolling
storage budget should run a background job that archives + deletes
old memories, then calls a stored procedure (or SQL UPDATE) to
decrement `current_storage_bytes`. The substrate does not surface a
"reclaim on delete" path today — `current_storage_bytes` is
append-only against the lifetime of the row. (Tracking issue: the
substrate's storage byte accounting predates the L2-7 archive sweep
integration.)

**Per-agent tuning is the right granularity.** There is no
namespace-scoped or tenant-scoped quota row today. Multi-tenant
operators should pre-allocate agent_ids per tenant and tune each
agent's row.

## Anti-pattern examples

- **Anti-pattern A: "Quota as the only fence."** The K8 substrate is
  the substrate's last line of defense, not the first. A misbehaving
  agent that consumes its quota in 30 seconds will still consume its
  quota in 30 seconds. Pair K8 with K9 governance rules
  ([`docs/governance.md`](governance.md)) to refuse the *kind* of
  write that's pathological, not just the volume.

- **Anti-pattern B: "Raise the cap whenever an agent complains."**
  Compiled defaults are generous; if every agent in the deployment is
  asking for more, the substrate is being asked to do something it
  wasn't designed for. Consider whether the right answer is a
  different store (vector DB for embeddings-heavy, KV cache for
  short-lived state) rather than raising the ceiling.

- **Anti-pattern C: "Setting `max_memories_per_day = 0` to soft-block
  an agent."** Cleaner: revoke the agent's API key, or use K9 to deny
  the agent entirely. Setting the quota to 0 produces a
  `QUOTA_EXCEEDED` on every write, which masks the operator intent
  (was the agent rate-limited or banned?) in the diagnostic trail.

- **Anti-pattern D: "Treating `current_storage_bytes` as a cleanup
  signal."** The counter is append-only at the wire level. Deleting
  memories from the substrate does not decrement it. A storage-cap
  reset requires SQL.

## Monitoring + alerting setup

**Recommended metrics scrape** (per 60s):

- `quota_writes_refused_total{agent_id, limit}` — increment-since-boot
  count of `QUOTA_EXCEEDED` returns. Today this is surfaced via
  `RUST_LOG=ai_memory::quotas=debug`; a future task wires it into the
  doctor metrics surface.
- `quota_usage_ratio{agent_id, dimension}` — derived from
  `current_<X> / max_<X>` per row. Scrape via the MCP tool's list
  envelope.

**Alerting thresholds.**

| Severity | Condition | Recommended action |
|---|---|---|
| Info | `usage_ratio > 0.75` for 1h | Cap headroom notice; consider proactive bump if the agent's workload is legitimate. |
| Warning | `usage_ratio > 0.90` for 30m | Operator pages a human; either bump cap or rate-limit upstream caller. |
| Critical | `writes_refused_total` delta > 0 | Agent is currently being refused. Either bump cap, banish the agent, or accept the refusal as policy. |

**Dashboard panels** (in a typical Grafana/Datadog deployment):

- "Top 10 agents by `current_memories_today`" — table.
- "Top 10 agents by `current_storage_bytes`" — table.
- "Refusal rate per agent over time" — time series.
- "Daily reset transition" — single-stat that pulses at UTC midnight
  to confirm `reset_daily` is firing.

## Test coverage

- [`tests/k8_quota_status_tool.rs`](../tests/k8_quota_status_tool.rs)
  — MCP tool wire shape + auto-insert behavior.
- [`tests/k8_quota_enforcement.rs`](../tests/k8_quota_enforcement.rs)
  — refusal semantics under enforcement; the H12 atomic
  check-and-record race-closure.
- [`tests/k8_daily_reset.rs`](../tests/k8_daily_reset.rs) — midnight
  UTC partition rotation; inline roll-over via `check_and_record`.

## Troubleshooting

| Symptom | Likely cause | Diagnostic recipe |
|---|---|---|
| Agent's writes start failing with `QUOTA_EXCEEDED` | Agent hit one of three caps | `memory_quota_status` with the agent_id — the response shows which counter is at its max. |
| Counters not resetting at UTC midnight | Sweeper not running | Check daemon logs for `bootstrap_serve` quota loop activation. Inline check at first post-midnight write should still roll the bucket. |
| Quota row missing for an agent that just wrote | Race against auto-insert | `memory_quota_status` with the agent_id auto-inserts the default row idempotently; one MCP call closes the gap. |
| `current_storage_bytes` keeps climbing despite deletes | Lifetime counter, not rolling | Expected. Reset requires SQL UPDATE. |
| Two writes pass quota check then one fails downstream | Op-failure after `check_and_record` succeeded but `refund_op` not called | Inspect the calling code path; the pattern is `check_and_record(...)?; op(...)?;` and on op-failure `refund_op(...)`. |
| `check_and_record` returns SQL error | DB locked under contention | The `BEGIN IMMEDIATE` is the intended serialization point. Retry after backoff; chronic contention means the substrate is over-loaded. |

## Operator runbook (3am procedures)

**An agent is being refused every write and the operator can't reach
the agent's owner.** Either (a) bump the cap inline with SQL:
```sql
UPDATE agent_quotas
SET max_memories_per_day = current_memories_today + 100,
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE agent_id = '<id>';
```
or (b) accept the refusal and let the agent back off. The MCP error
carries enough detail for the agent's caller to render a useful UI;
in most cases letting it back off is the correct response.

**Daily reset appears not to have fired.** Inline reset via
`check_and_record` will still roll the bucket on the next write; the
sweeper is for the dormant-agent case (rows that haven't been touched
all day). If the sweeper is genuinely stuck, look for the sweep loop
warning in the daemon log; restart the daemon as a last resort.

**Suspected storage byte miscounting.** The counter is incremented in
the same transaction that inserts the memory, so under normal
operation it cannot drift. A persistent drift signals either an
out-of-band write (someone editing the SQLite file directly — never
do this) or a substrate bug (file an issue with the row diff and
`tests/k8_quota_enforcement.rs` reproduction). Recover by re-counting
from the live `memories` table:
```sql
UPDATE agent_quotas
SET current_storage_bytes = (
  SELECT COALESCE(SUM(LENGTH(title) + LENGTH(content) + LENGTH(metadata)), 0)
  FROM memories
  WHERE json_extract(metadata, '$.agent_id') = agent_quotas.agent_id
)
WHERE agent_id = '<id>';
```

See also: [`docs/MIGRATION_v0.7.md` §"K8 quota tool"](MIGRATION_v0.7.md),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: K8 quota status tool"](internal/v070-feature-inventory.md),
the K9 governance pipeline that complements quota with rule-based
refusal at [`docs/governance.md`](governance.md), the SSE-approvals
path that surfaces governance refusals to operators at
[`docs/k10-sse-approvals.md`](k10-sse-approvals.md), the hook pipeline
that gates pre-write decisions before quota check at
[`docs/hook-pipeline.md`](hook-pipeline.md), the federated peer that
quota-checks inbound writes per-peer at
[`docs/federation.md`](federation.md), and the signed-events chain
that records each quota refusal as an auditable event at
[`docs/signed-events-v4.md`](signed-events-v4.md).
