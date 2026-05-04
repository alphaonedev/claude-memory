# ai-memory Roadmap — RETIRED, see [ROADMAP2.md](ROADMAP2.md)

**Status:** This file has been retired (2026-05-04). The single canonical roadmap is now [`ROADMAP2.md`](ROADMAP2.md).

## Why this file was retired

The Phase 0–6 framing in this document last reflected reality at v0.5.4 / v0.6.0. As of 2026-05-04 we are between v0.6.3 (shipped 2026-04-27 with 1,886 tests + 93.84% coverage + A2A 48/48 mTLS + LongMemEval R@5 97.8%) and v0.6.4 (shipping Mon–Fri 2026-05-04 → 2026-05-08, code-name `quiet-tools` — token economics + NHI guardrails phase 1).

Five milestones (v0.6.0.1, v0.6.1, v0.6.2, v0.6.3, v0.6.3.1) shipped between this document's last revision and the present. Three more (v0.6.4, v0.7, v0.8) have published charters this document doesn't reference. The Phase-N framing also conflicts with the version-tag framing used everywhere else in the project (CHANGELOG, release branches, tag schedule, public landing pages). Maintaining two parallel structures was creating drift.

## Where to look instead

| You want to know… | Read this |
|---|---|
| What ships next, when, with what scope | [`ROADMAP2.md`](ROADMAP2.md) §7 (release plan v0.6.3.1 / v0.6.4 / v0.7 / v0.8 / v0.9 / v1.0) |
| What's actively in flight this week | [`ROADMAP2.md`](ROADMAP2.md) §7.2 (v0.6.3.1) and §7.2.5 (v0.6.4) |
| What ships forever as OSS, and the trademark moat | [`ROADMAP2.md`](ROADMAP2.md) §14, §15 |
| The audit findings driving each release | [`ROADMAP2.md`](ROADMAP2.md) §5.1–§5.6 |
| Behavioral evidence (agent-side, complementary to substrate audit) | [`ROADMAP2.md`](ROADMAP2.md) §5.6, [v0.6.3.1 OpenClaw behavioral assessment](https://alphaonedev.github.io/ai-memory-a2a-v0.6.3.1/nhi/openclaw-behavioral-v0.6.3.1/) |
| North star + design philosophy | [`ROADMAP2.md`](ROADMAP2.md) §1, §2 |
| Active sprint detail for the v0.6.4 release | [`docs/v0.6.4/v0.6.4-roadmap.md`](docs/v0.6.4/v0.6.4-roadmap.md) and the per-task [NHI execution prompts](docs/v0.6.4/v0.6.4-nhi-prompts.md) |

## Historical archive

The pre-2026-05-04 content of this file (Phase 0–6 framing, v0.5.4 baseline) is preserved in git history. Last meaningful commit: see `git log -- ROADMAP.md`. The cross-walk from the old Phase-N items to the current release plan is documented in [`ROADMAP2.md`](ROADMAP2.md) §6 ("Recovered commitments from the prior phased roadmap").

— AlphaOne LLC, 2026-05-04
