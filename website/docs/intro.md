---
slug: /
sidebar_position: 1
title: What is ai-memory?
description: Persistent, peer-synced memory for AI agents. Local-first. Apache-2.0.
---

# What is ai-memory?

> **AI endpoint memory.** Every AI agent gets persistent, synced memory — the same way every network endpoint gets an IP address. A primitive, not a product.

AI agents are stateless by default. Every session starts from zero. Models get replaced. Vendors shut down. Infrastructure gets rebuilt. The knowledge disappears with them.

**ai-memory makes knowledge persistent.** What agents learn survives the agent, the model, the vendor, and the platform. One agent learns it, every agent knows it — across systems, across teams, across time.

> No AI agent should ever have to relearn what any AI agent already knows.

## Two day-one differentiators

**🆔 Every recall shows you which AI learned the memory.**
Provenance is built in. Every memory carries `metadata.agent_id` — claimed at write, immutable across update / sync / consolidate / import, and surfaced as the trailing column on the default TOON-compact recall format. No other memory product ships AI provenance on day one. → [Agent identity (NHI)](/docs/user/agent-identity)

**📥 Five-minute onboarding: paste your old conversations.**
`ai-memory mine` ingests Claude, ChatGPT, and Slack exports into ranked, tiered, recall-ready memories. Switch tools without losing context. Start your next agent with everything the last one already knew. → [Import your conversation history](/docs/user/import-history)

## Three audiences

This documentation site is organized for three readers:

| If you are a… | Start here |
|---|---|
| **User** running ai-memory on your laptop or workstation | [User → Quickstart](/docs/user/quickstart) |
| **Admin** deploying ai-memory for a team or organization | [Admin → Deployment](/docs/admin/deployment) |
| **Developer** building on top of ai-memory or contributing | [Developer → Architecture](/docs/developer/architecture) |

## Design philosophy

**Zero-cost memory for AI agents:**

- **Zero tokens** until recall
- **Zero infrastructure** (single SQLite file)
- **Zero latency** (local-first, no network)
- **Zero lock-in** (works with any MCP-compatible AI)
- **Zero knowledge loss** (agents die, memories survive)

SQLite is the backbone. Local-first is the moat. Every feature preserves this.

## What you get out of the box

- **23 MCP tools** for AI-native memory management
- **24 HTTP API endpoints** for external integration
- **26 CLI commands** for local operation and scripting
- **4 feature tiers** (keyword → semantic → smart → autonomous)
- **TOON format** for token-efficient structured responses (~79% smaller than JSON)
- **Hybrid recall** combining semantic + keyword + graph traversal
- **Peer-to-peer sync mesh** (v0.6.0+) with HTTPS / mTLS
- **Apache-2.0** licensed, USPTO-trademarked, OIN member

## Get started

→ [Install ai-memory](/docs/user/install)
→ [Run the quickstart](/docs/user/quickstart)
→ [Set up a peer mesh](/docs/admin/peer-mesh)
