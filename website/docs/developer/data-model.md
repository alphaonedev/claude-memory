---
sidebar_position: 2
title: Data model
description: Memory schema, links, metadata, scope, governance.
---

# Data model

## Memory (15 fields)

| Field | Type | Description |
|---|---|---|
| `id` | UUID | Primary key |
| `tier` | enum | `short` / `mid` / `long` |
| `namespace` | string | Hierarchical or flat |
| `title` | string | Required, used for upsert dedup |
| `content` | string | Required |
| `tags` | string[] | Comma-separated on input |
| `priority` | int | 1–10 |
| `confidence` | float | 0.0–1.0 |
| `source` | string | `user` / `claude` / `hook` / `api` / `cli` / `import` / `consolidation` / `system` / `sync` |
| `metadata` | JSON | Flexible: `agent_id`, `scope`, `governance`, custom fields (v0.6.0+) |
| `access_count` | int | Auto-incremented on recall |
| `created_at` | RFC3339 | |
| `updated_at` | RFC3339 | |
| `last_accessed_at` | RFC3339 | |
| `expires_at` | RFC3339? | nullable for `long` |

**Upsert semantics:** Storing with same `(title, namespace)` updates existing. Tier never downgrades (takes `max`). `expires_at` only cleared if new tier is `long`.

## MemoryLink

Typed directional relationship between two memories:

| Relation | Meaning |
|---|---|
| `related_to` | Loose connection |
| `supersedes` | A replaces B |
| `contradicts` | A and B disagree |
| `derived_from` | A was built from B (e.g., consolidation) |

## Metadata JSON (v0.6.0+)

Flexible per-memory storage. Reserved keys:

| Key | Purpose |
|---|---|
| `agent_id` | NHI identity of the agent that stored it (immutable after first set) |
| `scope` | Visibility scope: `private` / `team` / `unit` / `org` / `collective` |
| `governance` | Per-namespace policy (set on the namespace standard memory) |
| `imported_from_agent_id` | Original claim preserved when `import` restamps |
| `consolidated_from_agents` | Source authors preserved on consolidation |
| `mined_from` | Source format tag (`claude` / `chatgpt` / `slack`) |

User-defined keys are preserved across update / sync / consolidate.

## Schema versions

Schema lives in `src/db.rs`. Current = **v12** (v0.6.0). Migration history:

| Version | Added |
|---|---|
| v1–v3 | Initial schema |
| v4 | Archive table |
| v5 | namespace_meta |
| v6 | parent_namespace |
| v7 | metadata column |
| v8 | pending_actions |
| v9 | approvals |
| v10 | scope_idx + index |
| v11 | sync_state |
| v12 | last_pushed_at |

All migrations are idempotent + transactional. See [Upgrade guide](/docs/admin/upgrade).
