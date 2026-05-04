# ai-memory v0.6.4 — `quiet-tools`

**Status:** sprint authorized 2026-05-02; refactored 2026-05-04 against verified v0.6.3.1 source; dev cycle Mon 2026-05-04 → Fri 2026-05-08
**Theme:** cross-harness token economics + AI NHI capability-discovery protocol phase 1
**Issue count:** **17** (16 base + v0.6.4-017 G9 HTTP webhook parity, source-anchored fold-in)

## The single doc to feed an agent

If you are bootstrapping a fresh agent (human or NHI) for this epic, hand them **one file**:

> [`V0.6.4-EPIC.md`](V0.6.4-EPIC.md)

That document contains:
- v0.6.3.1 source-anchored ground truth (so the agent does not redo shipped work)
- Refactored 17-issue scope (incl. v0.6.4-017 HTTP webhook parity)
- Day-by-day schedule with explicit gates
- Inline Day-0 kickoff prompt
- Guardrails (Principle 1, Principle 6, hard prohibitions, four mandatory gates)
- Risk register and success metrics
- Definition of done

Everything else in this directory is reference material that `V0.6.4-EPIC.md` points to.

## Why this release

Boris Cherny's published 90-day instrumentation data (May 2026) quantified that 73% of Claude Code tokens go to nine waste patterns. ai-memory is the **#1 contributor to Pattern 6** ("just-in-case" tool definitions) on every coding-agent harness except Claude Code's deferred-tool path: ~25,800 input tokens per request just for tool schemas. v0.6.4 fixes this in one release.

## Headline change

Default profile flips from **43 tools** → **5 tools** (`memory_store`, `memory_recall`, `memory_list`, `memory_get`, `memory_search`). Other 38 tools available via:

- `--profile graph|admin|power|full` for static profile selection
- `memory_capabilities --include-schema family=<name>` for runtime discovery (NHI canonical path)

**Backward compatibility:** `ai-memory mcp --profile full` reproduces v0.6.3 surface 1:1.

## Documents in this release package

| File | Purpose |
|---|---|
| [`V0.6.4-EPIC.md`](V0.6.4-EPIC.md) | **Master framework. Single self-contained doc to feed an agent for cold-start bootstrap.** |
| [`rfc-default-tool-surface-collapse.md`](rfc-default-tool-surface-collapse.md) | Design RFC. Profiles, discovery dance, NHI guardrails, tier applicability matrix (T1–T6). |
| [`v0.6.4-roadmap.md`](v0.6.4-roadmap.md) | 17-issue sprint plan, 5-day schedule, test/cert/release plan, risk register, success metrics. |
| [`v0.6.4-nhi-prompts.md`](v0.6.4-nhi-prompts.md) | Self-contained AI NHI starter prompts for the dev cycle (Day 0 kickoff + Mon–Fri tracks + emergency-break recovery prompt). |

## Reading order

1. **First time on this release (any reader):** read `V0.6.4-EPIC.md`. It is the framework.
2. **Reviewing scope / approving (human):** `V0.6.4-EPIC.md` then skim `v0.6.4-roadmap.md`.
3. **Operating an NHI dev sprint:** dispatch `v0.6.4-nhi-prompts.md` per day.
4. **Need design rationale:** read `rfc-default-tool-surface-collapse.md`.

## Source-anchored ground truth (verified 2026-05-04)

These v0.6.3.1 items are SHIPPED — `V0.6.4-EPIC.md` §1 has line-citations for each:

- G4 (embedding_dim guard), G5 (archive lossless), G6 (on_conflict), G13 (endianness magic byte)
- G9 webhook coverage on the **MCP path** (memory_delete / promote / link / consolidate)
- R1 (budget_tokens), R7 (ai-memory doctor), Capabilities v2 honesty

The newly-surfaced **G9 HTTP gap** (zero `dispatch_event` calls in `handlers.rs`) is filed as v0.6.4-017 and lands Day 1 afternoon.

## Token economics — projected

| Harness | v0.6.3 baseline | v0.6.4 default (`core`) | Reduction |
|---|---|---|---|
| Claude Code (deferred-tools) | ~0 (already lazy) | ~0 | n/a |
| Claude Desktop (eager) | ~25,800 | ~3,250 | **~87%** |
| OpenAI Codex CLI (eager) | ~25,800 | ~3,250 | **~87%** |
| xAI Grok CLI (eager) | ~25,800 | ~3,250 | **~87%** |
| Google Gemini CLI (eager) | ~25,800 | ~3,250 | **~87%** |

Per-user-year savings on Sonnet 4.6 input pricing for a heavy user (~7,500 turns/year): **~$497/year** for users on eager-loading harnesses.

## NHI guardrails phase 1 (v0.6.4)

- Per-agent capability allowlist (config-driven, `agent_id`-keyed)
- Capability-expansion audit log (every `--include-schema` call recorded)

## Deferred to v0.7+

- Rate-limit on capability expansion
- Attestation-tier gating (depends on #238)
- Tier-6 redacted-discovery mode
- G1 (namespace-inheritance enforcement) → v0.7 Bucket 3
- G10 (auto-invoke `memory_expand_query` in recall) → v0.7 Bucket 0
- R2/R3/R4/R5/R6/R8 — see `V0.6.4-EPIC.md` §1.3 for full slipped list

## Sprint status

- [x] RFC drafted + approved
- [x] Roadmap drafted + approved
- [x] NHI prompt deck drafted + approved
- [x] Master framework (`V0.6.4-EPIC.md`) consolidated and source-anchored 2026-05-04
- [ ] 17 GitHub issues filed (Day 0 — Sat/Sun 2026-05-03)
- [ ] Branch `feat/v0.6.4` cut (Day 0)
- [ ] Mon–Fri sprint executes
- [ ] Tag `v0.6.4` Fri 2026-05-08 EOD
- [ ] Public announcement Mon 2026-05-11

## Tracking

- Cert matrix cell: `releases/v0.6.4/` in `alphaonedev/ai-memory-test-hub`
- Memory namespace for cross-NHI continuity: `campaign-v064`
- Milestone: `v0.6.4` on `alphaonedev/ai-memory-mcp`
