---
sidebar_position: 5
title: Namespaces
description: Organize memories by team, project, agent, or hierarchy.
---

# Namespaces

Every memory belongs to a namespace. Namespaces partition the memory space and are the basis for visibility, governance, and recall scoping.

## Flat vs hierarchical

Flat namespaces (any string):
```bash
ai-memory store -T "title" -c "content" --namespace "my-project"
```

Hierarchical namespaces (`/`-delimited, v0.6.0+):
```bash
ai-memory store -T "title" -c "content" --namespace "acme/engineering/platform"
```

Max depth: **8** levels.

## Visibility scopes (v0.6.0+)

When storing, set `--scope`:

| Scope | Visible to |
|---|---|
| `private` *(default)* | exact namespace match only |
| `team` | parent namespace + all descendants |
| `unit` | grandparent + subtree |
| `org` | great-grandparent + subtree |
| `collective` | every agent everywhere |

Recall queries with `--as-agent` apply these rules automatically.

## Namespace standards

Set a "standard" memory at any namespace level — it's auto-prepended to every recall scoped to that namespace. Useful for project rules, team policies, organizational standards.

> **v0.6.0 note:** standard configuration is currently MCP-only. CLI/HTTP support is tracked in issue #236.

## Three-level rule layering

Standards cascade: **global** (`*`) → **parent** → **namespace**. A query at `acme/engineering/platform` returns global rules + acme's rules + the platform-specific rules, layered.
