---
sidebar_position: 9
title: AI developer governance
description: Authority classes, attribution, and hard prohibitions for AI-assisted development.
---

# AI developer governance

ai-memory's governance model assumes humans + AI agents share the contributor pool. To keep that working, every AI-assisted PR follows the same governance.

## Execution model

> **Human-led, AI-accelerated.** Humans maintain full oversight. AI coding agents (Claude Code, OpenAI Codex, xAI Grok, others) are tools under human direction — not autonomous developers.

- **Owner & Gatekeeper:** `@alphaonedev` approves all merges to `main` (CODEOWNERS-enforced)
- **Architect:** humans make all design decisions
- **Quality gate:** humans vet code against [Engineering Standards](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/ENGINEERING_STANDARDS.md)
- **Contributors:** human developers + human-supervised AI sessions, same PR process

## Authority classes (per change)

| Class | Examples | Approval |
|---|---|---|
| **Trivial** | typo, comment, formatting | self-merge OK on develop |
| **Standard** | new CLI flag, additive schema, feature flag | maintainer review |
| **Sensitive** | new tool, governance change, schema migration | extra review + tests |
| **Restricted** | crypto, sync protocol, public API | architect sign-off |

## Attribution — every AI commit MUST end with

```
Co-Authored-By: Claude Sonnet 4.6 (1M context) <noreply@anthropic.com>
```

(or whichever model). Every AI-authored PR includes the **AI involvement** section described in [`AI_DEVELOPER_WORKFLOW.md` §8.2](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/AI_DEVELOPER_WORKFLOW.md).

## Hard prohibitions

These never happen, regardless of human approval:

- ❌ Skip CI hooks (`--no-verify`)
- ❌ Bypass commit signing
- ❌ Force-push to `main`
- ❌ Self-merge without CODEOWNERS approval
- ❌ Commit secrets / `.env` / credentials
- ❌ Modify production data without staging soak

## Memory governance

The product enforces its own governance rules — see [Admin → Governance](/docs/admin/governance) for runtime policies.

## Why this matters

ai-memory itself could enable AI-led development teams someday — agents that remember what they built, why they built it, and what broke last time. The persistent memory that makes autonomous AI development viable may be the product we're building.

That future isn't here yet. When it arrives, ai-memory will be the infrastructure it runs on. Until then: **humans approve, AI assists, every change auditable**.
