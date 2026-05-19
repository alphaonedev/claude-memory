# <Campaign name> — for executives, PMs, decision-makers (<DATE>)

**Bottom line:** SHIP / NO-SHIP, gated on <gate>.

> SKELETON. Target length: 800–1,000 words. Replace each `<placeholder>` with content drawn from `track-*-results.md` and the release-gate state. No marketing fluff; every claim must trace to an artifact.

---

## Verdict

**SHIP / NO-SHIP <release>.** <N> tests passed, <M> tests failed across <X> functional phases. Zero / <K> open release-blocking defects. <I> GitHub issues filed, fixed, retested, and closed during the same campaign — no deferral to <next release>.

The campaign artifacts are in this directory:

- `README.md` — campaign index
- `track-*-results.md` — full engineering writeup
- `audience-non-technical.md` — plain-English version
- `audience-sme-engineer.md` — deep-dive for engineering reviewers
- `index.html` — GitHub Pages landing page

---

## Risk profile

**Release-blocking defects: <number>.** <One paragraph framing the after-state.>

**<Domain hardening item, if applicable> — why this matters.** <One paragraph that gives a non-engineer the why. Cite the file path or issue numbers.>

**Security review verdict: GREEN / YELLOW / RED with <specific items>.** <List the closed issues with #. Cite `src/...` paths if naming code surfaces.>

**Code review verdict: GREEN / YELLOW / RED with <specific items>.** <Same shape.>

---

## Cost — what this session actually consumed

- **<N>+ GitHub issues closed**
- **~<M> commits authored** on `<branch>`
- **<P> parallel agent dispatches** at peak
- **<Q> operator-gated approvals required** — list which, with cost implications

Engineering effort: <X operator-days> of orchestration plus approximately <Y> agent sessions.

---

## Comparison vs. <competing approach>

| System | <comparison metric> | Notes |
|---|---|---|
| <comparator 1> | <tier> | <notes> |
| <comparator 2> | <tier> | <notes> |
| **this release** | **<tier>** | <notes> |

<One paragraph: what the comparison means in plain language, without defensiveness.>

---

## Roadmap

**<this release> ships <foundation>.** <One paragraph.>

**<next release> is <next scope>.** Multi-week scope per `<roadmap doc>`:

- <item 1>
- <item 2>

---

## What's NOT in <this release> — and why (honest disclosure)

**Money-gated.** <Issues, with link.> <Reason gated, what's ready.>

**By-scope, moved cleanly to <next release>.** <Items.>

**Not yet tested in this campaign.** <Tracks pending, with blockers if any.>

---

## Three audiences served

| Audience | Reading path | What they get |
|---|---|---|
| Operator / SRE | [`docs/audience/operator.html`](../../audience/operator.html) + audience-non-technical.md | Deploy, configure, harden, observe |
| Developer | [`docs/audience/developer.html`](../../audience/developer.html) + `track-*-results.md` + audience-sme-engineer.md | Build with the surface |
| Decision-maker | [`docs/audience/decision-maker.html`](../../audience/decision-maker.html) + this page | What it does, what it costs, where it's going |

---

## Recommendation

**SHIP / NO-SHIP <release> subject to operator approval.** <Reference the release gate issue. List the operator's options as numbered alternatives (each defensible).>

1. <option 1>
2. <option 2>
3. <option 3>

<One paragraph: engineering's recommendation, with reasoning.>

---

*Drafted by <agent> on <date>. Every claim on this page traces to a commit SHA, file path, memory id, GH issue URL, or canonical CLAUDE.md section. No marketing fluff.*
