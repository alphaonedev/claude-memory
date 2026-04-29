---
sidebar_position: 7
title: Import your conversation history
description: Make every AI you've ever talked to remember from day one. Paste your Claude / ChatGPT / Slack export — get an instant, ranked, recallable knowledge base.
---

# Import your conversation history

> **Five minutes to a useful agent.** Export your old AI conversations, run one command, and your next agent starts with everything the last one already knew.

`ai-memory mine` parses Claude, ChatGPT, and Slack exports into ranked, tiered, recall-ready memories. No re-typing. No copy-paste. No loss of context across model changes.

## Why retroactive import matters

Every AI conversation you've had is a body of decisions, corrections, preferences, and small facts your future agents will need. Today, when you switch tools or start a new context window, that knowledge evaporates. `mine` ingests it into your local SQLite store — and from that moment, *every* MCP-compatible AI you talk to recalls it.

This is the fastest path from *"new tool, blank slate"* to *"new tool, every project context I've ever explained."*

## Five-minute onboarding

```bash
# 1. Drop your export into the working directory
ls conversations.json   # ChatGPT export
# or
ls claude-export/conversations.jsonl   # Claude export
# or
ls slack-export.zip   # extract first → ls slack-export/

# 2. Dry-run to see what will land
ai-memory mine ./conversations.json --format chatgpt --dry-run

# 3. Import for real
ai-memory mine ./conversations.json --format chatgpt

# 4. Confirm + recall
ai-memory stats
ai-memory recall "what database did we decide on for the analytics service"
```

That's it. Your next agent starts with an opinion.

## What gets imported

Each conversation that survives the `--min-messages` threshold (default 3) becomes one memory:

| Field | Source |
|---|---|
| `title` | First user message, truncated |
| `content` | Full conversation text — alternating user/assistant turns |
| `tier` | `mid` by default (7-day TTL, auto-promotes to `long` after 5 recalls) |
| `namespace` | Auto-set per format: `claude-export`, `chatgpt-export`, `slack-export` |
| `source` | `mine-claude` / `mine-chatgpt` / `mine-slack` |
| `metadata.mined_from` | Source format tag |
| `metadata.agent_id` | The caller's identity (you) |
| `created_at` | Original conversation timestamp |

Every imported memory is **fully integrated** with the rest of the system: scope visibility, governance gates, recall scoring, deduplication on `(title, namespace)`, link creation, consolidation. `mine` is not a sidecar — it's a first-class write path.

## Per-format recipes

### Claude export

Claude exports give you a `conversations.jsonl` file (one JSON object per line — one conversation per line).

**Where to find it:**
1. Open Claude → Settings → Privacy → Export data
2. Wait for the email; download the zip
3. Inside, find `conversations.jsonl`

**Import:**

```bash
# Dry run first — see what will be imported, count + sizes
ai-memory mine ./claude-export/conversations.jsonl --format claude --dry-run

# Real import, default mid-tier
ai-memory mine ./claude-export/conversations.jsonl --format claude

# Or pin everything to long-tier (no TTL) into a custom namespace
ai-memory mine ./claude-export/conversations.jsonl \
  --format claude \
  --namespace personal/claude-history \
  --tier long \
  --min-messages 5
```

### ChatGPT export

ChatGPT exports give you a `conversations.json` file (a single JSON array of conversations).

**Where to find it:**
1. Open ChatGPT → Settings → Data Controls → Export data
2. Wait for the email; download the zip
3. Inside, find `conversations.json`

**Import:**

```bash
# Dry run
ai-memory mine ./chatgpt-export/conversations.json --format chatgpt --dry-run

# Real import
ai-memory mine ./chatgpt-export/conversations.json --format chatgpt

# Filter to substantive conversations only (10+ messages)
ai-memory mine ./chatgpt-export/conversations.json \
  --format chatgpt \
  --min-messages 10 \
  --tier long
```

### Slack export

Slack workspace exports give you a directory of per-channel `.json` files. `mine` walks the tree.

**Where to find it:**
1. Slack → Workspace settings → Import / Export Data → Export
2. Pick "Standard export"; wait for the email
3. Download + unzip the archive

**Import:**

```bash
# Dry run on the whole export
ai-memory mine ./slack-export/ --format slack --dry-run

# Real import — narrow to a high-signal channel
ai-memory mine ./slack-export/eng-decisions/ --format slack \
  --namespace work/slack/eng-decisions \
  --tier long \
  --min-messages 4
```

## Flags reference

| Flag | Default | Meaning |
|---|---|---|
| `--format`, `-f` | (required) | `claude`, `chatgpt`, `slack` |
| `--namespace`, `-n` | format-specific | Override target namespace |
| `--tier`, `-t` | `mid` | `short` / `mid` / `long` |
| `--min-messages` | `3` | Skip conversations shorter than this |
| `--dry-run` | `false` | Print what would be imported, don't write |

The miner stamps `metadata.agent_id` to the caller's identity (see [Agent identity](./agent-identity)) and records the original conversation timestamp on `created_at`.

## After the import

Your imported memories are immediately part of the live recall mix:

```bash
# Recall ranks across imported + native memories together
ai-memory recall "kubernetes deployment strategy" --tier long

# Filter to just the imported set
ai-memory list --namespace chatgpt-export
ai-memory list --source mine-chatgpt

# Promote the standout decisions to long-tier permanently
ai-memory promote <id> --tier long --priority 9

# Consolidate two redundant decisions into one canonical memory
ai-memory consolidate <id1> <id2> -T "Auth strategy — final decision"
```

## A pattern that works

1. **Mine your last 90 days first.** Set `--min-messages 5` to skip the trivial chatter.
2. **Pin the survivors.** Run `ai-memory recall` for your most common project topics; promote the top hits to long-tier.
3. **Consolidate the duplicates.** Three memories about the same auth flow? `ai-memory consolidate` collapses them — and keeps the lineage in `metadata.consolidated_from_agents`.
4. **Wire your next AI.** Whatever comes next — Claude, GPT, Grok, a local Llama — points at the same SQLite file and inherits everything you imported.

This is the workflow Mem0 charges you SaaS pricing for. ai-memory does it locally, in one binary, in five minutes.

## Next

→ [Quickstart](./quickstart) — store + recall basics
→ [Tiers](./tiers) — when to use short / mid / long
→ [Recall](./recall) — full scoring + ranking
→ [Agent identity](./agent-identity) — every memory carries provenance
