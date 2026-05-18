# <Campaign name> — for non-technical readers (<DATE>)

**Verdict:** SHIP / NO-SHIP.

> SKELETON. Target length: 600–800 words, plain English, zero jargon. Replace each `<placeholder>` and each guidance bullet with real content drawn from `track-*-results.md`. Do not invent claims beyond what the engineering writeup supports.

---

## What is "<short topic>" in 60 seconds

<One short paragraph. Define every technical term you use, in everyday language. If you can't define it without using more jargon, drop it.>

---

## What the test campaign asked

<One question, expressed in plain English. Then a numbered list of the test areas / phases, each in 1 line.>

1. <area 1>
2. <area 2>
...

---

## What the verdict was

**SHIP / NO-SHIP. N tests passed. M tests failed.**

<One paragraph on what was opened/fixed/retested/closed during the campaign. If issues remain open, list them honestly.>

---

## What it means for the user

If you're going to use <feature/system> for real work, the <release> is/isn't a system you can trust because:

- <bullet 1: concrete behavioral guarantee>
- <bullet 2>
- <bullet 3>

---

## A note on <any companion work that landed in the same release>

<Plain-English explanation of the companion work. Use one analogy that helps; do not chain multiple analogies. Cite the issue numbers + the file path of the deep writeup.>

---

## What we did NOT test in this campaign — honest disclosure

<Be specific. List anything that was on the wider scope but not exercised. Use the format "X is not done because Y" — not "X is out of scope.">

1. <gap 1>
2. <gap 2>

---

## For the curious

- Engineering writeup: [`track-*-results.md`](track-*-results.md)
- Campaign index: [`README.md`](README.md)
- C-level / decision-maker view: [`audience-c-level.md`](audience-c-level.md)
- Deep-dive engineering view: [`audience-sme-engineer.md`](audience-sme-engineer.md)

---

*Drafted by <agent name> on <date>, in plain English on purpose. If a sentence on this page felt like it was hiding something, that's a bug — please file an issue.*
