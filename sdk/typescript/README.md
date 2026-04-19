# @alphaone/ai-memory

TypeScript SDK for the [ai-memory](../../) HTTP API — persistent, tier-aware
memory for AI agents, built on top of the same daemon that powers the MCP
server.

> Status: `0.6.0-alpha.0`. Target server is ai-memory v0.6.0 HTTP API at
> `/api/v1/`. Some endpoints (subscriptions, notify/inbox, cluster, metrics,
> grant/revoke) target v0.6.0.0 shape and may not be merged server-side yet.

- Runtime: Node 20+ (uses `undici.fetch` — Node 20 ships undici as its
  platform `fetch`). Modern browsers work too; see [Browser usage](#browser-usage).
- License: Apache-2.0
- Repo: <https://github.com/alphaone/ai-memory>

## Install

```bash
npm install @alphaone/ai-memory
# or
pnpm add @alphaone/ai-memory
# or
yarn add @alphaone/ai-memory
```

> This SDK is not yet published to npm. During the alpha, consume it via
> the workspace directly or a tarball (`npm pack`). See the root ROADMAP.

## Quickstart

```ts
import { AiMemoryClient } from "@alphaone/ai-memory";

const memory = new AiMemoryClient({
  baseUrl: "http://localhost:9077",
  apiKey: process.env.AI_MEMORY_API_KEY,     // optional
  agentId: "ai:claude-opus-4.7@laptop:pid-1234", // optional default header
});

// Store
const m = await memory.store({
  title: "DNS zone refresh procedure",
  content: "Run `rndc reload` then check SERIAL bump in named.log …",
  tier: "long",
  namespace: "ops/dns",
  tags: ["bind9", "runbook"],
  priority: 8,
});

// Recall — fuzzy, scored, mutates access_count + TTL
const hits = await memory.recall({
  context: "how do I refresh a dns zone",
  namespace: "ops/dns",
  limit: 5,
});
for (const hit of hits.memories) {
  console.log(hit.score.toFixed(3), hit.title);
}
```

## Methods

All methods return typed `Promise<T>` and throw typed errors (see
[Error handling](#error-handling)). `RequestOptions` (last argument) lets
you override `agentId`, pass an `AbortSignal`, or add custom headers.

| Method | Endpoint | Description |
| --- | --- | --- |
| `store(body, opts?)` | `POST /api/v1/memories` | Create a new memory. |
| `storeBulk(memories, opts?)` | `POST /api/v1/memories/bulk` | Batch insert up to 1000. |
| `get(id, opts?)` | `GET /api/v1/memories/:id` | Fetch by id. |
| `update(id, body, opts?)` | `PUT /api/v1/memories/:id` | Patch fields. |
| `delete(id, opts?)` | `DELETE /api/v1/memories/:id` | Delete by id. |
| `promote(id, opts?)` | `POST /api/v1/memories/:id/promote` | Promote to long tier. |
| `list(query?, opts?)` | `GET /api/v1/memories` | Paginated list with filters. |
| `recall(body, opts?)` | `POST /api/v1/recall` | Fuzzy hybrid recall. |
| `search(query, opts?)` | `GET /api/v1/search` | AND keyword search. |
| `forget(body, opts?)` | `POST /api/v1/forget` | Bulk delete by pattern/ns/tier. |
| `link(body, opts?)` | `POST /api/v1/links` | Link two memories. |
| `getLinks(id, opts?)` | `GET /api/v1/links/:id` | Fetch all links for a memory. |
| `stats(opts?)` | `GET /api/v1/stats` | Aggregate stats. |
| `health(opts?)` | `GET /api/v1/health` | Liveness probe. |
| `namespaces(opts?)` | `GET /api/v1/namespaces` | List namespaces w/ counts. |
| `agents(opts?)` | `GET /api/v1/agents` | List registered agents. |
| `registerAgent(body, opts?)` | `POST /api/v1/agents` | Register an NHI. |
| `metrics(opts?)` | `GET /api/v1/metrics` | Prometheus text-format. |
| `subscribe(body, opts?)` | `POST /api/v1/subscriptions` | Register a webhook. |
| `unsubscribe(id, opts?)` | `DELETE /api/v1/subscriptions/:id` | Remove a webhook. |
| `listSubscriptions(opts?)` | `GET /api/v1/subscriptions` | List current webhooks. |
| `grant(memoryId, body, opts?)` | `POST /api/v1/memories/:id/grant` | Grant access. |
| `revoke(memoryId, body, opts?)` | `POST /api/v1/memories/:id/revoke` | Revoke access. |
| `notify(body, opts?)` | `POST /api/v1/notify` | Send inbox message. |
| `inbox(query?, opts?)` | `GET /api/v1/inbox` | Read inbox. |
| `cluster(body, opts?)` | `POST /api/v1/cluster` | Peer management. |

### Examples

#### `.store()`

```ts
await memory.store({
  title: "Rust trait bounds refresher",
  content: "Trait bounds `where T: Send + Sync` …",
  tier: "mid",
  namespace: "rust",
  tags: ["rust", "traits"],
  priority: 6,
  confidence: 0.9,
  metadata: { source_commit: "abc123" },
});
```

#### `.recall()`

```ts
const { memories, tokens_used } = await memory.recall({
  context: "what's our nginx reload command?",
  namespace: "ops",
  budget_tokens: 2000,
});
```

#### `.search()`

```ts
const { results } = await memory.search({
  q: "bind9 AND reload",
  namespace: "ops/dns",
  limit: 20,
});
```

#### `.list()`

```ts
const { memories } = await memory.list({
  namespace: "rust",
  tier: "long",
  limit: 50,
  offset: 0,
});
```

#### `.get()` / `.delete()`

```ts
const m = await memory.get("b4e3…");
await memory.delete("b4e3…");
```

#### `.subscribe()` + webhook verification

```ts
import express from "express";
import { verifyWebhookSignature } from "@alphaone/ai-memory/webhooks";

const sub = await memory.subscribe({
  callback_url: "https://myapp.example.com/webhooks/ai-memory",
  events: ["memory.stored", "memory.updated"],
  secret: process.env.WEBHOOK_SECRET,
});

const app = express();
app.post(
  "/webhooks/ai-memory",
  express.raw({ type: "application/json" }),
  (req, res) => {
    const sig = String(req.header("X-AI-Memory-Signature") ?? "");
    if (!verifyWebhookSignature(req.body, sig, process.env.WEBHOOK_SECRET!)) {
      return res.status(401).send("bad signature");
    }
    const event = JSON.parse(req.body.toString("utf8"));
    console.log("event", event);
    res.status(204).end();
  },
);
```

#### `.unsubscribe()`

```ts
await memory.unsubscribe(sub.id);
```

#### `.notify()` / `.inbox()`

```ts
await memory.notify({
  to: "ai:codex-5.4@ci:pid-42",
  subject: "review needed",
  body: "please approve pending action pa_123",
  memory_id: "b4e3…",
});

const { messages, unread } = await memory.inbox({ unread: true, limit: 20 });
```

#### `.grant()` / `.revoke()`

```ts
await memory.grant("b4e3…", { agent_id: "alice", permission: "read" });
await memory.revoke("b4e3…", { agent_id: "alice" });
```

#### `.cluster()`

```ts
const { peers } = await memory.cluster({ action: "list" });
```

#### `.agents()` / `.registerAgent()`

```ts
await memory.registerAgent({
  agent_id: "ai:claude-opus-4.7@ci:pid-99",
  agent_type: "ai:claude-opus-4.7",
  capabilities: ["memory_write", "memory_recall"],
});
const { agents } = await memory.agents();
```

#### `.health()`

```ts
const h = await memory.health();
if (h.status !== "ok") throw new Error("memory daemon unhealthy");
```

## Agent identity (NHI)

`agent_id` precedence (from `docs/CLAUDE.md` §Agent Identity):

1. Explicit body-level `agent_id` on `.store()` / `.registerAgent()` etc.
2. `X-Agent-Id` HTTP header — set via `new AiMemoryClient({ agentId })` or
   per-call `opts.agentId`.
3. Server-side `anonymous:req-<uuid8>` fallback (logged at WARN).

The id must match the regex `^[A-Za-z0-9_\-:@./]{1,128}$` — permits prefixed
forms like `ai:claude-opus-4.7@host:pid-123` and future SPIFFE-style ids.
Rejects whitespace, null bytes, control chars, shell metacharacters.

## Authentication

### API key (`X-API-Key`)

The daemon's `api_key_auth` middleware checks `X-API-Key` (or `?api_key=` query
param) when an API key is configured. Pass it to the client once:

```ts
const memory = new AiMemoryClient({
  baseUrl: "https://memory.internal",
  apiKey: process.env.AI_MEMORY_API_KEY,
});
```

### mTLS

ai-memory itself is HTTP-only by design. Terminate TLS and client-certificate
auth at a reverse proxy (nginx, Caddy, Envoy) in front of the daemon, then
forward the verified identity via a header to the backend.

Example nginx snippet:

```nginx
server {
  listen 443 ssl;
  ssl_certificate     /etc/ssl/memory.crt;
  ssl_certificate_key /etc/ssl/memory.key;
  ssl_client_certificate /etc/ssl/ca.crt;
  ssl_verify_client on;

  location / {
    proxy_set_header X-Agent-Id $ssl_client_s_dn_cn;
    proxy_pass http://127.0.0.1:9077;
  }
}
```

## Error handling

All methods reject with a typed error:

```ts
import {
  ApiError,
  ValidationError,
  UnauthorizedError,
  NotFoundError,
  ConflictError,
  ServerError,
  NetworkError,
} from "@alphaone/ai-memory";

try {
  await memory.get("bogus");
} catch (err) {
  if (err instanceof NotFoundError) {
    // 404
  } else if (err instanceof ValidationError) {
    // 400 — err.message carries the server's reason
  } else if (err instanceof UnauthorizedError) {
    // 401/403 — missing or bad API key
  } else if (err instanceof ApiError) {
    console.error(err.status, err.code, err.message);
  } else if (err instanceof NetworkError) {
    // connection refused, DNS, TLS, timeout, etc.
  } else {
    throw err;
  }
}
```

## Claude Code / Cursor integration

### Claude Code (MCP server already includes memory tools)

Run ai-memory as the MCP server and this SDK in your own tooling scripts:

```json
// ~/.config/claude-code/settings.json
{
  "mcpServers": {
    "memory": { "command": "ai-memory", "args": ["serve"] }
  }
}
```

Inside a Claude Code slash command script (or hook) you can call the HTTP
SDK against the same daemon, e.g. to fan-out a memory bulk-create from a
CI hook:

```ts
import { AiMemoryClient } from "@alphaone/ai-memory";
const mem = new AiMemoryClient({ baseUrl: "http://localhost:9077" });
await mem.storeBulk(newFacts.map(f => ({ title: f.title, content: f.body, tier: "long" })));
```

### Cursor

In Cursor's `.cursorrules` or a project-level script, use the same client.
Cursor's agent surface will find stored memories via the MCP server; the SDK
is for bespoke tool integrations (post-commit hooks, review automation).

### Claude desktop extensions

Register the SDK-driven tool under `.claude/commands/*.ts` and import the
client as shown in Quickstart.

## Browser usage

`undici.fetch` is a Node module. For browsers, pass the native `fetch`:

```ts
import { AiMemoryClient } from "@alphaone/ai-memory";
// @ts-ignore - browser globalThis.fetch
const memory = new AiMemoryClient({ baseUrl: "/api" }, globalThis.fetch);
```

Webhook verification (`verifyWebhookSignature`) uses `node:crypto`; for the
browser use the Web Crypto API (`crypto.subtle.importKey` + `sign("HMAC")`) —
not yet shipped in this package.

## Development

```bash
npm install        # (not run by the scaffold generator — review first)
npm run typecheck  # tsc --noEmit
npm run build      # emit ./dist
AI_MEMORY_TEST_DAEMON=1 npm test   # integration tests against a live daemon
```

## Links

- Main repo: <https://github.com/alphaone/ai-memory>
- Architecture notes: [`docs/CLAUDE.md`](../../CLAUDE.md)
- HTTP API handlers: [`src/handlers.rs`](../../src/handlers.rs)
- Validation rules: [`src/validate.rs`](../../src/validate.rs)
