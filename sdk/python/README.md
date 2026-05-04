# ai-memory — Python SDK

Typed Python client for the [ai-memory](https://github.com/alphaonedev/ai-memory-mcp)
HTTP API. Wraps the daemon's `/api/v1/` surface with sync and async clients,
Pydantic v2 models that mirror the Rust structs, and HMAC-SHA256 webhook
verification.

**Status:** `0.6.0-alpha.0` — scaffolding, unpublished. API may change before
GA.

## Install

```bash
# Not yet published; install from a local checkout:
pip install -e ./sdk/python
```

Requires Python 3.10+.

## Quickstart

```python
from ai_memory import AiMemoryClient, Tier

with AiMemoryClient(base_url="http://localhost:9077") as client:
    created = client.store(
        title="BIND9 build notes",
        content="Use --with-openssl=/opt/openssl, disable DoH for the lab.",
        tier=Tier.LONG,
        tags=["dns", "bind9"],
    )
    print(created["id"])

    hits = client.recall(context="how do I build BIND9?")
    for memory in hits.memories:
        print(memory.title, memory.confidence)
```

## Async

```python
import asyncio
from ai_memory import AsyncAiMemoryClient

async def main() -> None:
    async with AsyncAiMemoryClient(base_url="http://localhost:9077") as client:
        resp = await client.recall(context="hello")
        for memory in resp.memories:
            print(memory.title)

asyncio.run(main())
```

## Authentication

### API key

```python
AiMemoryClient(base_url="https://memory.example.com", api_key="sk-...")
```

The key is sent as the `X-API-Key` header on every request. The server
exempts `/api/v1/health` from auth.

### mTLS

```python
AiMemoryClient(
    base_url="https://memory.example.com",
    verify="/etc/ssl/certs/server-ca.pem",
    cert=("/etc/ssl/client/client.pem", "/etc/ssl/client/client.key"),
)
```

### Agent identity (NHI)

Set `agent_id` to stamp the `X-Agent-Id` header on every request. The
server writes `metadata.agent_id` accordingly (see CLAUDE.md §Agent
Identity).

```python
AiMemoryClient(base_url="http://localhost:9077", agent_id="ai:claude-opus-4.7@host")
```

## All methods

| Method | Endpoint | Notes |
|---|---|---|
| `health()` | `GET /api/v1/health` | Exempt from auth. |
| `metrics()` | `GET /api/v1/metrics` | Prometheus text format. |
| `store(...)` | `POST /api/v1/memories` | Upsert on `(title, namespace)`. |
| `bulk_store([...])` | `POST /api/v1/memories/bulk` | Up to 1000 per call. |
| `get(id)` | `GET /api/v1/memories/{id}` | Returns `Memory`. |
| `update(id, UpdateMemory)` | `PUT /api/v1/memories/{id}` | Partial update. |
| `delete(id)` | `DELETE /api/v1/memories/{id}` | |
| `promote(id)` | `POST /api/v1/memories/{id}/promote` | Tier -> `long`. |
| `list(...)` | `GET /api/v1/memories` | Filters: namespace, tier, tags, agent_id. |
| `search(q, ...)` | `GET /api/v1/search` | FTS AND search. |
| `recall(context, ...)` | `POST /api/v1/recall` | Hybrid semantic + FTS. |
| `forget(...)` | `POST /api/v1/forget` | Bulk delete by pattern. |
| `link(a, b, relation)` | `POST /api/v1/links` | `related_to`, `supersedes`, `contradicts`, `derived_from`. |
| `get_links(id)` | `GET /api/v1/links/{id}` | |
| `stats()` | `GET /api/v1/stats` | |
| `namespaces()` | `GET /api/v1/namespaces` | |
| `gc()` | `POST /api/v1/gc` | |
| `export()` / `import_()` | `GET` / `POST /api/v1/export|import` | |
| `subscribe(req)` / `unsubscribe(id)` / `subscriptions()` | `/api/v1/subscriptions` | Webhook mgmt. |
| `notify(req)` / `inbox(...)` | `/api/v1/notify`, `/api/v1/inbox` | Agent-to-agent messaging. |
| `grant(id, agent)` / `revoke(id, agent)` | `/api/v1/memories/{id}/grant|revoke` | Per-memory ACL. |
| `cluster(req)` | `POST /api/v1/cluster` | Peer management. |
| `agents()` / `register_agent(...)` | `/api/v1/agents` | NHI registry. |

## Webhook verification

```python
from ai_memory import verify_webhook_signature

def handle(request) -> None:
    sig = request.headers["X-AI-Memory-Signature"]
    if not verify_webhook_signature(request.body, sig, secret="..."):
        raise PermissionError("bad signature")
    ...
```

`body` must be the raw bytes as received — do not re-encode a parsed JSON
payload; whitespace differences will break the HMAC.

## Errors

All SDK errors derive from `AiMemoryError`:

| Exception | HTTP |
|---|---|
| `ValidationError` | 400 |
| `AuthError` | 401 |
| `ForbiddenError` | 403 |
| `NotFoundError` | 404 |
| `ConflictError` | 409 |
| `RateLimitError` | 429 |
| `ServerError` | 5xx |
| `TransportError` | network failure |

## License

Apache-2.0, see [LICENSE](../../LICENSE).
