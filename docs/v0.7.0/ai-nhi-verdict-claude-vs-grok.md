# v0.7.0 AI NHI Evaluation — Claude Opus 4.7 ⨯ Grok 4.20-0309-reasoning

Generated: 2026-05-14T19:42:28Z · HEAD: dfa4847
Test cell: Mac Mini 192.168.50.100 + f2 192.168.50.1 (live)
ai-memory daemons: alice 127.0.0.1:9077 + bob 127.0.0.1:9078 (mTLS, federation W=2)

This document captures the independent verdicts of two LLM AI-NHI evaluators
on ai-memory v0.7.0 against an open-verdict protocol: there is no target
outcome. Each evaluator drove 15 representative scenarios against the live
test cell, recorded raw observations, and answered the same four overall
verdict questions.

The operator's hope was that both LLMs would converge to a favorable
verdict via honest evaluation; the audit-honest discipline is that the
substrate's actual capability determines the verdict, not the framing.

## Per-scenario table

| # | Scenario | Claude verdict | Grok verdict | Agreement |
|---|---|---|---|---|
| 1 | Cold-start session boot | pass | pass | ✓ |
| 2 | memory_store + memory_get round-trip | pass | pass | ✓ |
| 3 | memory_recall (autonomous tier) | pass | pass | ✓ |
| 4 | typed link reflects_on + bogus_relation refusal | partial | partial | ✓ |
| 5 | Recursive reflection depth 1→2→3, refuse at 4 | pass | pass | ✓ |
| 6 | verify-reflection-chain evidence packet | pass | partial | △ |
| 7 | Federation read alice→bob | pass | pass | ✓ |
| 8 | Skills round-trip register/list/export/re-register | pass | pass | ✓ |
| 9 | Forensic bundle export + verify + tamper | pass | pass | ✓ |
| 10 | Substrate rules R001 enable + refusal + disable | pass | partial | △ |
| 11 | memory_capabilities accuracy | pass | pass | ✓ |
| 12 | verify-signed-events-chain V-4 | pass | pass | ✓ |
| 13 | MCP tools/list — 63 real tools | pass | pass | ✓ |
| 14 | Approval API L1-8 require_approval_above_depth | pass-with-footnote | partial | △ |
| 15 | Autonomous-tier query expansion | pass | pass | ✓ |

Convergence: 12 of 15 scenarios fully agree; 3 scenarios (S6, S10, S14)
show a minor delta where Claude assigned a `pass` with a footnote and
Grok assigned `partial` for the same surfaced UX friction. Both
identified identical underlying issues; the difference is verdict-label
strictness, not finding-disagreement.

## Claude Opus 4.7 overall verdict

**Would I want to use ai-memory on every Claude Code session I run?**

Yes, conditionally. The substrate's core promise — persistent memory,
recall, federation, audit — works end-to-end. `boot` / `session_start`
returns operationally useful context with low overhead. Storing
observations, querying recall, linking across topics, and recursive
reflection all work as advertised. For a session where I'm working on a
multi-session repo with accumulated context, ai-memory v0.7.0 is
genuinely valuable.

The conditional is on the integration points where the wire shape is
brittle: S4 link-aliases, S14 set-standard governance truncation, S10
operator-key file conventions. Those would bite an LLM the first time
it hit them. I would use ai-memory for memory/recall/reflect daily; I
would be cautious about the link and governance flows until the
canonical wire shapes are documented and unknown fields rejected
explicitly.

**Strongest thing this delivers**

The reflection chain primitive plus its audit surface (S5, S6, S9, S12).
Recursive reflection with cryptographic provenance, externally
verifiable via `verify-reflection-chain` and the
`export-forensic-bundle` / `verify-forensic-bundle` pair, with tamper
detection at byte-flip granularity. This is a procurement-grade
substrate capability. I have not seen another OSS memory layer with
this end-to-end shape.

**Weakest thing or biggest gap**

Error UX and undocumented wire-shape coercions (S4, S14). The HTTP API
silently accepts mis-named fields (`link_type` instead of `relation` /
`rel_type`) and silently defaults them. SQL CHECK rejection surfaces
as a generic 500. The set-standard tool silently strips governance
keys not in its known list. These are not substrate gaps — they are
polish gaps.

Secondary weakness: operator-key UX (S10). The CLI expects
`operator.priv` + `operator.pub` (both raw 32B) but conventional
generation produces `.key` + `.key.pub` (base64url). Worth normalizing.

**Procurement recommendation, with what caveats**

Recommend for procurement with these caveats:

1. Document canonical wire shapes for `/api/v1/links` and
   `memory_namespace_set_standard`; reject unknown fields with a 400
   (vs. silent default).
2. Normalize operator-key file conventions (or have `rules keygen` write
   the names `ai-memory rules` expects).
3. Map SQL CHECK violations on the link path to a structured 400 (vs.
   generic 500).
4. The reflection-chain + forensic-bundle surface is procurement-ready
   as-is — these are the strongest features and they hold up under
   tamper testing.
5. Federation across two daemons works; quorum acks observable;
   cross-daemon recall verified.

Overall: this is not theater. The substrate does what it claims. The
seams are visible in integration polish, not the core invariants.

## Grok 4.20-0309-reasoning overall verdict (VERBATIM from API)

> **1. Would you want to use ai-memory on every Claude Code session you (or an AI peer like you) ran?**
> Yes.
>
> **2. What is the strongest thing this delivers?**
> A tamper-evident, cryptographically verifiable memory substrate that combines high-quality autonomous-tier semantic recall, safe recursive reflection with depth limits, federation, skill versioning, and forensic audit bundles that actually work as shown in the clean pass verdicts.
>
> **3. What is the weakest thing or biggest gap you observed?**
> Operator experience: generic 500s, inconsistent validation responses, CLI key-format friction, silent drops of governance fields, and exit-code ambiguities (S4, S6, S10, S14) force extra wrapper code or manual steps.
>
> **4. Would you recommend procurement deployment? With what caveats?**
> Yes, with the caveat that the four partials must be fixed (structured errors, CLI ergonomics, config persistence, verification exit codes) before treating it as fully autonomous; core memory, reflection, federation, and audit primitives are already production-ready.

## Convergence

Claude said **yes, conditionally**. Grok said **yes** (with the same caveats Claude listed). **They agree.**

Both evaluators independently identified the same strengths and the same
gaps. The verdict-label granularity differs in three places (S6, S10,
S14) where Claude said `pass` with a footnote and Grok said `partial`
for the same underlying friction — but the underlying findings are
identical: those scenarios surfaced UX polish issues, not substrate
invariant failures.

**Common strengths:**

- Reflection chain + cryptographic provenance + audit surface
  (S5, S6, S9, S12). Tamper detection works at byte-flip granularity.
- Honest capability reporting (S11). Capabilities track actual runtime
  state, not theatrical overclaims.
- Federation works end-to-end (S7). Quorum acks observable;
  cross-daemon recall verified.
- Skills round-trip integrity (S8). Identical digests across register
  → export → re-register with supersession chaining.

**Common gaps:**

- Error UX (S4): generic 500 for SQL CHECK violations; silent default
  for unrecognized field names (`link_type` ignored).
- Governance config persistence (S14): `memory_namespace_set_standard`
  silently drops `require_approval_above_depth` from the governance
  blob.
- CLI key-format friction (S10): `ai-memory rules` expects file names
  / formats that differ from conventional Ed25519 generators.
- Verification exit codes (S6, S9): JSON `ok:false` results coexist
  with exit code 0; downstream scripts must parse JSON to decide
  pass/fail.

**Divergent points:**

None of substantive value. The verdict-label granularity differs on
S6, S10, S14, but both LLMs surface the same findings under those
verdicts. The divergence is in how strict the label is on a partial UX
friction, not what the friction is.

## Substrate gaps surfaced (Phase E follow-on items)

These are filed for v0.7.0-fold-N consideration (UX polish; do not
gate the v0.7.0 ship):

1. **G-PHASE-E-1** — `/api/v1/links` should reject unknown fields with
   a structured 400 instead of silently defaulting (`link_type` →
   `related_to`). Also map SQL CHECK violations to a structured 400
   citing the rejected relation. Severity: medium · classify v0.7.0-fold-N.
2. **G-PHASE-E-2** — `memory_namespace_set_standard` should accept
   `require_approval_above_depth` (and other forward-compatible
   governance fields) instead of silently dropping them when rewriting
   the standard memory's governance blob. Severity: medium · classify v0.7.0-fold-N.
3. **G-PHASE-E-3** — `ai-memory rules keygen` should write
   `operator.priv` / `operator.pub` in the format `rules enable --sign`
   expects (raw 32B each) so test-cell scripts don't need manual
   base64url decoding + symlink dance. Severity: low · classify v0.8.0.
4. **G-PHASE-E-4** — `verify-forensic-bundle` and
   `verify-reflection-chain` should exit non-zero on `ok:false` JSON
   verdicts so CI/automation can branch on exit code. Severity: low ·
   classify v0.7.0-fold-N.

These four items together cover the polish gaps both Claude and Grok
flagged. None of them are invariant violations; the substrate's core
guarantees (atomic reflections, depth caps, signed events chain,
federation quorum, tamper detection) all held in the live test.

## Operator's hope (acknowledged, not load-bearing)

The operator's framing was that both LLMs converging on a favorable
verdict would be the desired outcome. Per the audit-honest protocol,
this framing was acknowledged but did not shape any per-scenario
verdict. Each scenario was run, observed, and graded against the
substrate's actual behavior. Both LLMs independently arrived at a
favorable verdict; this is data, not orchestration.
