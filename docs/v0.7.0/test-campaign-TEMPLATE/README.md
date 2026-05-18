# Test campaign template (v0.7.0+)

> **How to use this template.** Copy this directory to
> `docs/v0.7.0/test-campaign-YYYY-MM-DD/` (or `v0.8.0/...`), then fill in
> each file. Every campaign produces exactly six artifacts. Do not skip
> any; the discipline is the value.

## What lives in this directory

| File | Audience | Purpose |
|------|----------|---------|
| `README.md` | All readers | Campaign index — what was tested, when, by whom, verdict at a glance |
| `track-{lane}-{name}-results.md` | Engineering | Per-track raw results, phase-by-phase, single file per track (e.g. `track-a-nhi-results.md`, `track-b-a2a-results.md`) |
| `audience-non-technical.md` | End users / curious observers | 600–800 words, plain English, zero jargon |
| `audience-c-level.md` | Executive / PM / decision-maker | 800–1,000 words, verdict + risk + cost + roadmap |
| `audience-sme-engineer.md` | SME engineers + architects | 1,500–2,000 words, reproducibility + methodology + data + dispositions |
| `index.html` | GitHub Pages | Landing page, cards into all of the above |

## Why three audience files

Per operator directive 2026-05-18, every test campaign must be readable by three audiences:

1. **Non-technical reader** — gets to understand the verdict and what it means for them without specialist vocabulary
2. **Decision-maker** — gets enough risk, cost, comparison, and roadmap to decide ship/no-ship
3. **Engineer / architect** — gets enough reproducibility, methodology, and data to verify the claim or extend the test

The engineering writeup (`track-*-results.md`) is the single source of truth; the three audience files re-present that truth at three abstraction levels. **They do not invent claims beyond what the engineering writeup supports.**

## What goes in `README.md`

```
# ai-memory <version> — <campaign name> (<date>)

## What this is
<one-paragraph framing: what tracks were exercised, what binary was tested>

## Verdict at a glance
**SHIP** / **NO-SHIP** with one-line justification.

## How this directory is organised
<table referencing the 6 files above>

## Issues closed in this campaign
<numbered list with one-line description each, linked to GH>

## Code commits this campaign
<table: SHA | author | scope>

## Reproducibility contract
<pinned binary SHA, branch, schema version, model versions, DB path, agent_id>

## Hard rules during the campaign
<refer to the prime directive testing-loop addendum, list the in-campaign discipline>

## Memory namespace convention
<table: item | namespace | title pattern>

## Provenance
<table: campaign date | operator | authoring agent | authority | prior campaign>
```

## What goes in `track-*-results.md`

```
# Track <X> — <name> Results (<date>)

<framing paragraph: what the track is, what binary was tested, link to playbook memory>

<phase summary table: phase | status | pass/fail | result memory id>

**Verdict at a glance line.**

---

## How this campaign reached 100% remediation
<if applicable — for in-campaign fix batches>

---

## Phase 0 — <name>
<expected vs actual table, one row per test>

## Phase 1 — <name>
...

(repeat for every phase)

---

## Verdict: **SHIP** / **NO-SHIP**

### Strengths
<bullets>

### Audit trail
<memory ids, GH issues, commits>

### Recommendation
<one paragraph>
```

## What goes in each audience file

See the v0.7.0 reference implementation at:

- `docs/v0.7.0/test-campaign-2026-05-18/audience-non-technical.md`
- `docs/v0.7.0/test-campaign-2026-05-18/audience-c-level.md`
- `docs/v0.7.0/test-campaign-2026-05-18/audience-sme-engineer.md`

The skeleton files in this template directory carry section headers + one-line guidance under each. Replace the guidance with real content; do not delete the section headers.

## What goes in `index.html`

Copy `docs/v0.7.0/test-campaign-2026-05-18/index.html` as the starting point. The page should:

- Hero with verdict pill (SHIP / NO-SHIP, with green or red)
- Issues-closed grid
- Files-in-this-campaign cards (one per audience file + engineering writeup + README)
- Discipline section (testing-loop addendum compliance)
- Provenance reproducibility table
- Related-campaigns links

## Honesty discipline (operator emphasis 2026-05-18 pm)

The three audience files must be **honest about what shipped and what didn't.** Banned phrases (canonical list in CLAUDE.md):

- "non-blocking"
- "P2/P3 follow-up"
- "surface-level"
- "vN+1 polish"
- "DEFER-TO-V080"
- "no network access"
- "operator should..."
- "I lack..." / "I can't..." (without 6-step verification)
- "out of scope" (when scope was actually you-just-haven't-done-it)
- "mostly done" / "partial"

Every claim on every page must trace to: a commit SHA, a file path, a memory id, a test name, or a GH issue URL. If you can't trace it, don't claim it.

## Where this template lives

- This file: `docs/v0.7.0/test-campaign-TEMPLATE/README.md`
- Skeletons: `audience-non-technical.md`, `audience-c-level.md`, `audience-sme-engineer.md` in the same directory
- Reference implementation: `docs/v0.7.0/test-campaign-2026-05-18/`

## Provenance

| Item | Value |
|---|---|
| Template created | 2026-05-18 |
| Template authority | Operator directive 2026-05-18 pm (three-audience requirement) |
| Reference implementation | `docs/v0.7.0/test-campaign-2026-05-18/` |
| Authoring agent | Claude (Opus 4.7, 1M context) |

Apache-2.0, © 2026 AlphaOne LLC.
