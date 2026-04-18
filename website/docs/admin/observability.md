---
sidebar_position: 8
title: Observability
description: Logging, metrics, health, and stats.
---

# Observability

## Logs

ai-memory uses `tracing` with `RUST_LOG`-style filters:

```bash
RUST_LOG=ai_memory=debug ai-memory serve
```

Useful filters:
- `ai_memory=info` — startup, recall events, GC, sync cycles
- `ai_memory=debug` — embedding model loads, query expansion, contradiction checks
- `tower_http=info` — HTTP request log
- `ai_memory=warn` — sync cycle failures, per-peer errors

## Health endpoint

```bash
curl https://localhost:9077/api/v1/health
# {"service":"ai-memory","status":"ok"}
```

## Stats

```bash
ai-memory stats
ai-memory stats --json
```

Returns counts by tier, namespace breakdown, link counts, archive size, DB file size.

## Sync state

```bash
sqlite3 ~/.local/share/ai-memory/memories.db \
  "SELECT agent_id, peer_id, last_seen_at, last_pulled_at, last_pushed_at FROM sync_state"
```

Shows the vector-clock state for each peer.

## Metrics (planned)

Prometheus `/metrics` endpoint is on the v0.7+ roadmap. For now, scrape stats via the `--json` CLI flag and convert in your collector.
