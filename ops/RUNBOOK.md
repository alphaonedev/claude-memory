# ai-memory-mcp v0.6.3 Grand-Slam — Operator Runbook

> **The operational harness has moved.** The Python package at
> `agentic-mem-labs/tools/campaign/` is now the canonical implementation.
> See its [README](https://github.com/alphaonedev/agentic-mem-labs/blob/main/tools/campaign/README.md)
> for the full operator guide. This file is kept as a thin pointer plus
> the per-user launchd plist for this repo.

---

## Quickstart

Assumes both `agentic-mem-labs` and `ai-memory-mcp` are cloned under your
home directory (defaults: `~/agentic-mem-labs`, `~/ai-memory-mcp`).

```bash
export PYTHONPATH=$HOME/agentic-mem-labs/tools

# Pre-flight (8 health checks)
python -m campaign preflight

# Detached launch
python -m campaign start

# Live-render the newest iter log (TUI)
python -m campaign watch

# Status / stop
python -m campaign status
python -m campaign stop
```

## 24×7 via launchd

The plist `com.alphaone.claude-campaign-fate.plist` (in this directory)
invokes `python -m campaign run` with the right `PYTHONPATH`.

```bash
cp ~/ai-memory-mcp/ops/com.alphaone.claude-campaign-fate.plist \
   ~/Library/LaunchAgents/com.alphaone.claude-campaign.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.alphaone.claude-campaign.plist
```

Status: `launchctl print gui/$(id -u)/com.alphaone.claude-campaign`
Stop:   `launchctl bootout gui/$(id -u)/com.alphaone.claude-campaign`

## Approval scope, hard rules, observability, risks, reset

All canonical — see the [package README](https://github.com/alphaonedev/agentic-mem-labs/blob/main/tools/campaign/README.md)
and the campaign charter at
`agentic-mem-labs/strategy/2026-04-25/ai-memory-v0.6.3-grand-slam.md`.

## History

The original shell harness (`run-campaign.sh` + `start.sh` + `stop.sh`)
that landed in PR #380 was retired in favour of the Python package
(`campaign` ≥ 1.0.0, Apache 2.0, © AlphaOne LLC). Same `.agentic/`
state directory layout, same kill-switch convention.
