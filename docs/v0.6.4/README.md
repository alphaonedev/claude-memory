# ai-memory v0.6.4 — `quiet-tools`

**Status:** sprint authorized 2026-05-02; dev cycle Mon 2026-05-04 → Fri 2026-05-08
**Theme:** cross-harness token economics + AI NHI capability-discovery protocol phase 1

## Why this release

Boris Cherny's published 90-day instrumentation data (May 2026) quantified that 73% of Claude Code tokens go to nine waste patterns. ai-memory is the **#1 contributor to Pattern 6** ("just-in-case" tool definitions) on every coding-agent harness except Claude Code's deferred-tool path: ~25,200 input tokens per request just for tool schemas. v0.6.4 fixes this in one release.

## Headline change

Default profile flips from **42 tools** → **5 tools** (`memory_store`, `memory_recall`, `memory_list`, `memory_get`, `memory_search`). Other 37 tools available via:

- `--profile graph|admin|power|full` for static profile selection
- `memory_capabilities --include-schema family=<name>` for runtime discovery (NHI canonical path)

**Backward compatibility:** `ai-memory mcp --profile full` reproduces v0.6.3 surface 1:1.

## Documents in this release package

| File | Purpose |
|---|---|
| [`rfc-default-tool-surface-collapse.md`](rfc-default-tool-surface-collapse.md) | Design RFC. Profiles, discovery dance, NHI guardrails, tier applicability matrix (T1–T6). |
| [`v0.6.4-roadmap.md`](v0.6.4-roadmap.md) | 16-issue sprint plan, 5-day schedule, test/cert/release plan, risk register, success metrics. |
| [`v0.6.4-nhi-prompts.md`](v0.6.4-nhi-prompts.md) | Self-contained AI NHI starter prompts for the dev cycle (Day 0 kickoff + Mon–Fri tracks + emergency-break recovery prompt). |

## Reading order

1. **First time on this release:** read this README, then `rfc-default-tool-surface-collapse.md`.
2. **Reviewing scope / approving:** read `v0.6.4-roadmap.md`.
3. **Operating an NHI dev sprint:** read `v0.6.4-nhi-prompts.md`.

## Token economics — projected

| Harness | v0.6.3 baseline | v0.6.4 default (`core`) | Reduction |
|---|---|---|---|
| Claude Code (deferred-tools) | ~0 (already lazy) | ~0 | n/a |
| Claude Desktop (eager) | ~25,200 | ~3,250 | **~87%** |
| OpenAI Codex CLI (eager) | ~25,200 | ~3,250 | **~87%** |
| xAI Grok CLI (eager) | ~25,200 | ~3,250 | **~87%** |
| Google Gemini CLI (eager) | ~25,200 | ~3,250 | **~87%** |

Per-user-year savings on Sonnet 4.6 input pricing for a heavy user (~7,500 turns/year): **~$497/year** for users on eager-loading harnesses.

## NHI guardrails phase 1 (v0.6.4)

- Per-agent capability allowlist (config-driven, `agent_id`-keyed)
- Capability-expansion audit log (every `--include-schema` call recorded)

## Deferred to v0.7+

- Rate-limit on capability expansion
- Attestation-tier gating (depends on #238)
- Tier-6 redacted-discovery mode

## Sprint status

- [x] RFC drafted + approved
- [x] Roadmap drafted + approved
- [x] NHI prompt deck drafted + approved
- [ ] 16 GitHub issues filed (Day 0 — Sat/Sun 2026-05-03)
- [ ] Branch `feat/v0.6.4` cut (Day 0)
- [ ] Mon–Fri sprint executes
- [ ] Tag `v0.6.4` Fri 2026-05-08 EOD
- [ ] Public announcement Mon 2026-05-11

## Tracking

- Cert matrix cell: `releases/v0.6.4/` in `alphaonedev/ai-memory-test-hub`
- Memory namespace for cross-NHI continuity: `campaign-v064`
- Milestone: `v0.6.4` on `alphaonedev/ai-memory-mcp`
