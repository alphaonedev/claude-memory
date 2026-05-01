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
  "expires_at": "2026-05-08T10:30:00Z",
  "metadata": {"custom": "data"},
  "agent_id": "alice",
  "scope": "private"
}
```

`ttl_secs` is HTTP-only — the MCP `memory_store` tool exposes
`expires_at` instead (also accepted on this HTTP endpoint). See the
HTTP ↔ MCP parameter coverage table at the bottom of this document.

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

> **Note (HTTP ↔ MCP parity):** The MCP `memory_recall`,
> `memory_search`, and `memory_list` tools accept an optional `format`
> parameter (`json` | `toon` | `toon_compact`). The HTTP endpoints do
> not yet expose `format`; HTTP responses are always JSON. The MCP
> `memory_recall` tool also accepts a `context_tokens` array (v0.6.0.0
> contextual recall — recent conversation tokens biasing the query
> embedding at 70/30) that the HTTP body does not surface.

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

## Knowledge Graph + taxonomy (v0.6.3)

These endpoints operate on the temporal-validity knowledge graph
(`memory_links` with `valid_from` / `valid_until` / `observed_by`
columns added in schema v15) and the namespace taxonomy. See
`docs/MIGRATION-v0.6.2-to-v0.6.3.md` for the schema changes and
`docs/USER_GUIDE.md` for the matching MCP tools.

### `GET /api/v1/taxonomy`

Walk live (non-expired) memories grouped by namespace into a
hierarchical tree.

Query params: `prefix` (optional, restricts walk), `depth` (1-10, default 5),
`limit` (1-10000, default 1000).

```json
{
  "tree": [
    { "namespace": "alphaone", "count": 0, "subtree_count": 47, "children": [...] }
  ],
  "total_count": 47,
  "truncated": false
}
```

### `POST /api/v1/check_duplicate`

Embedding cosine-similarity duplicate detection.

```json
{
  "title": "Project uses PostgreSQL 15",
  "content": "The main database is PostgreSQL 15 with pgvector for embeddings.",
  "namespace": "my-app",
  "threshold": 0.85
}
```

Response:

```json
{
  "is_duplicate": true,
  "threshold": 0.85,
  "nearest": { "id": "...", "title": "...", "namespace": "...", "similarity": 0.92 },
  "suggested_merge": "...",
  "candidates_scanned": 412
}
```

`threshold` is clamped to a 0.5 floor. Requires the `semantic` feature
tier or higher. Returns `409` (Conflict) only on internal embedding
errors; threshold mismatches return `200` with `is_duplicate: false`.

### `POST /api/v1/entities`

Register an entity-as-typed-memory. Idempotent on
`(canonical_name, namespace)`.

```json
{
  "canonical_name": "PostgreSQL",
  "namespace": "my-app",
  "aliases": ["pg", "postgres"],
  "metadata": {}
}
```

Response: `{"entity_id":"ent-...","canonical_name":"PostgreSQL","namespace":"my-app","aliases":["pg","postgres","PostgreSQL"],"created":true}`.

Returns `409` if a non-entity memory with the same
`(title, namespace)` exists.

### `GET /api/v1/entities/by_alias`

Resolve an alias to its canonical entity.

Query params: `alias` (required), `namespace` (optional; without it,
picks the most-recently-created match across namespaces).

```json
{
  "found": true,
  "entity_id": "ent-...",
  "canonical_name": "PostgreSQL",
  "namespace": "my-app",
  "aliases": ["pg", "postgres", "PostgreSQL"]
}
```

`found: false` (and null fields) when the alias resolves to nothing.

### `GET /api/v1/kg/timeline`

Ordered timeline of links anchored at a source. Skips links with NULL
`valid_from`.

Query params: `source_id` (required), `since` / `until` (RFC 3339,
optional), `limit` (1-1000, default 200).

```json
{
  "source_id": "...",
  "events": [
    { "target_id": "...", "relation": "depends_on", "valid_from": "...", "valid_until": null, "observed_by": "..." }
  ],
  "count": 1
}
```

### `POST /api/v1/kg/invalidate`

Mark a link superseded by setting `valid_until`. **Does NOT delete**
the link — historical queries pinned to `valid_at < now` still see
it. Idempotent.

```json
{
  "source_id": "...",
  "target_id": "...",
  "relation": "depends_on",
  "valid_until": "2026-04-26T03:00:00Z"
}
```

Response: `{"found":true,"valid_until":"...","previous_valid_until":null}`.

> **Federation:** invalidations apply locally and propagate
> asynchronously via the sync-daemon — they are NOT quorum-broadcast.

### `POST /api/v1/kg/query`

Recursive-CTE traversal of the temporal knowledge graph rooted at a
source memory.

```json
{
  "source_id": "...",
  "max_depth": 3,
  "valid_at": "2026-04-26T00:00:00Z",
  "allowed_agents": ["ai:claude-code@host:pid-12345"],
  "limit": 200
}
```

Constraints: `max_depth` clamped to 1..=5 (depth 0 errors,
depth > 5 errors). `allowed_agents: []` (empty array) returns zero
rows; omit the field to skip the agent filter entirely.

Response:

```json
{
  "source_id": "...",
  "max_depth": 3,
  "memories": [
    {
      "target_id": "...",
      "title": "...",
      "target_namespace": "my-app",
      "relation": "depends_on",
      "valid_from": "...",
      "valid_until": null,
      "observed_by": "...",
      "depth": 1,
      "path": "src->tgt"
    }
  ],
  "paths": ["src->tgt->..."],
  "count": 1
}
```

Ordering: `depth ASC, COALESCE(valid_from, link_created_at) ASC,
link_created_at ASC`.

## Namespaces

### `GET /api/v1/namespaces`

```json
{ "namespaces": [{"name":"global","count":50},{"name":"project-x","count":30}] }
```

### `GET /api/v1/namespaces/{ns}/standard` — get namespace standard

Query: `inherit` (boolean, default `false`). When `true`, returns the
full N-level resolved chain (global `*` → ancestors → namespace) instead
of the single namespace's standard.

```json
{ "namespace": "engineering/auth", "standards": [ … ], "chain": ["*","engineering","engineering/auth"], "count": 3 }
```

Returns 200 with `count: 0` and an empty `standards` array when no
standard is set. Equivalent MCP tool: `memory_namespace_get_standard`
(`src/mcp.rs:576`).

### `POST /api/v1/namespaces/{ns}/standard` — set namespace standard

Body: `{ "id": "<memory-id>", "parent": "<optional-parent-namespace>", "governance": { … } }`.
`governance` accepts `write` / `promote` / `delete` (each `any` |
`registered` | `owner` | `approve`), `approver` (ApproverType), and
`inherit` (boolean, default `true`). Equivalent MCP tool:
`memory_namespace_set_standard` (`src/mcp.rs:552`).

### `DELETE /api/v1/namespaces/{ns}/standard` — clear namespace standard

Removes the namespace's pinned standard (the standard memory itself is
not deleted; only the `namespace_meta.standard_id` link). Equivalent
MCP tool: `memory_namespace_clear_standard` (`src/mcp.rs:588`).

## Archive

### `GET /api/v1/archive` — list archived memories

Query: `namespace`, `limit` (default 50, max 1000), `offset`.

```json
{ "memories": [ … ], "count": 24 }
```

Equivalent MCP tool: `memory_archive_list` (`src/mcp.rs:489`).

### `POST /api/v1/archive/{id}/restore` — restore archived memory

Path param: `id` (archived memory id). On success the row is removed
from `archived_memories` and re-inserted into `memories` with
`original_tier` and `original_expires_at` re-applied where present.
Equivalent MCP tool: `memory_archive_restore` (`src/mcp.rs:501`).

### `DELETE /api/v1/archive?older_than_days=30` — purge archived memories

Query: `older_than_days` (optional). Without the query param, all
archived rows are eligible. Returns `{"purged": N}`. Equivalent MCP
tool: `memory_archive_purge` (`src/mcp.rs:512`).

### `GET /api/v1/archive/stats` — archive counters

```json
{ "total": 24, "by_namespace": [{"namespace":"global","count":18}, … ] }
```

Equivalent MCP tool: `memory_archive_stats` (`src/mcp.rs:522`).

## Agents + governance

### `POST /api/v1/agents`

```json
{ "agent_id": "alice", "agent_type": "human", "capabilities": ["read","write"] }
```

`agent_type` accepts `human`, `system`, or any `ai:<name>` form
(`ai:claude-opus-4.7`, `ai:gpt-5`, etc.).

### `GET /api/v1/agents`

Returns `{"agents":[…],"count":N}`.

### `GET /api/v1/pending` — list pending governance actions

Query: `status=pending|approved|rejected`, `limit` (default 100, max 1000).

```json
{ "pending": [ { "id": "…", "action_type": "store", "namespace": "…", "status": "pending", "approvals": [ … ] } ], "count": 3 }
```

Equivalent MCP tool: `memory_pending_list` (`src/mcp.rs:599`).

### `POST /api/v1/pending/{id}/approve` — approve pending action

Path param: `id`. Stamps `decided_by` with the caller's `X-Agent-Id`.
200 if consensus reached (and the governed action is executed). 202 if
still collecting approvers. Equivalent MCP tool: `memory_pending_approve`
(`src/mcp.rs:610`).

### `POST /api/v1/pending/{id}/reject` — reject pending action

Path param: `id`. Returns `{"rejected":true,"id":"…","decided_by":"alice"}`.
Equivalent MCP tool: `memory_pending_reject` (`src/mcp.rs:621`).

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

Three endpoints under `/api/v1/subscriptions` — create them via MCP
tools or the REST surface. Dispatch is SSRF-hardened (rejects
private-range IPs; requires `https://` unless loopback).

### `POST /api/v1/subscriptions` — register webhook

Body: `{ "url": "https://…", "events": ["memory_store", …], "secret": "<shared-secret>", "namespace_filter": "…", "agent_filter": "…" }`.
Stores `secret` as a SHA-256 hash for HMAC signing of dispatched
events. Returns the new subscription `id`. Equivalent MCP tool:
`memory_subscribe` (`src/mcp.rs:680`).

### `DELETE /api/v1/subscriptions?id=<id>` — unregister webhook

Returns `{"deleted": true}`. Equivalent MCP tool: `memory_unsubscribe`
(`src/mcp.rs:695`).

### `GET /api/v1/subscriptions` — list subscriptions

Returns `{"subscriptions":[…],"count":N}`. Each entry includes `url`,
`events`, `created_at`, `dispatch_count`, `failure_count`. Equivalent
MCP tool: `memory_list_subscriptions` (`src/mcp.rs:706`).

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

## HTTP ↔ MCP parameter coverage

A small set of parameters are surfaced by only one transport. The MCP
tool schema in `src/mcp.rs::tool_definitions()` is authoritative for
the MCP surface; the HTTP body / query types in `src/models.rs` and
the route handlers in `src/handlers.rs` are authoritative for HTTP.

| Tool | Param | HTTP | MCP | Notes |
|---|---|---|---|---|
| `memory_store` | `ttl_secs` | ✓ | ✗ | HTTP-only; the MCP tool exposes `expires_at` (also accepted by HTTP). |
| `memory_store` | `expires_at` | ✓ | (via `update`) | HTTP body accepts; documented in the `POST /api/v1/memories` example. |
| `memory_recall` | `format` | ✗ | ✓ | MCP-only; HTTP responses are always JSON. |
| `memory_recall` | `context_tokens` | ✗ | ✓ | MCP-only (v0.6.0.0 contextual recall). |
| `memory_search` | `format` | ✗ | ✓ | MCP-only. |
| `memory_list` | `format` | ✗ | ✓ | MCP-only. |

These gaps are intentional for v0.6.3.1 and tracked for parity
follow-up — they are NOT drift in the doc surface, just transport-level
surface-area differences captured here so operators don't re-derive
them.

## See also

- `docs/USER_GUIDE.md` — MCP tool reference (parallel to this HTTP doc).
- `sdk/typescript/README.md` — TypeScript SDK using these endpoints.
- `sdk/python/README.md` — Python sync + async SDK.
- `docs/CLI_REFERENCE.md` — corresponding CLI surface.
- `docs/SECURITY.md` — API key + mTLS + governance.
- `docs/TROUBLESHOOTING.md` — common error scenarios.
