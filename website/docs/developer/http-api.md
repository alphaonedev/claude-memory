---
sidebar_position: 5
title: HTTP API reference
description: All 24 REST endpoints under /api/v1/.
---

# HTTP API reference

ai-memory's HTTP daemon listens on port **9077** by default. All routes prefix `/api/v1/`.

## Memory CRUD

| Method | Path | Handler |
|---|---|---|
| `POST` | `/memories` | Create (or upsert) |
| `GET` | `/memories` | List with query params |
| `GET` | `/memories/{id}` | Get by ID (returns `{links, memory}`) |
| `PUT` | `/memories/{id}` | Update |
| `DELETE` | `/memories/{id}` | Delete |
| `POST` | `/memories/bulk` | Batch create (max 1000) |
| `POST` | `/memories/{id}/promote` | Promote to long |

## Recall + search

| Method | Path |
|---|---|
| `GET` | `/recall?context=...&namespace=...&as_agent=...&budget_tokens=...` |
| `POST` | `/recall` (body) |
| `GET` | `/search?query=...` |

## Forget + consolidate

| Method | Path |
|---|---|
| `POST` | `/forget` |
| `POST` | `/consolidate` |

## Links

| Method | Path |
|---|---|
| `POST` | `/links` |
| `GET` | `/links/{id}` |

## Namespaces + agents (v0.6.0+)

| Method | Path |
|---|---|
| `GET` | `/namespaces` (list) |
| `GET` | `/agents` |
| `POST` | `/agents` (register) |
| `GET` | `/pending` (governance queue) |
| `POST` | `/pending/{id}/approve` |
| `POST` | `/pending/{id}/reject` |

## Sync (v0.6.0+)

| Method | Path |
|---|---|
| `GET` | `/sync/since?since=<rfc3339>&limit=<N>&peer=<id>` |
| `POST` | `/sync/push` |

> **Security:** Sync endpoints **MUST** be protected by `--tls-cert` + `--tls-key` + `--mtls-allowlist` in production. See [#231](https://github.com/alphaonedev/ai-memory-mcp/issues/231).

## Misc

| Method | Path |
|---|---|
| `GET` | `/health` |
| `GET` | `/stats` |
| `GET` | `/export` |
| `POST` | `/import` |
| `POST` | `/gc` |
| `GET`/`DELETE` | `/archive` |
| `POST` | `/archive/{id}/restore` |
| `GET` | `/archive/stats` |

## Headers

| Header | Purpose |
|---|---|
| `X-Agent-Id` | Multi-tenant attribution (required if no body `agent_id`) |
| `X-API-Key` | Optional API key auth (planned) |

## Source

Routes in `src/main.rs::827-872`. Handlers in `src/handlers.rs`.
