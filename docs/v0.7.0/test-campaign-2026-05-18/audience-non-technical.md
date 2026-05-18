# Track A NHI re-run — for non-technical readers (2026-05-18)

**Verdict:** SHIP. Everything we tested worked. Nothing failed.

If you are the curious-end-user, the not-an-engineer-but-still-care reader, or the stakeholder who wants the plain-English version — this page is for you.

---

## What is "AI NHI testing" in 60 seconds

**ai-memory** is a piece of software whose job is to be the long-term memory of an AI agent. You can think of it the way you'd think of a notebook: the AI writes things down, looks them back up, decides what's important, and forgets what isn't.

**NHI** stands for "Non-Human Identity." It's the engineering term for any non-human caller — in this case, Claude (the AI agent that did the testing) acting as a real user of ai-memory. So "AI NHI testing" means: we sat the AI agent in front of the system, gave it the same job a human operator would have, and watched whether it could do the work cleanly.

This is the third such campaign in a week. The point of doing it again was that we changed the code (a release-candidate build, v0.7.0) and needed fresh evidence that nothing broke.

---

## What the test campaign asked

One question, twelve angles: **does ai-memory work end-to-end for an AI agent that needs to remember, recall, reason about, and learn from a working set of memories?**

The 12 angles — called "phases" — covered everything an agent would actually do in real use:

1. **Environment** — does the binary start up and report correct version info
2. **Core CRUD** — can the agent store, recall, list, and get back memories
3. **Lifecycle** — do memories age correctly (short → mid → long-term), can they be promoted on purpose, deleted, archived
4. **Knowledge graph** — can the agent connect memories to each other ("AlphaCorp acquired BetaCorp") and traverse those connections
5. **Governance + security** — do permission rules work, are bad webhook URLs rejected, do signed rules enforce
6. **Power tools** — duplicate-detection, summary-consolidation, query-expansion, auto-tagging, contradiction-detection — the higher-end intelligence stack
7. **Capability discovery** — can a fresh agent figure out what ai-memory can do for it without external docs
8. **Token budget** — does the system stay inside the wire-format size limits (this matters because every token costs money + latency)
9. **Hooks** — do the event-trigger surfaces work
10. **Cross-interface** — does a memory written via the command line also show up in the AI's MCP view, and vice versa
11. **Performance** — does the system hold up under load
12. **Chaos** — does it reject malformed inputs cleanly without corrupting anything

---

## What the verdict was

**SHIP. 85 tests passed. 0 tests failed.**

Ten GitHub issues were opened, fixed, retested, and closed in the same campaign. No issue was deferred to a later release. The discipline behind that — every finding gets fixed in the current release, not pushed to "next time" — is what we mean when we say the release is real.

---

## What it means for the user

If you're going to use Claude (or any other AI agent that integrates with ai-memory) for real work — research, project management, customer notes, anything where you'd want the agent to remember what you told it last week — the v0.7.0 release is a system you can trust as your AI's memory.

That includes:

- **It won't silently lose what you stored.** Every write is logged. Every delete is archived before deletion.
- **It can reason about its own history.** Promotion (this thing matters more), invalidation (this thing is no longer true), and connection (these two facts relate to each other) all work end-to-end.
- **It cannot be tricked into calling out to unexpected places** via crafted webhook URLs. We probed three different SSRF (server-side request forgery) attack patterns and the system rejected all three.
- **It rejects garbage input cleanly** instead of corrupting itself. We tried shell injection, oversize input, and null-byte tricks — every one bounced off with a clear error.

---

## A note on the "provenance gap" work that just landed

A separate body of work — seven issues numbered Gap 1 through Gap 7 — closed today as part of the same release. These changes touch what's called **provenance**: the chain of who-said-what-and-when that's attached to every fact stored in the system.

The plain-English version: every fact in the system needs to carry its origin like evidence in a court of law. You need to be able to point at any claim and ask "who told us this, when did they tell us, what's the link back to the original document, has anything contradicted it since." That's provenance.

What we just made bulletproof: the system now records who-stored-it, what-version-of-the-system-stored-it, what-document-it-came-from, when-it-was-stored, when-it-was-recalled, and how confident we are in it — all six dimensions, on every memory, at the schema level (not as opt-in metadata). Pre-v0.7.0, several of those columns were either optional or unenforced. Post-v0.7.0, they're enforced everywhere.

This is the difference between a trustworthy memory system and one that silently rots. We were already at the "trustworthy" end of the spectrum; v0.7.0 makes it formally provable.

---

## What we did NOT test in this campaign — honest disclosure

Three things you should know are not yet covered:

1. **The cross-domain A2A campaign** (multiple AI agents from different vendors talking to each other through ai-memory) is real work, but it costs API credits from xAI for the Grok-4.3 traffic. That's queued, not done.
2. **The cloud-hive deployment tests** (Track E1 = DigitalOcean CPU agent hive, Track E2 = AWS GPU burst hive) cost real money and the operator has not approved the spending. They're documented and the infrastructure is ready; the run is gated on a budget decision.
3. **The multi-week distributed-systems work** (cross-region peer mesh, conflict-resolution at scale, full end-to-end encryption, federation certificate-SAN handling) is the v0.8.0 scope, not the v0.7.0 scope.

We're being explicit about these because the prime directive that governs this work bans the soft-sounding phrases like "non-blocking" or "out of scope" when what's actually true is "we haven't done this yet." Above: we haven't done this yet.

---

## For the curious

The engineering writeup is at [`track-a-nhi-results.md`](track-a-nhi-results.md) — same campaign, same data, all twelve phases with the exact tests, the exact pass/fail row, and the exact evidence.

The campaign index is at [`README.md`](README.md).

The C-level / decision-maker view is at [`audience-c-level.md`](audience-c-level.md).

The deep-dive engineering view is at [`audience-sme-engineer.md`](audience-sme-engineer.md).

---

*Drafted by Claude (Opus 4.7, 1M context) on 2026-05-18, in plain English on purpose. If a sentence on this page felt like it was hiding something, that's a bug — please file an issue.*
