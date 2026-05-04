# `ai-memory doctor` — operator health dashboard

**Phase P7 / R7** (v0.6.3.1). Read-only health-and-fitness report for an
ai-memory deployment. Runs against a local SQLite DB or, with `--remote`,
against a live `ai-memory serve` daemon (the **fleet doctor** mode at T3+).

## Quick start

```bash
# Inspect the local DB. Default path is the same one `ai-memory store`
# writes to.
ai-memory doctor

# Read from a non-default DB.
ai-memory --db /var/lib/ai-memory/store.db doctor

# Machine-readable output for CI / scripting.
ai-memory doctor --json | jq '.sections[] | select(.severity == "critical")'

# Treat warnings as failures (useful for `pre-commit` / CI gates).
ai-memory doctor --fail-on-warn

# Fleet doctor — read a remote daemon's capabilities + stats endpoints.
ai-memory doctor --remote https://node-a.example.com:9077

# Combine: JSON + remote, no DB lookup at all.
ai-memory doctor --remote https://node-a.example.com:9077 --json
```

## Exit codes

| Code | Meaning |
|------|---------|
| `0`  | Healthy. No critical findings. (Warnings allowed unless `--fail-on-warn`.) |
| `1`  | At least one warning, with `--fail-on-warn` set. |
| `2`  | At least one critical finding. Always returned regardless of flags. |

Suitable for shell-style branching:

```bash
if ! ai-memory doctor --fail-on-warn; then
    pagerduty-cli incident create --service ai-memory --severity warn
fi
```

## Report sections

Each section carries a severity (`INFO`, `WARN`, `CRIT`, `N/A`) and a list
of `(key, value)` facts. The overall report severity is the max across
sections.

### Storage

- `total_memories`, `expiring_within_1h`, `links`, `db_size_bytes`
- per-tier and per-namespace counts
- `dim_violations` — count of memories whose `embedding_dim` disagrees
  with their namespace's modal dim. **Critical** when > 0.
  - On pre-P2 schemas the column doesn't exist; the field renders as
    `not_observed (pre-P2 schema)` with no severity bump.

### Index

- `hnsw_size_estimate` — count of memories with a non-null embedding
  (proxy for the in-memory HNSW index size).
- `cold_start_rebuild_secs_estimate` — rough estimate of daemon-restart
  cost at the canonical 50k inserts/sec rate.
- `index_evictions_total` — eviction count from `MAX_ENTRIES = 100_000`.
  **Critical** when > 0 once P3 wires the counter; `not_observed` until
  then. The doctor still raises a **warning** when `hnsw_size >= 95k`
  as a forward-leaning hint.

### Recall

- `recall_mode_distribution` — distribution of `hybrid` vs `keyword_only`
  vs `degraded` over the rolling window. `not_observed` until P3 lands.
- `reranker_used_distribution` — distribution of `neural` vs
  `lexical_fallback` vs `off`. `not_observed` until P3 lands.
- `--remote` mode reports the live `recall_mode_active` and
  `reranker_active` from Capabilities v2 (P1) instead.

### Governance

- `namespaces_with_policy` / `namespaces_without_policy` — count of
  namespaces with a registered standard whose
  `metadata.governance` block is non-null.
- `inheritance_depth` — histogram of `parent_namespace` chain depths
  across `namespace_meta` rows (`d0=N,d1=N,...`).
- `oldest_pending_age_secs` — age of the oldest `pending_actions` row
  in `pending` status. **Critical** when > 86400 (24h).
- `pending_actions_total` — count of `pending` rows.

### Sync

- `peer_count` — distinct `(agent_id, peer_id)` rows in `sync_state`.
- `max_skew_secs` — max `|last_seen_at - last_pulled_at|` across peers.
  **Critical** when > 600s.
- `N/A` when no peers are registered (single-node deployment).

### Webhook

- `subscription_count` — rows in `subscriptions`.
- `dispatched_total` / `failed_total` — lifetime totals from the
  `dispatch_count` / `failure_count` columns.
- `success_rate_pct` — `(dispatched - failed) / dispatched * 100`.
  **Warning** when < 95% over the lifetime totals (P5 will refine this
  to a rolling-1h window).

### Capabilities

- In `--remote` mode: queries `/api/v1/capabilities` and reports
  `schema_version`, `recall_mode_active`, `reranker_active`. Bumps to
  **Warning** when the daemon reports `recall_mode_active != hybrid`
  on a tier (`semantic` / `smart` / `autonomous`) that should support
  it (silent-degradation signal from P1).
- In local mode: `N/A` — the local doctor doesn't construct a
  TierConfig. Use `--remote http://localhost:9077` for the live read.

## Severity rules (initial)

| Severity | Trigger |
|----------|---------|
| **Critical** | `dim_violations > 0`; pending action older than 24h; sync skew > 600s; HNSW evictions > 0 |
| **Warning**  | Capabilities v2 reports a silent-degrade flag (`recall_mode_active != hybrid` on a capable tier); subscription delivery success < 95% |
| **Info**     | Anything else worth surfacing |
| **N/A**      | The section can't be queried in this mode (raw SQL section in `--remote`, P2/P3-only fields on a pre-P2/P3 schema) |

## Remote (fleet) mode

`--remote <url>` queries the daemon's existing HTTP surfaces:

- `GET /api/v1/capabilities` — the full Capabilities v2 (P1) JSON.
- `GET /api/v1/stats` — total memories, expiring soon, link count.

Sections that need raw SQL access (`Index`, `Governance`, `Sync`,
`Webhook`) render as **N/A** in remote mode. T3+ deployments will gain
a `/api/v1/doctor` endpoint that returns those sections directly so the
fleet doctor stops being read-by-API-only — tracked under R7.

Example fleet sweep:

```bash
for node in node-a node-b node-c; do
    echo "=== $node ==="
    ai-memory doctor --remote "https://$node.example.com:9077" --json \
        | jq '{node: "'"$node"'", overall, criticals: [.sections[] | select(.severity == "critical") | .name]}'
done
```

## What's stubbed pending P1/P2/P3

The doctor ships a working baseline against the v0.6.3 surface set. The
following fields render as `not_observed (pre-PX surface)` until those
phases land:

| Field | Lands with |
|-------|------------|
| `dim_violations` (numeric value > 0) | P2 — `embedding_dim` column |
| `index_evictions_total` (numeric value) | P3 — eviction counter |
| `recall_mode_distribution` (rolling window) | P3 — recall_mode counter |
| `reranker_used_distribution` (rolling window) | P3 — reranker counter |
| `recall_mode_active` (in `--remote`) | P1 — Capabilities v2 |
| `reranker_active` (in `--remote`) | P1 — Capabilities v2 |

When any of those phases merges, the doctor wires up automatically
without further code changes — every consumer is gated on schema /
field presence and falls back gracefully when absent.

## Anti-goals (per spec)

- The doctor does not introduce new monitoring infrastructure (no
  Prometheus, OTel exporters). It reads existing surfaces only.
- The doctor never writes to the database. Read-only.
- The doctor uses indexed `COUNT(*)` queries to keep its DB-lock window
  sub-millisecond on a populated store.
