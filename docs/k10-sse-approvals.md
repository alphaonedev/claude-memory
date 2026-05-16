# K10 SSE approvals — `/api/v1/approvals/stream`

v0.7.0 ships a server-sent-events stream that surfaces pending-approval
lifecycle changes to listening operators (CLI tools, dashboards, A2A
governance peers). The stream pairs with the HMAC-signed decide path
so a hostile network observer who sees the SSE event stream still
cannot forge a decision. The HMAC binds method + URL + body + a
timestamp inside a 300-second replay window, and the signature itself
is consumed single-use to defeat replay within that window.

- **Code paths:** [`src/approvals.rs`](../src/approvals.rs),
  [`src/handlers/mod.rs:705`](../src/handlers/mod.rs)
  (the `approvals_sse` handler),
  [`src/handlers/mod.rs:352`](../src/handlers/mod.rs)
  (the `verify_approval_hmac` core),
  [`src/handlers/transport.rs:455`](../src/handlers/transport.rs)
  (the `/api/v1/approvals/stream` route matcher),
  [`src/lib.rs:359`](../src/lib.rs) (route registration).
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
     -H "X-Agent-Id: ai:dashboard@host" \
     http://127.0.0.1:9077/api/v1/approvals/stream
```

The stream emits one named event per state change. Frame names
([`src/handlers/mod.rs:705-806`](../src/handlers/mod.rs)):

| Event | Frame body | Fires on |
|---|---|---|
| `approval_requested` | JSON of `ApprovalEvent::ApprovalRequested` (pending_id, agent_id, namespace, summary, ttl) | New row inserted in `pending_actions`. |
| `approval_decided`   | JSON of `ApprovalEvent::ApprovalDecided` (pending_id, decision, decided_by, decided_at) | Operator or sweeper resolves a pending row. |
| `lagged`             | `{"lagged": true}` (no per-event detail) | The subscriber dropped frames; reconnect to re-sync. |

A keepalive comment line fires every 15s
([`src/handlers/mod.rs:805`](../src/handlers/mod.rs)) to prevent
intermediary timeouts. The stream is intentionally unauthenticated
beyond `api_key_auth` middleware — SSE re-key handshakes are clunky,
and the HMAC gate sits on the **write** side (the decide endpoint),
not the read side.

The `lagged` event carries **only the boolean flag**, never the
per-event count — closing the K10 lagged-event-leak finding (commit
`d1f6c9f`, [`src/handlers/mod.rs:785-794`](../src/handlers/mod.rs)).
The count would leak cross-tenant traffic volume to a noisy-neighbour
subscriber. Subscribers that see `lagged` must reconnect and re-fetch
the pending list via `GET /api/v1/pending`.

### Subscriber agent_id resolution

The subscriber's `agent_id` is captured at subscribe time from the
`X-Agent-Id` header
([`src/handlers/mod.rs:733`](../src/handlers/mod.rs)) and every event
is filtered through `sse_event_visible_to`
([`src/handlers/mod.rs:649`](../src/handlers/mod.rs)) before fan-out.
Cross-tenant events are silently dropped — the subscriber sees only
their own pending rows and decisions, plus rows in namespaces an
active K9 `Allow` rule grants them.

**`host:` prefix bypass closure** (commit `7496a6e`):
self-asserted `host:`-prefixed agent_ids are rejected at the
handshake. `host:` is the server-side fallback identifier produced by
`identity::resolve_agent_id`; it must never be accepted from an
external client. A client passing `X-Agent-Id: host:…` is treated as
anonymous (empty subscriber_agent → fail-closed; every event is
filtered out). Pinned by
[`tests/k10_approval_sse.rs`](../tests/k10_approval_sse.rs).

## HMAC binding on the decide path

The companion decide path:

```
POST /api/v1/approvals/{pending_id}
```

with body `{"decision": "approve|deny", "remember": "once|session|forever"}`,
requires two headers when the substrate has an
`[hooks.subscription].hmac_secret` configured:

- `X-AI-Memory-Signature: sha256=<hex>` — HMAC-SHA256 over the
  canonical request, keyed on `SHA256(hmac_secret)`.
- `X-AI-Memory-Timestamp: <unix seconds>` — request freshness clock.

The canonical request that the HMAC covers is
([`src/handlers/mod.rs:344-345, 409`](../src/handlers/mod.rs)):

```text
canonical = "<unix_ts>.<METHOD>.<pending_id>.<body>"
```

Both signer and verifier MUST use the exact same join. Reformatting
even a single byte of the body invalidates the signature.

### Why every component is bound

- **Method binding** (`99ffacc`) prevents a hostile observer from
  replaying an `approve` HMAC against a future `reject` endpoint (or
  vice versa). The pending-row decision is `decision: approve|deny`
  inside the body so the binding is to METHOD = POST, but the body's
  decision field is HMAC-covered as part of the body content.
- **pending_id binding** (`99ffacc`) prevents replay across pending
  actions — a captured signature for pending row A cannot be
  redirected to pending row B by changing the URL.
- **Body binding** prevents post-hoc decision flipping. Captured
  signature must replay the exact body that was signed.
- **Timestamp + 300s window**
  ([`src/handlers/mod.rs:308`](../src/handlers/mod.rs),
  `APPROVAL_HMAC_MAX_AGE_SECS`) bounds the replay window. The 60s
  future-skew tolerance
  ([`src/handlers/mod.rs:314`](../src/handlers/mod.rs),
  `APPROVAL_HMAC_MAX_SKEW_SECS`) absorbs NTP drift without admitting
  forged-future-dated signatures.
- **Nonce single-use within window** (`a69325f`,
  [`src/handlers/mod.rs:422-447`](../src/handlers/mod.rs)) — the
  signature hex itself is recorded in a process-wide replay cache for
  600s (`APPROVAL_HMAC_MAX_AGE_SECS * 2`). A captured signature
  cannot be replayed even within the freshness window. Entries expire
  after the window so the cache doesn't grow unboundedly.

When no `[hooks.subscription].hmac_secret` is configured, the decide
endpoint **rejects every request** with 401
([`src/handlers/mod.rs:358-368`](../src/handlers/mod.rs)). The K10
contract is strict by default — better to refuse a write than to
accept an unauthenticated one.

Pinned by [`tests/k10_approval_security.rs`](../tests/k10_approval_security.rs),
[`tests/k10_approval_http.rs`](../tests/k10_approval_http.rs),
[`tests/k10_approval_sse.rs`](../tests/k10_approval_sse.rs),
[`tests/k10_remember_forever.rs`](../tests/k10_remember_forever.rs).

## Full HMAC binding walkthrough

Suppose the operator wants to approve pending action `pa-12345` with
`remember=session`. Body:

```json
{"decision":"approve","remember":"session"}
```

Step 1. Compute current Unix timestamp:

```bash
TS=$(date +%s)            # e.g. 1747300800
PENDING_ID=pa-12345
BODY='{"decision":"approve","remember":"session"}'
```

Step 2. Build the canonical preimage:

```bash
CANONICAL="${TS}.POST.${PENDING_ID}.${BODY}"
# "1747300800.POST.pa-12345.{\"decision\":\"approve\",\"remember\":\"session\"}"
```

Step 3. Compute the key (`SHA256(secret)`) and the signature:

```bash
SECRET="$(cat /etc/ai-memory/hmac.secret)"
KEY_HEX=$(printf '%s' "$SECRET" | openssl dgst -sha256 -hex | awk '{print $2}')
SIG=$(printf '%s' "$CANONICAL" | openssl dgst -sha256 -hmac "$KEY_HEX" -hex | awk '{print $2}')
```

Step 4. Send the request:

```bash
curl -X POST \
  -H "X-API-Key: $API_KEY" \
  -H "X-AI-Memory-Timestamp: $TS" \
  -H "X-AI-Memory-Signature: sha256=$SIG" \
  -H "Content-Type: application/json" \
  --data-binary "$BODY" \
  "http://127.0.0.1:9077/api/v1/approvals/${PENDING_ID}"
```

Step 5. Verify with the SSE consumer — within one round-trip the
stream emits an `approval_decided` frame whose `pending_id` matches.

The mirror outbound construction
([`src/config.rs:2589`](../src/config.rs)) is what
`[hooks.subscription]` peers use to sign their own requests; signers
must produce byte-identical canonical strings or the verify will
401.

## Replay-window operator tuning

The 300s / 60s constants are intentionally hardcoded — they mirror
AWS SigV4 and Stripe webhook windows and have been validated against
both NTP drift and exfiltration windows. They are NOT operator-tunable
via config today. The constants are visible at
[`src/handlers/mod.rs:308-314`](../src/handlers/mod.rs).

**Why 300s.** Long enough to absorb client-side retry jitter
(network blip, queue lag), short enough that an exfiltrated
signature expires before an attacker can weaponise it through the
typical operator-incident-response pipeline.

**Why 60s future-skew.** NTP drift on a healthy host is sub-second;
60s tolerates a misconfigured-NTP client without admitting deliberate
future-dating attacks.

**What if the operator's clock is genuinely off by more than 60s.**
The signer (operator's tooling) is the one whose clock matters —
the timestamp is the signer-claimed value. If the daemon's clock is
ahead by >60s, every legitimate signed request will 401 with
`stale signature`. Fix the host clock; this is not a constants
problem.

**Nonce cache memory.** The replay cache is in-process and bounded by
`APPROVAL_HMAC_MAX_AGE_SECS * 2 = 600s` of signature hexes. At a
sustained 10 ops/s the cache holds 6,000 entries (~400 KiB at 64
bytes per hex string + overhead). No operator action required.

## Pairing with the MCP surface

The MCP tools `memory_pending_list` / `memory_pending_approve` /
`memory_pending_reject` are the stdio-side equivalents. The v0.7-alpha
drafts named these `memory_approval_pending` /
`memory_approval_decide`; **the shipped names are `memory_pending_*`**
(see [`src/mcp/registry.rs`](../src/mcp/registry.rs)). The MCP path
uses the same HMAC binding as the HTTP path when the substrate is in
`enforce` mode.

## SSE client implementation guide

A minimum-viable operator dashboard:

```python
# pip install httpx-sse
import httpx, json
from httpx_sse import connect_sse

with httpx.Client(timeout=None) as client:
    headers = {
        "X-API-Key": API_KEY,
        "X-Agent-Id": "ai:dashboard@host",   # NOT host:-prefixed
    }
    with connect_sse(client, "GET",
                     "http://127.0.0.1:9077/api/v1/approvals/stream",
                     headers=headers) as event_source:
        for sse in event_source.iter_sse():
            if sse.event == "approval_requested":
                pending = json.loads(sse.data)
                render_prompt(pending)   # operator UI
            elif sse.event == "approval_decided":
                decided = json.loads(sse.data)
                mark_resolved(decided["pending_id"])
            elif sse.event == "lagged":
                # Re-sync via the snapshot endpoint
                refetch_via_get_pending()
```

**Reconnect policy.** Treat the SSE stream as best-effort. On
disconnect, reconnect with exponential backoff (1s → 30s cap) and
re-fetch `GET /api/v1/pending` to backfill anything missed.

**Heartbeat handling.** The 15s keepalive arrives as an SSE comment
line (`:keepalive`), which httpx-sse silently consumes. If your SSE
library surfaces comments, ignore them.

**Subscriber identification.** Pass a stable, non-`host:`-prefixed
`X-Agent-Id` per dashboard instance. Different dashboards see the same
events (each gets its own subscription); tenant isolation is enforced
per-event via `sse_event_visible_to`, not per-subscriber.

**Sign-then-send for the decide.** Reuse the canonical-request
construction from the walkthrough above. Validate the response is
2xx; on 401, log the reason (most likely stale timestamp or replay-
cache hit) and retry with a fresh timestamp.

## `remember=forever` progressive trust

`POST /api/v1/approvals/{pending_id}` with body
`{"decision":"deny", "remember":"forever"}` (or
`memory_pending_reject(remember=forever)`) writes a permanent
deny-rule into the rule corpus via
`record_synthetic_rule`
([`src/approvals.rs:115`](../src/approvals.rs)), so the same action
shape is auto-rejected without re-prompting. The reverse — `approve`
+ `remember=forever` — similarly writes a permanent allow. Use
sparingly; pinned by
[`tests/k10_remember_forever.rs`](../tests/k10_remember_forever.rs).

`Remember` variants ([`src/approvals.rs:71`](../src/approvals.rs)):

| Variant | Effect |
|---|---|
| `once` (default) | Decision applies to this row only. |
| `session` | Decision applies until the agent's session ends. |
| `forever` | Decision is written to the persistent rule corpus. |

## Tuning guidance

| Deployment shape | SSE subscribers | Decide cadence | Notes |
|---|---|---|---|
| Single-operator dev | 1 (CLI) | ad-hoc | Default `APPROVAL_BROADCAST_CAPACITY = 1024` is overkill; no tuning needed. |
| Team dashboard | 2-10 | tens/min | Per-subscriber tenant filter handles isolation; no quota knobs. |
| Multi-tenant SaaS | 10-100 | hundreds/min | Watch for `lagged` frames — every subscriber sees every event before filtering. If lag is chronic, shard tenants across daemons. |
| Compliance gate | 1-5 (with audit) | low (every write) | `fail_mode = "closed"` on the upstream K9 hooks so denied writes raise visible refusals. |

`APPROVAL_BROADCAST_CAPACITY` ([`src/approvals.rs:50`](../src/approvals.rs))
is the broadcast-channel capacity (currently 1024). A slow SSE
subscriber that exceeds the channel depth triggers `lagged`, never
loses correctness (it must reconnect-and-resync), but does cause a
brief miss-window. Raising the capacity helps slow subscribers
survive transient back-pressure; lowering it does not save memory in
practice (the channel grows on demand).

## Troubleshooting

| Symptom | Likely cause | Diagnostic recipe |
|---|---|---|
| Decide returns 401 with no logs | No HMAC secret configured | Check `[hooks.subscription].hmac_secret` in config.toml; daemon logs `no [hooks.subscription].hmac_secret configured` on every refused decide. |
| Decide returns 401 `stale signature` | Timestamp older than 300s OR signer/daemon clock skew | Check `date` on both hosts; re-sign with current `$(date +%s)`. |
| Decide returns 401 `future-dated signature` | Signer clock is >60s ahead | Fix signer NTP. |
| Decide returns 401 with valid-looking signature | Canonical request mismatch (body reformatted, wrong method, wrong pending_id) OR signature replayed | Recompute `<ts>.POST.<id>.<body>` byte-exact. If it still 401s and the timestamp is fresh, the signature already hit the replay cache; generate a fresh signature. |
| SSE stream emits no events for one tenant | Subscriber agent_id wasn't recognized (host:-prefixed or empty) | Check the SSE handshake's `X-Agent-Id`. Tenant-filtered events are silently dropped from cross-tenant subscribers. |
| `lagged` frames every few seconds | Subscriber too slow OR cross-tenant fan-out volume too high | Profile the subscriber; reconnect and re-fetch. If chronic, consider sharding tenants across daemons. |
| SSE connection drops every minute | Intermediary timeout | Check intermediary keepalive; the substrate sends `:keepalive` every 15s. |

## Operator runbook (3am procedures)

**Approvals not flowing — SSE silent, decide endpoint returns 401.**
Most likely the HMAC secret was rotated and the signer wasn't
updated. Check `[hooks.subscription].hmac_secret` in config.toml; the
signer's secret must match. Restore the previous secret (or
distribute the new one), restart the daemon, validate with a fresh
test signature.

**Suspected captured-signature replay.** Inspect daemon log for
`K10 approval rejected: stale signature` AND
`K10 approval rejected: nonce already used` (the latter is the
replay-cache hit). A pattern of refused-replay attempts on a single
pending_id strongly suggests a captured signature; rotate the HMAC
secret immediately and audit the `signed_events` chain for any
matching write that did succeed before the rotation.

**Need to bulk-approve while SSE pipeline is down.** Use the MCP
`memory_pending_approve` tool over stdio — same HMAC binding, but
no SSE round-trip required. The substrate writes the decision and
re-broadcasts on the SSE channel for any other subscribers, so
recovery is observable as soon as the SSE consumer reconnects.

**Lagged events cascading on a noisy tenant.** Identify the
high-volume tenant via the daemon log (every published event has the
tenant_agent_id at WARN). Either shard the tenant to its own daemon
or raise `APPROVAL_BROADCAST_CAPACITY` and ship.

See also: [`docs/governance.md`](governance.md) for the wider
permissions pipeline, [`docs/MIGRATION_v0.7.md` §"K10 SSE approvals"](MIGRATION_v0.7.md),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: K1/G1 namespace-inheritance"](internal/v070-feature-inventory.md),
the hook pipeline that emits `AskUser` decisions feeding the
approvals queue at [`docs/hook-pipeline.md`](hook-pipeline.md), the
signed-events chain that records every approval as an append-only
audit row at [`docs/signed-events-v4.md`](signed-events-v4.md), the
K8 quotas substrate that paired-blocks an over-quota agent's
pending-action queue at [`docs/k8-quotas.md`](k8-quotas.md), the
federation hardening that propagates approval decisions to peers at
[`docs/federation.md`](federation.md), and the sidechain transcripts
that capture the context of each approved write at
[`docs/sidechain-transcripts.md`](sidechain-transcripts.md).
