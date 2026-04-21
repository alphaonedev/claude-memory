# ai-memory HTTP API Reference

Complete reference for every endpoint the `ai-memory serve` daemon
exposes. All endpoints are prefixed with `/api/v1/` unless noted.

## Base URL

Default: `http://127.0.0.1:9077`.

Configure via `ai-memory serve --host <host> --port <port>`. Production
deployments should always bind TLS: `--tls-cert` + `--tls-key`.

## Authentication

### API key

When `--api-key <key>` is set (or `api_key = "…"` in config), every
endpoint except `/api/v1/health` requires one of:

- Header: `X-API-Key: <key>`
- Query parameter: `?api_key=<key>`

Failure → **401** `{"error": "missing or invalid API key"}`.

### Agent identity — `X-Agent-Id`

Optional on writes. Identifies the caller for governance + attribution.

```
X-Agent-Id: ai:claude-opus-4.7@host.local
X-Agent-Id: alice
X-Agent-Id: host:prod-web-01:pid-12345-a1b2c3d4
```

Resolution precedence (write paths):

1. `agent_id` field in request body.
2. `X-Agent-Id` header.
3. Per-request anonymous id (`anonymous:req-<uuid8>`).

Validation pattern: `^[A-Za-z0-9_\-:@./]{1,128}$`.

### mTLS (Layer 2 peer mesh)

When `--mtls-allowlist` is set, every TCP connection must present a
client certificate whose SHA-256 fingerprint appears (hex, optional
`:` separators, `#` comments) on the allowlist file. Peers without a
listed cert cannot even open the TCP connection.

See `docs/ADMIN_GUIDE.md` § "Peer-mesh security" for setup.

## Response envelopes

### Success (2xx)

JSON body, shape depends on endpoint. Common patterns:

```json
{ "memory": { … } }
{ "memories": [ … ], "count": 5 }
{ "id": "abc123" }
{ "ok": true }
```

### Error (4xx, 5xx)

Uniform envelope:

```json
{ "error": "descriptive message" }
```

Status codes you'll commonly encounter:

| Code | Meaning |
|------|---------|
| 200 | OK |
| 201 | Created |
| 202 | Accepted — governance pending |
| 400 | Bad request — validation, parse, or limit error |
| 401 | Unauthorized — missing / invalid API key |
| 403 | Forbidden — governance denied |
| 404 | Not found |
| 409 | Conflict — duplicate `(title, namespace)` |
| 500 | Internal server error |
| 503 | Service unavailable |

## Limits

- Bulk payload cap: **1000 items** (`/memories/bulk`, `/import`, `/sync/push`).
- List pagination: capped at **200** per page.
- Recall: capped at **50** per request.
- Sync/since: capped at **10,000** per request.
- No per-client rate limiting at the HTTP layer — all writes contend
  for a single `Mutex<Connection>`. Batch or throttle at the caller.

## The `Memory` object

```json
{
  "id": "uuid-v4",
  "tier": "mid",
  "namespace": "global",
  "title": "Memory title",
  "content": "Memory body",
  "tags": ["tag1", "tag2"],
  "priority": 5,
  "confidence": 0.95,
  "source": "api",
  "access_count": 3,
  "created_at": "2026-04-19T10:30:00Z",
  "updated_at": "2026-04-19T10:30:00Z",
  "last_accessed_at": "2026-04-19T12:00:00Z",
  "expires_at": "2026-04-26T10:30:00Z",
  "metadata": {
    "agent_id": "ai:claude-opus-4.7",
    "scope": "private",
    "custom_field": "value"
  }
}
```

Fields marked in `metadata` are preserved across update / upsert /
sync / consolidate.

---

## Health + metrics

### `GET /api/v1/health`

No authentication required. Returns daemon liveness.

**Response**

```json
{ "status": "ok", "service": "ai-memory" }
```

```bash
curl http://127.0.0.1:9077/api/v1/health
```

### `GET /metrics` and `GET /api/v1/metrics`

Prometheus text exposition format. Scrape from Prometheus, alertmanager,
or Grafana Agent.

```bash
curl http://127.0.0.1:9077/metrics
```

### `GET /api/v1/stats`

Structured database stats (counts by tier/namespace, links, size,
last GC).

```json
{
  "total": 150,
  "by_tier": [{"tier":"short","count":20},{"tier":"mid","count":100},{"tier":"long","count":30}],
  "by_namespace": [{"namespace":"global","count":90}],
  "expiring_soon": 5,
  "links_count": 23,
  "db_size_bytes": 524288
}
```

## Memory CRUD

### `POST /api/v1/memories` — create

```json
{
  "title": "Quick note",
  "content": "Content",
  "tier": "mid",
  "namespace": "global",
  "tags": ["urgent"],
  "priority": 7,
  "confidence": 0.9,
  "source": "api",
  "ttl_secs": 604800,
  "metadata": {"custom": "data"},
  "agent_id": "alice",
  "scope": "private"
}
```

- **201 Created** with `{ "id": "...", "tier": "mid", "namespace": "...", "title": "...", "agent_id": "..." }`.
- **202 Accepted** (governance pending) with `{ "status": "pending", "pending_id": "...", "action": "store" }`.
- **400 / 403 / 500** per validation / governance / server error.

```bash
curl -X POST http://127.0.0.1:9077/api/v1/memories \
  -H "X-API-Key: KEY" -H "X-Agent-Id: alice" \
  -H "Content-Type: application/json" \
  -d '{"title":"Meeting notes","content":"Q2 roadmap","tier":"mid"}'
```

### `GET /api/v1/memories` — list

Query params: `namespace`, `tier`, `limit` (default 20, max 200),
`offset`, `min_priority`, `since`, `until`, `tags` (comma list),
`agent_id`.

```json
{ "memories": [ … ], "count": 1 }
```

### `GET /api/v1/memories/{id}` — get

UUID or unique prefix. Returns memory + its links.

```json
{
  "memory": { … },
  "links": [{"source_id":"…","target_id":"…","relation":"related_to","created_at":"…"}]
}
```

### `PUT /api/v1/memories/{id}` — update

All fields optional. Tier never downgrades.

```json
{ "title": "New", "priority": 8, "tier": "long" }
```

- **200** on success, **409** on `(title, namespace)` collision, **404** on missing.

### `DELETE /api/v1/memories/{id}` — delete

Archives before delete when `archive_on_gc=true`.

- **200 OK** `{"deleted": true}` or **202** when governance is pending.

### `POST /api/v1/memories/bulk` — batch create

Body is a JSON array of `CreateMemory` objects, **≤ 1000** items.

```json
{ "created": 998, "errors": ["item 17: title is required", … ] }
```

## Recall + search

### `GET /api/v1/recall` and `POST /api/v1/recall`

Hybrid recall (FTS5 + semantic + blend). **Mutates the database**
(touches, auto-promotes).

Query / body fields: `context` (required), `namespace`, `limit`
(default 10, max 50), `tags`, `since`, `until`, `as_agent`,
`budget_tokens`.

```json
{
  "memories": [ { …, "score": 0.87 } ],
  "count": 5,
  "tokens_used": 234,
  "budget_tokens": 3000
}
```

```bash
curl -X POST http://127.0.0.1:9077/api/v1/recall \
  -H "Content-Type: application/json" \
  -d '{"context":"quarterly planning","limit":10}'
```

### `GET /api/v1/search`

Read-only FTS5 keyword search. Same filter params as list, plus `q`
(required).

```json
{ "results": [ … ], "count": 3, "query": "urgent deadline" }
```

## Lifecycle

### `POST /api/v1/memories/{id}/promote`

Bump to long tier. 200 / 202 / 404.

### `POST /api/v1/forget`

```json
{ "namespace": "scratch", "pattern": "deprecated", "tier": "short" }
```

At least one filter required. Returns `{"deleted": N}`.

### `POST /api/v1/consolidate`

```json
{
  "ids": ["id1","id2","id3"],
  "title": "Summary",
  "summary": "Merged content",
  "namespace": "global",
  "tier": "long"
}
```

201 with `{"id":"consolidated-uuid","consolidated":3}`.

### `POST /api/v1/gc`

Immediate garbage collection. Empty body. Returns `{"expired_deleted":N}`.

## Links

### `POST /api/v1/links`

```json
{ "source_id": "abc", "target_id": "def", "relation": "supersedes" }
```

Relations: `related_to`, `supersedes`, `contradicts`, `derived_from`.

### `GET /api/v1/links/{id}`

Returns inbound + outbound links for a memory.

## Namespaces

### `GET /api/v1/namespaces`

```json
{ "namespaces": [{"name":"global","count":50},{"name":"project-x","count":30}] }
```

## Archive

### `GET /api/v1/archive`

Query: `namespace`, `limit` (default 50, max 1000), `offset`.

### `POST /api/v1/archive/{id}/restore`

### `DELETE /api/v1/archive?older_than_days=30`

### `GET /api/v1/archive/stats`

## Agents + governance

### `POST /api/v1/agents`

```json
{ "agent_id": "alice", "agent_type": "human", "capabilities": ["read","write"] }
```

`agent_type` accepts `human`, `system`, or any `ai:<name>` form
(`ai:claude-opus-4.7`, `ai:gpt-5`, etc.).

### `GET /api/v1/agents`

Returns `{"agents":[…],"count":N}`.

### `GET /api/v1/pending`

Query: `status=pending|approved|rejected`, `limit` (default 100, max 1000).

### `POST /api/v1/pending/{id}/approve`

200 if consensus reached (and governed action executed). 202 if still
collecting approvers.

### `POST /api/v1/pending/{id}/reject`

200 with `{"rejected":true,"id":"…","decided_by":"alice"}`.

## Sync / federation

### `POST /api/v1/sync/push`

Peer-to-peer push with timestamp-aware merge.

```json
{
  "sender_agent_id": "peer-remote-1",
  "memories": [ { … up to 1000 … } ],
  "dry_run": false
}
```

Response includes `applied`, `noop`, `skipped`, `receiver_agent_id`,
`receiver_clock`.

### `GET /api/v1/sync/since`

Query: `since` (RFC3339, optional), `limit` (default 500, max 10000),
`peer` (attribution tag).

```json
{ "count": 5, "limit": 500, "memories": [ … ] }
```

## Import / export

### `GET /api/v1/export`

Returns `{"memories":[…],"links":[…],"count":N,"exported_at":"…"}`.

### `POST /api/v1/import`

Body matches export shape. `≤ 1000` memories per call. Preserves
original `metadata.agent_id` into `metadata.imported_from_agent_id`.

## Webhooks (v0.6.0.0)

Three endpoints under `/api/v1/` — create them via MCP tools or (when
wired) the REST surface. See the MCP reference for authoritative
definitions: `memory_subscribe`, `memory_unsubscribe`,
`memory_list_subscriptions`. Dispatch is SSRF-hardened (rejects
private-range IPs; requires `https://` unless loopback).

## Federation (v0.7, opt-in via `--quorum-writes`)

When `ai-memory serve --quorum-writes N --quorum-peers URL,URL,…` is
set, every write fans out to peers and returns **only** once W-1 peer
acks land within `--quorum-timeout-ms`.

- **201** + `{"quorum_acks": W}` on success.
- **503** + `{"error":"quorum_not_met","got":X,"needed":Y,"reason":"unreachable|timeout|id_drift"}` + `Retry-After: 2` on failure.

Local write is **not** rolled back on quorum failure — per
`ADR-0001`, the sync-daemon's eventual-consistency loop converges
peers afterwards.

## Curl recipes

```bash
# Health
curl http://127.0.0.1:9077/api/v1/health

# Store a memory
curl -X POST -H "Content-Type: application/json" \
  http://127.0.0.1:9077/api/v1/memories \
  -d '{"title":"hi","content":"there","tier":"mid"}'

# Recall
curl -X POST -H "Content-Type: application/json" \
  http://127.0.0.1:9077/api/v1/recall \
  -d '{"context":"what did I store","limit":5}'

# Incremental sync pull since a timestamp
curl 'http://127.0.0.1:9077/api/v1/sync/since?since=2026-04-01T00:00:00Z&limit=1000'

# Prometheus scrape
curl http://127.0.0.1:9077/metrics
```

## See also

- `docs/USER_GUIDE.md` — MCP tool reference (parallel to this HTTP doc).
- `sdk/typescript/README.md` — TypeScript SDK using these endpoints.
- `sdk/python/README.md` — Python sync + async SDK.
- `docs/CLI_REFERENCE.md` — corresponding CLI surface.
- `docs/SECURITY.md` — API key + mTLS + governance.
- `docs/TROUBLESHOOTING.md` — common error scenarios.
