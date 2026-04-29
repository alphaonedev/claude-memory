---
sidebar_position: 6
title: Agent identity (NHI)
description: Every memory carries the identity of the AI that learned it. Every recall surfaces it. Provenance for AI knowledge — built in from day one.
---

# Agent identity (NHI)

> **Every recall result tells you which AI learned the memory.** This is the differentiator: `ai-memory` doesn't just store knowledge — it remembers *who* added it, and that information is in front of every agent that recalls.

`metadata.agent_id` is a first-class field on every memory. When your AI calls `memory_recall`, the response includes a column telling you which agent each result came from — by default, in the token-efficient TOON compact format that AI clients are already optimised for.

## See it in action

Default recall output (TOON compact format):

```
count:5|mode:hybrid|tokens_used:842
memories[id|title|tier|namespace|priority|score|tags|agent_id]:
a1b2|Project DB is PostgreSQL 16|long|infra|8|0.91|database,postgres|ai:claude-code@workstation:pid-3812
c3d4|API rate limit is 100 rps|long|infra|7|0.87|api,limits|ai:claude-desktop@laptop:pid-5219
e5f6|Use TOON not JSON for MCP|long|protocol|9|0.82|toon,mcp,tokens|host:pop-os:pid-1144-ad1b9c
g7h8|Deploy gate fixed for v0.6.3|long|releases|6|0.79|deploy,gate|ai:codex@server:pid-9911
i9j0|Ship-gate cleanup complete|long|releases|5|0.74|cleanup|alice
```

Eight columns. Last column is `agent_id`. The recall blends what's relevant (text, semantics, priority, recency) **and** tells you who wrote it.

## What `agent_id` means

`agent_id` is a *claimed* identity attached to every memory at write time. It survives every operation that touches the memory: update, dedup, import, sync, consolidate. It's the substrate for filtering, scoping, governance, and (in v0.7) cryptographic attestation.

> **Honest framing:** `agent_id` today is *claimed*, not *attested*. Don't make security decisions on it without pairing with agent registration (Task 1.3 — upcoming) or attestation (v0.7+). The schema reservation for cryptographic signatures lives at `memory_links.signature` (schema v15, v0.6.3) and gets wired in v0.7.

## Resolution precedence

When `ai-memory` decides what `agent_id` to stamp on a new memory:

**CLI and MCP path:**

1. Explicit value from caller — `--agent-id <id>` flag, or `agent_id` field on the MCP tool, or `metadata.agent_id` embedded in a store request
2. `AI_MEMORY_AGENT_ID` environment variable
3. (MCP only) Captured from `initialize.clientInfo.name` → e.g. `ai:claude-code@workstation:pid-3812`
4. Stable per-process fallback → `host:<hostname>:pid-<pid>-<uuid8>`
5. `anonymous:pid-<pid>-<uuid8>` if hostname unavailable

**HTTP daemon path** (multi-tenant — no process-level default):

1. `agent_id` field in `POST /api/v1/memories` body
2. `X-Agent-Id` request header
3. Per-request `anonymous:req-<uuid8>` (logged at WARN)

## Validation

- Format: `^[A-Za-z0-9_\-:@./]{1,128}$`
- Permits: `ai:`, `host:`, `anonymous:` prefixes; `@` scope separator; `/` for SPIFFE-style IDs
- Rejects: whitespace, null bytes, control chars, shell metacharacters

## Immutability — defense in depth

Once a memory is stored, its `agent_id` doesn't drift. **Both** the caller layer (`identity::preserve_agent_id`) and the SQL layer (`json_set` CASE clauses in `db::insert` and `db::insert_if_newer`) enforce preservation across:

- `memory_update`
- Upsert / dedup on `(title, namespace)`
- HTTP `PUT /memories/{id}`
- `import` from JSON
- `sync_push` from a peer
- `memory_consolidate`

If a malicious or buggy caller tries to overwrite `agent_id`, the SQL layer refuses. This is the defense-in-depth NHI invariant from issue #148 / Task 1.2.

## Filter by agent

Every recall, list, and search supports `agent_id` filtering:

```bash
# CLI
ai-memory list --agent-id ai:claude-desktop@laptop:pid-5219
ai-memory search --agent-id alice "deploy"

# MCP — the agent_id property
{"name": "memory_search", "arguments": {"query": "deploy", "agent_id": "alice"}}

# HTTP
curl 'http://127.0.0.1:9077/api/v1/memories?agent_id=alice&query=deploy'
```

## System-stamped metadata (don't overwrite)

These keys are produced by ai-memory; the system sets them, callers must not.

| Key | When stamped |
|---|---|
| `imported_from_agent_id` | `ai-memory import` re-stamps the originator's `agent_id` (absent when `--trust-source` is passed) |
| `consolidated_from_agents` | `memory_consolidate` records the array of source authors; the consolidator's id becomes the new `agent_id` |
| `mined_from` | `ai-memory mine` stamps the source format (`claude` / `chatgpt` / `slack`) alongside the caller's `agent_id` |

## Privacy: scrub the leaky default

The fallback `host:<hostname>:pid-<pid>-<uuid8>` exposes hostname and PID. **When writing memories to a shared or upstream database, set an opaque `--agent-id` or `AI_MEMORY_AGENT_ID`.** Tracking issue: #198.

```bash
# Production daemon — opaque identity
AI_MEMORY_AGENT_ID=fleet-prod-9 ai-memory serve --bind 0.0.0.0:9077

# Or per-CLI-call
ai-memory --agent-id alice store -T "Auth flow notes" -c "..."
```

## Why this matters

Every other memory product treats AI knowledge as **anonymous text in a vector DB**. ai-memory treats every memory as a **dated, attributable assertion**. When your fleet of agents grows from one to ten to hundreds:

- You can answer *"who wrote this?"* in every recall — instantly.
- You can scope retrieval to *"show me what `ai:secops-skill@*` learned this week."*
- Per-namespace governance can require approval for writes from specific agents (see [Architectures · T2](/docs/architectures/t2-single-node-many-agents) and [T3](/docs/architectures/t3-multi-node-cluster)).
- v0.7 attestation extends `agent_id` from claimed to cryptographically signed — same field, stronger guarantee.

Provenance is infrastructure. `ai-memory` ships it on day one.

## Next

→ [Recall](/docs/user/recall) — full scoring + ranking
→ [Namespaces](/docs/user/namespaces) — scope visibility on top of NHI
→ [Architectures · T2](/docs/architectures/t2-single-node-many-agents) — multi-agent isolation in practice
