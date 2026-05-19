# AI NHI dogfood test — for non-technical readers (2026-05-18)

**Verdict:** Safer to ship now than it was this morning. The dogfood test found 4 real defects; 3 are already fixed in the same release, and the fourth is filed with a concrete fix scoped for the next engineering session.

If you are the curious-end-user, the not-an-engineer-but-still-care reader, or the stakeholder who wants to understand the difference between "tests passed" and "we actually used it ourselves" — this page is for you.

---

## What is a "dogfood test" in 60 seconds

"Dogfooding" is the engineering practice of using your own product before you ship it to anyone else. The phrase comes from a marketing slogan ("eat your own dog food") and what it means is: don't just run automated tests against your code, actually sit down and use it the way a real customer would, then watch what breaks.

For an AI memory system, the natural dogfooder is the AI itself. The AI is the customer here — the AI is the thing that stores, recalls, updates, and reasons over the memories. So an AI NHI dogfood test means: we asked the AI to sit in front of the system and use it for real, and we watched closely.

"NHI" stands for "Non-Human Identity" — the engineering term for any non-human caller of an API. The AI is the NHI in this context. It is a real user of the software, just not a flesh-and-blood one.

This dogfood was the third round of testing in the same day. Earlier in the day, two other test campaigns had run and reported clean: a structured 12-phase playbook (Track A) and a requirements-coverage audit (Gaps 1-7 audit). Both said: nothing wrong. The dogfood was the third pass — the unstructured, free-form, "just go use it" pass — and it found four things.

---

## Why a third pass matters

The first two passes were structured. They asked specific questions and got specific answers. The dogfood was unstructured: the AI was given a fresh database and told to exercise the brand-new provenance features (introduced in v0.7.0) end-to-end, using the same wire interface a real AI customer would use.

This caught problems the structured tests could not have caught, because the structured tests reached around the wire interface and tested the code directly. When you reach around the wire interface, you miss bugs that live in the wire interface itself.

That's exactly the kind of bug we found.

---

## What we found, in plain language

**Finding 1:** When the AI tried to save a fact with a "source document" reference attached (e.g. "this fact came from doc:report-2024-Q3"), the system accepted the save and returned success — but the source-document reference was thrown away. The memory was saved without it. To a downstream reader, it would look as if the fact had no documented source.

**Finding 2:** The system has a safety feature called "expected version" that lets the AI say "only update this memory if its version is still 5, otherwise tell me there's a conflict." That safety feature was implemented and working — but the wire interface did not advertise the parameter, so a fresh AI session reading the manual would not know the feature existed. (Once told, the AI could use it; the gap was discoverability, not function.)

**Finding 3:** The documentation for the "supersede" feature said the system writes a particular kind of relationship-row when one memory replaces another. It does not — and it shouldn't, because the database has a structural rule that would reject it. The actual lineage (which memory replaced which) is preserved through a different mechanism that works correctly. The documentation was wrong; the behavior was right. We fixed the documentation.

**Finding 4:** All of the above lives in the standard database (SQLite). The system also supports a second, optional database backend (PostgreSQL with a graph extension called Apache AGE) for larger deployments. That backend has not yet received the same provenance upgrades the standard backend got today. The gap is real, the work to close it is about 600 lines of code, and it is filed as the next engineering task.

---

## What we did about it

| Finding | Status | Fixed by |
|---------|--------|----------|
| 1 (source-URI dropped) | Fixed and verified end-to-end | Commit `39aa158f9` |
| 2 (version + edit-source not advertised) | Fixed and verified end-to-end | Commit `39aa158f9` |
| 3 (supersede docstring drift) | Fixed; documentation now matches behavior | Commit `19b08543c` |
| 4 (PostgreSQL + AGE backend lag) | Filed; scoped; assigned to next engineering session | Tracked under issue `#894` |

Findings 1, 2, and 3 were fixed in the same release the dogfood was run against. The test was retested after the fix landed, with hand-written SQL queries verifying the database actually held what the API claimed to hold. The retest passed.

Finding 4 is the honest one. It is filed. It is scoped. It is not labelled "later" or "next release"; it is the next thing the next engineering session will work on. The PostgreSQL backend does not silently rot in the meantime — it simply does not yet expose the new provenance columns, which means a deployment using that backend would not get the v0.7.0 provenance benefits until the catch-up work lands.

---

## Why this is safer to ship than it was this morning

This morning, the system had four invisible defects. They were invisible because the structured tests had passed and the requirements-coverage audit had passed, and nobody had run the AI end-to-end against the wire interface yet. The defects existed and would have shipped.

This evening, three of the defects are fixed and pinned by tests, and the fourth is filed with a precise scope. A future engineer cannot lose track of finding 4 because it has its own issue number. A future regression on findings 1-3 cannot land silently because there are now tests that would fail.

That is the difference between a product where bugs are invisible and a product where bugs are tracked. The dogfood was the mechanism that converted invisible to tracked.

---

## What we did NOT test in this campaign — honest disclosure

This campaign was scoped to the v0.7.0 provenance write and read paths against the sqlite backend. Three things were deliberately out of scope and are not yet covered:

1. **The PostgreSQL + Apache AGE backend** was not exercised, because the v0.7.0 provenance migrations (schema v44 through v47) have not landed on that backend yet. That is what finding 4 (issue #894) tracks. Until the migrations land, dogfooding the PG+AGE path would only confirm the gap finding 4 already documents.
2. **The recall-observations live test** (Gap 3) was run through the new MCP tool's parameter branches via unit tests, but a live end-to-end probe that actually populates the `recall_observations` table during a real recall and then reads it back was not run in this dogfood. The unit-test path covers the same dispatch the live tool uses, but the live-data round-trip is queued for the next pass.
3. **Signed-link attestation-level decoration** (the system's ability to surface "this link is signed" on the recall response) was not exercised, because the dogfood test corpus contained no signed links. To exercise this surface, the dogfood would need to first sign a link via the governance subsystem and then recall it; that work is queued for a follow-on dogfood pass.

These are not "out of scope" in the dismissive sense. They are work that has not been done yet, and saying so plainly is the rule the prime directive imposes on this project.

---

## For the curious

- Engineering writeup: [`audience-engineer.md`](audience-engineer.md) — full technical detail, every commit, every reproduction step.
- Campaign index: [`README.md`](README.md).
- C-level / decision-maker view: [`audience-c-level.md`](audience-c-level.md).
- Flat list of every finding: [`findings.md`](findings.md).

---

*Drafted by Claude Opus 4.7 (1M context) on 2026-05-18, in plain English on purpose. If a sentence on this page felt like it was hiding something, that's a bug — please file an issue.*
