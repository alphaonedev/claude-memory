---
sidebar_position: 7
title: Governance
description: Per-namespace policies, approvers, and consensus voting.
---

# Governance

Phase 1 (v0.6.0) introduces per-namespace governance: **who can write, promote, delete, and approve changes** at each level of your hierarchy.

> **v0.6.0 limitation:** Governance configuration is currently **MCP-only**. CLI/HTTP support is tracked in [#236](https://github.com/alphaonedev/ai-memory-mcp/issues/236).

## Policy structure

Each namespace can define a `governance` policy in its standard memory's metadata:

```json
{
  "governance": {
    "write": "owner",
    "promote": "approve",
    "delete": "owner",
    "approver": "human",
    "owner": "alice"
  }
}
```

## Levels

| Level | Meaning |
|---|---|
| `any` | anyone can perform this action (default) |
| `registered` | only registered agents (`agents register`) |
| `owner` | only the namespace owner |
| `approve` | queued in `pending_actions` for approver decision |

## Approver types

| Approver | Behavior |
|---|---|
| `human` | a human approves via `pending approve <id>` |
| `agent:<id>` | a specific designated agent must approve |
| `consensus:N` | **N distinct registered agents** must agree |

## Consensus voting (breaking change)

> **v0.6.0 breaking change:** `consensus:N` now requires **agent pre-registration** ([#234](https://github.com/alphaonedev/ai-memory-mcp/issues/234)).

Before deploying a consensus policy, register the approver agents:

```bash
ai-memory agents register --agent-id alice --agent-type human
ai-memory agents register --agent-id bob --agent-type human
ai-memory agents register --agent-id carol --agent-type human
```

Then they vote:

```bash
ai-memory --agent-id alice pending approve <action-id>
ai-memory --agent-id bob pending approve <action-id>
ai-memory --agent-id carol pending approve <action-id>
# Action auto-executes when N=3 distinct registered approvers vote yes
```

Vote counting is **case-insensitive** (`alice` == `ALICE` == `Alice` — counts as one).

## Why governance matters

Without it, any agent can write anything anywhere. That's fine for a single agent. For 100 agents in an organization, it's chaos.

Governance is the difference between collective intelligence and collective noise. **But it must be flexible** — a startup with 3 agents wants zero friction. An enterprise with 1000 agents wants approval chains. The governance model is **metadata, not code** — configured per namespace, not hardcoded.
