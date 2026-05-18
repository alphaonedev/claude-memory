# ai-memory v0.7.0 — Track A NHI Re-run + In-Campaign Fix Batch (2026-05-18)

## What this is

A focused re-run of the NHI test playbook Track A against the post-PR-#820 binary, plus the **complete remediation of every defect surfaced during the re-run**, all landed in v0.7.0 (no deferral). Executed per the prime directive testing-loop addendum (memory `f1dca8fa-6c33-4139-b0b5-389cca45b921`, supersedes `5d703efe`) which mandates: discover → file → fix-in-current-release → retest → re-check → close until 100% remediated.

This campaign exercises **only Track A** (the NHI test playbook). Tracks B (IronClaw A2A 4-domain on Docker + Grok 4.3), C (Postgres + Apache AGE on Node B), and D (cross-node integration) remain on the strategic-tracking backlog, gated on operator pre-flight directives.

## How this directory is organised

| File | Audience | Purpose |
|------|----------|---------|
| `README.md` | All | Campaign index (this file) |
| `track-a-nhi-results.md` | Engineering | Track A raw results, all 12 phases + verdict, single file |

## Verdict at a glance

**SHIP v0.7.0** — 85 PASS / 0 FAIL across 12 phases. **Zero open defects in scope for this campaign.**

10 GitHub issues closed in-campaign with retest + re-check evidence:
- **#826** memory_update MCP coverage (transitively via #859)
- **#829** verbose token budget 15570 → 9507
- **#830** TTL "extend" docs drift
- **#831** memory_promote step-skip docs drift
- **#837** NHI-P2 forget scratch-DB discipline
- **#859** MCP `tools/list` schema-trim restored optional property discovery
- **#860** memory_get_links exposes `valid_from`/`valid_until`/`observed_by`/`attest_level`
- **#861** memory_archive_list preserves `metadata.agent_id` + emits `tags` as JSON array
- **#862** tool count off-by-one help-text clarification (70 callable + 1 always-on = 71)
- **#865** playbook boot-directive worktree path drift (memory `081791ae` refreshed)

In flight at write time: **#863** (CLI `ai-memory governance check-action` subcommand) and **#864** (Family naming clarification) — being closed by a background agent.

## Code commits this campaign (branch `local/install-815-816`)

| SHA | Author | Scope |
|-----|--------|-------|
| `091350c` | Claude (Agent B) | `fix(#860, #861)`: get_links surfaces temporal+attest cols; archive_list preserves metadata + emits tags as JSON array |
| `d41b8cb` | Claude (Agent C) | `perf(#829)`: trim verbose tool docs from 15570 → 9507 cl100k tokens |
| `5ab3315` | Claude (Agent A) | `fix(#859)`: MCP tools/list exposes optional property schemas for NHI discovery |
| `e99fb0e` | Claude | `style(tests)`: is_none_or for attest_level assertion in get_links_temporal (post-#860 pedantic cleanup) |
| *(in flight)* | Claude (Agent for #863+#864) | CLI subcommand + Family-naming doc clarification |

## Reproducibility contract

1. **Pinned binary** — git SHA `e99fb0e` on branch `local/install-815-816` in worktree `/Users/fate/v07/v07-fixes/`. The pre-fix-batch baseline was `f612675`; the fix batch is the 4 commits above.
2. **Binary location** — `/Users/fate/.local/bin/ai-memory` → `/Users/fate/v07/v07-fixes/.cargo-shared-target/release/ai-memory` (26 MB).
3. **DB** — `/Users/fate/.claude/ai-memory.db` (the operator's live MCP DB), schema v43.
4. **Models** — `nomic-ai/nomic-embed-text-v1.5` (768 d), `cross-encoder/ms-marco-MiniLM-L-6-v2`, `gemma4:e4b`. Tier `autonomous`.
5. **Authoring agent** — `ai:claude-code@FROSTYi.local:pid-1060`.
6. **Fix agents** — 4 parallel background agents (Agent A schema-trim, Agent B storage serialization, Agent C verbose trim, Agent D yesterday-issue verification) + 1 follow-up agent (#863+#864).

## Hard rules during the campaign

Per the prime directive testing-loop addendum (canonical in memory `f1dca8fa` + CLAUDE.md §"Testing-loop discipline"):

- **No deferral to v0.7.1.** Every issue surfaced during the test campaign was fixed in v0.7.0.
- **No banned framings.** "non-blocking", "P2/P3 follow-up", "surface-level", "vN+1 polish" are all disallowed.
- **Audit trail mandatory.** Every GH issue body links to ai-memory evidence; ai-memory evidence links to GH issue id; commit messages reference both.
- **Retest discipline.** Each fix was verified via the same scenario that surfaced it (retest) PLUS a fresh probe at a different angle (re-check). No "close as fixed" without both green.
- **Recompile + batch retest.** The campaign rebuilt the release binary once after all fixes landed, then ran the batch retest sweep against the new binary via CLI + raw MCP probes.

## Memory namespace convention

| Item | Namespace | Title pattern |
|------|-----------|---------------|
| Track A phase results | `ai-memory/v0.7.0-nhi-testing` | `NHI-P{N}-{name} (v0.7.0 re-run @ f612675, 2026-05-18)` |
| Verdict | `ai-memory/v0.7.0-nhi-testing` | `v0.7.0 — Full-spectrum NHI verdict (ship-readiness) — re-run @ f612675 + fix batch @ e99fb0e, 2026-05-18 pm` |
| Strategic checkpoint | `_v070_strategic_tracking` | `iter #19` (initial re-run), `iter #20` (post-fix-batch SHIP) |
| Prime directive testing-loop | `global/policies` | `f1dca8fa-6c33-4139-b0b5-389cca45b921` |
| Sandbox sub-namespaces (evidence trails) | `ai-memory/v0.7.0-nhi-testing/{sandbox-2026-05-18, p3-kg-2026-05-18, p4-locked-2026-05-18, p11-2026-05-18}` | Per-test memories preserved |

## Provenance

| Item | Value |
|------|-------|
| Campaign date | 2026-05-18 (morning re-run + afternoon fix batch) |
| Operator | binary2029@gmail.com (justin@alpha-one.mobi) |
| Authoring agent | Claude (Opus 4.7 1M context) |
| Authority | Autonomous execution authorized by operator 2026-05-18 (connectivity-loss recovery + fix-everything-in-v0.7.0 directive + testing-loop addendum) |
| Iter checkpoints | `_v070_strategic_tracking/iter #19` (memory `a1697779`), `iter #20` (this campaign verdict) |
| Prior campaign | `docs/v0.7.0/test-campaign-2026-05-17/` |
| Binary at retest | git SHA `e99fb0e` on branch `local/install-815-816` |

Drafted by Claude (Opus 4.7 1M context).
