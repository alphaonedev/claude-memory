# K10 SSE approvals — `/api/v1/approvals/stream`

v0.7.0 ships a server-sent-events stream that surfaces pending-approval
lifecycle changes to listening operators (CLI tools, dashboards, A2A
governance peers). The stream pairs with the HMAC-signed `decide`
HTTP path so a hostile network observer who sees the SSE event stream
still cannot forge a decision.

- **Code paths:** [`src/approvals.rs`](../src/approvals.rs),
  [`src/handlers/transport.rs`](../src/handlers/transport.rs)
  (the `/api/v1/approvals/stream` route handler at line 455).
- **Schema:** [`migrations/sqlite/0015_v07_pending_action_timeouts.sql`](../migrations/sqlite/0015_v07_pending_action_timeouts.sql)
  + [`migrations/sqlite/0021_v07_a2a_correlation.sql`](../migrations/sqlite/0021_v07_a2a_correlation.sql).
- **Reconciliation security-sweep commits:**
  - `7496a6e` — K10 SSE `host:` prefix bypass closure.
  - `99ffacc` — K10 HMAC method+pending_id binding.
  - `a69325f` — K10 HMAC nonce single-use 300s window.
  - `d1f6c9f` — K10 SSE lagged-event count strip.

## Stream contract

```bash
curl -N -H "X-API-Key: $API_KEY" \
  http://127.0.0.1:9077/api/v1/approvals/stream
```

The stream emits one named event per state change. Frame names:

| Event | Frame body | Fires on |
|---|---|---|
| `approval_pending`  | `{id, agent_id, namespace, requested_at, ttl_secs, summary}` | New row inserted in `pending_actions`. |
| `approval_decided`  | `{id, decision, decided_by, decided_at}` | Operator or sweeper resolves a pending row. |
| `approval_timeout`  | `{id, ttl_secs}` | The 60s `default_timeout_seconds` sweeper expires a row. |
| `lagged`            | `{count}` (no per-event detail) | The subscriber dropped frames; reconnect to re-sync. |
| `heartbeat`         | empty | 30s liveness ping. |

The `lagged` event carries **only the count**, never the per-event
detail — closing the K10 lagged-event-leak finding (commit `d1f6c9f`).
Subscribers that see `lagged` must reconnect and re-fetch the pending
list via `GET /api/v1/pending`.

## HMAC binding on the decide path

The companion decide path:

```
POST /api/v1/pending/{id}/approve
POST /api/v1/pending/{id}/reject
```

requires three headers when `permissions.mode = "enforce"`:

- `X-HMAC-Signature: <hex SHA-256>` — HMAC of the canonical request.
- `X-HMAC-Nonce: <16+ char ascii>` — single-use within a 300s window.
- `X-HMAC-Timestamp: <unix seconds>` — request freshness clock.

The canonical request bound by the HMAC is:

```
<method>\n<pending_id>\n<nonce>\n<timestamp>\n<body sha256>
```

- **Method binding** (`99ffacc`) prevents a hostile observer from
  replaying an `approve` HMAC against the `reject` endpoint or vice
  versa.
- **pending_id binding** (`99ffacc`) prevents replay across pending
  actions.
- **Nonce single-use 300s window** (`a69325f`) prevents replay attacks
  even within the freshness window — a nonce is rejected on second
  use regardless of timestamp.
- **`host:` prefix bypass** (`7496a6e`) — the v0.7.0-alpha
  implementation accepted nonce values prefixed with `host:` as a
  free-pass. Closed in `7496a6e`.

Pinned by [`tests/k10_approval_security.rs`](../tests/k10_approval_security.rs),
[`tests/k10_approval_http.rs`](../tests/k10_approval_http.rs),
[`tests/k10_approval_sse.rs`](../tests/k10_approval_sse.rs),
[`tests/k10_remember_forever.rs`](../tests/k10_remember_forever.rs).

## Pairing with the MCP surface

The MCP tools `memory_pending_list` / `memory_pending_approve` /
`memory_pending_reject` are the stdio-side equivalents. The v0.7-alpha
drafts named these `memory_approval_pending` / `memory_approval_decide`;
**the shipped names are `memory_pending_*`** (see
[`src/mcp/registry.rs`](../src/mcp/registry.rs)). The MCP path uses
the same HMAC binding as the HTTP path when the substrate is in
`enforce` mode.

## Operator workflow

1. **Bring up the SSE consumer.** A tiny operator dashboard that
   subscribes to `/api/v1/approvals/stream`, renders the pending row
   with the substrate-supplied `summary` field, and prompts the human
   for approve/reject. Reference implementation lives in the
   `tools/post-ship-converge/` helper.
2. **Sign the decide.** Compute the canonical-request string above,
   HMAC with the operator secret, send the three `X-HMAC-*` headers.
3. **Verify** the operator-side log records the SSE `approval_decided`
   frame within one round-trip.

## `remember=forever` progressive trust

`POST /api/v1/pending/{id}/reject?remember=forever` (or
`memory_pending_reject(remember=forever)`) writes a permanent
deny-rule into the rule corpus, so the same action shape is auto-rejected
without re-prompting. The reverse `remember=forever` on `approve`
similarly writes a permanent allow. Use sparingly; pinned by
[`tests/k10_remember_forever.rs`](../tests/k10_remember_forever.rs).

See also: [`docs/governance.md`](governance.md) for the wider
permissions pipeline, [`docs/MIGRATION_v0.7.md` §"K10 SSE approvals"](MIGRATION_v0.7.md),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: K1/G1 namespace-inheritance"](internal/v070-feature-inventory.md).
