---
sidebar_position: 5
title: Peer-to-peer mesh
description: The grand-slam capability — distributed AI fleet brain.
---

# Peer-to-peer mesh

> **The grand-slam capability.** `ai-memory sync-daemon --peers <url>` forms a live peer-to-peer knowledge mesh with any other `ai-memory serve` instance. One agent learns it; the peer knows it within a cycle. **No cloud. No login. No SaaS.**

## Two-laptop demo

Two machines, one command each:

```bash
# Laptop A
ai-memory serve --tls-cert serverA.pem --tls-key serverA.key \
  --mtls-allowlist peers.allow &
ai-memory sync-daemon --peers https://laptop-b:9077 \
  --client-cert clientA.pem --client-key clientA.key

# Laptop B
ai-memory serve --tls-cert serverB.pem --tls-key serverB.key \
  --mtls-allowlist peers.allow &
ai-memory sync-daemon --peers https://laptop-a:9077 \
  --client-cert clientB.pem --client-key clientB.key
```

Agent on A: `ai-memory store -T "PostgreSQL 16" -c "...".`
Agent on B (10 seconds later): `ai-memory recall "what database"` → returns it.

## How it works

Each cycle (default 10 s, minimum 1 s):

1. **Pull** — `GET /api/v1/sync/since?since=<watermark>` from each peer
2. **Insert-if-newer** — only memories newer than local copy are written
3. **Push** — `POST /api/v1/sync/push` with local memories newer than `last_pushed_at`
4. **Update vector clocks** in `sync_state` table

## Daemon flags

| Flag | Default | Purpose |
|---|---|---|
| `--peers <url,url,...>` | required | Comma-separated peer URLs |
| `--interval <secs>` | 10 | Seconds between cycles |
| `--batch-size <N>` | 500 | Cap memories per peer per cycle |
| `--client-cert` / `--client-key` | none | Layer 2 mTLS client cert |
| `--api-key <key>` | none | `X-API-Key` header for auth |

## Resilience

- **Per-cycle errors don't crash the daemon.** Bad URL, refused connection, TLS handshake failure — daemon logs WARN and retries next cycle.
- **Graceful shutdown** on SIGINT — in-flight requests get 10 s ([#233](https://github.com/alphaonedev/ai-memory-mcp/issues/233) tracking longer window for v0.6.1).
- **Idempotent sync** — second cycle through the same memories reports `pulled=0 pushed=0`.

## What you get

- **Shift handoff** — night-shift agents sync to day-shift agents automatically. Zero knowledge gap.
- **Swarm intelligence** — 100 agents processing tickets. Each learns. Promotion + sync = the 100th ticket benefits from all 99 prior learnings.
- **Knowledge inheritance** — server decommissioned, agent dies, replacement syncs and has full institutional knowledge from day one.
- **Distributed immune system** — one agent encounters a novel failure, promotes it, syncs to fleet. Entire fleet immunized.

## Roadmap

Phase 3 foundation ships in v0.6.0 GA. Tracked under [#224](https://github.com/alphaonedev/ai-memory-mcp/issues/224):

- [x] Vector clocks per agent
- [x] Push / pull HTTP endpoints with watermark
- [x] sync-daemon with graceful shutdown
- [x] Native TLS (Layer 1)
- [x] mTLS with fingerprint allowlist (Layer 2)
- [ ] Field-level CRDT-lite merge (Task 3a.1)
- [ ] Attested `sender_agent_id` from cert CN/SAN (Layer 2b)
- [ ] Per-peer auth tokens, streaming, resume-on-interrupt (Task 3b.2)
- [ ] Hierarchy-scoped selective sync (Task 3b.3)
- [ ] E2E memory encryption (X25519 + ChaCha20-Poly1305) — Layer 3, v0.8 target
