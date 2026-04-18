---
sidebar_position: 2
title: Upgrade guide
description: Upgrade ai-memory between versions safely.
---

# Upgrade guide

## Schema migrations

ai-memory's schema is automated and additive. Old databases open in new code. New databases open in old code as long as new fields are gracefully defaulted.

### v0.5.4.x → v0.6.0

Six migrations run on first start:

| Version | Adds |
|---|---|
| v7 | `metadata` JSON column |
| v8 | `pending_actions` table (governance queue) |
| v9 | `approvals` array column |
| v10 | `scope_idx` generated column + index |
| v11 | `sync_state` table (vector clocks) |
| v12 | `last_pushed_at` watermark column |

**All migrations are idempotent, additive, transactional, and default-safe.**

Worst-case upgrade time on a 10M-row database: **1–3 seconds** (dominated by v10 index build).

### Recommended upgrade procedure

```bash
# 1. Stop applications
systemctl stop ai-memory
systemctl stop my-app

# 2. Backup
cp ~/.local/share/ai-memory/memories.db ~/memories.db.backup

# 3. Upgrade binary
brew upgrade ai-memory   # or apt/dnf/cargo/etc.

# 4. Trigger migration
ai-memory list --limit 1   # opens DB, runs migrations

# 5. Restart
systemctl start ai-memory
systemctl start my-app
```

## Breaking changes

### v0.6.0

- **Consensus governance now requires agent pre-registration** ([#234](https://github.com/alphaonedev/ai-memory-mcp/issues/234)). Existing `consensus:N` policies will become indefinitely-locked unless the approver agents are registered first.

  ```bash
  ai-memory agents register --agent-id alice --agent-type human
  ai-memory agents register --agent-id bob --agent-type human
  ai-memory agents register --agent-id carol --agent-type human
  ```

- **Agent type whitelist closed** ([#235](https://github.com/alphaonedev/ai-memory-mcp/issues/235)) — only 6 hardcoded values. Use `system` for custom agents until v0.6.1.

## Downgrade

Downgrades are not supported. Restore from backup if needed.
