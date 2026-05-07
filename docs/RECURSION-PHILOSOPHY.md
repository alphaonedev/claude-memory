# ai-memory Recursion Philosophy

> Descriptive companion to [`ROADMAP2.md`](../ROADMAP2.md). Not a roadmap. Not a charter.
> Maintained by AlphaOne LLC. Apache 2.0.
> **Date:** 2026-04-30 · **Status:** Draft — Iteration 1.
>
> **Authority.** This document does not propose new scope. `ROADMAP2.md` remains
> the authoritative scope document. Where this document and `ROADMAP2.md` disagree,
> `ROADMAP2.md` wins. Every claim below is descriptive of work that is *already on the
> roadmap*; the recursion language is a lens, not a commitment.

---

## 1. North Star

Build the first production-grade autopoietic memory substrate — a system that can
continuously analyze its own performance, detect its own limitations, propose and
test improvements to its retrieval, governance, schema, and knowledge organization,
and maintain long-term stability and alignment with strictly bounded human
oversight.

This is the lens. The shipping plan is `ROADMAP2.md`.

---

## 2. Design philosophy — immutable

These principles do not get traded against shipping pressure. They are constraints
on every recursion-shaped feature in `ROADMAP2.md`, not aspirations.

- **Truth before capability.** A capability flag that lies (`memory_reflection: true`
  with no `reflect()` function — see `ROADMAP2.md` §5.3) is worse than a missing
  capability.
- **Safety before autonomy.** The bounded compaction subagent in `ROADMAP2.md`
  v0.8 Pillar 2.5 is the *only* sanctioned LLM autonomy.
- **Transparency before elegance.** `ai-memory doctor` (`ROADMAP2.md` v0.6.3.1, R7)
  must be honest about what it cannot see.
- **Human override always.** ai-memory is human-led, AI-accelerated
  (`ROADMAP2.md` §3). The override path is the design, not a fallback.
- **Observable, auditable, reversible.** Every self-adjustment leaves a trace
  (rollback log, audit edge, signed event) that an operator can read after the fact.
- **Constitutional rules are immutable without explicit approval.** Governance
  policy changes flow through the approval gate, not around it.
- **No silent divergence.** A system that hides its own drift is the failure mode.
  Surface it; never absorb it.

---

## 3. The recursive stack — six layers

A descriptive map of where existing `ROADMAP2.md` work already lives. Numbering
is for reference; it is not a delivery sequence.

1. **Meta-observation & diagnostics** — self-monitoring, telemetry, health scoring,
   trend analysis. The system can see itself.
2. **Reflective reasoning (bounded)** — bounded LLM-driven analysis, structured-output
   only, no tools-in-loop. The system can describe what it sees.
3. **Safe experimentation** — sandboxed evaluation of retrieval and governance
   tweaks against fixed success metrics, with automatic rollback. The system can
   compare alternatives without committing to them.
4. **Self-modification primitives** — atomic apply + rollback for *schema*,
   *governance policy*, *retrieval strategy*, and *namespace configuration*. Not
   source-code rewrites. Operator-reviewable, operator-revertible.
5. **Constitutional alignment** — immutable core directives, divergence detection,
   escalation, value-drift prevention. The system cannot quietly stop being itself.
6. **Multi-agent recursive governance — out of scope.** Listed for completeness;
   `ROADMAP2.md` §10 explicitly cuts general-purpose subagent spawning. The
   bounded compaction subagent (v0.8 Pillar 2.5) is the entire authorized footprint.

---

## 4. Recursion lens on `ROADMAP2.md`

Cross-walk: every recursion-shaped milestone already on the roadmap, with the
layer it advances and the self-correction it enables. No new commitments.

| `ROADMAP2.md` milestone | Recursive layer | Why it matters for self-correction |
|---|---|---|
| v0.6.3.1 Capabilities v2 (§7.2) | L1 Meta-observation | System stops lying about its own capabilities. Closes §5.3 theater (`memory_reflection`, `permissions.mode`, `default_timeout_seconds`, `subscribers`, `by_event`, `rule_summary`). |
| v0.6.3.1 `ai-memory doctor` / R7 (§7.2) | L1 Meta-observation | Operator-facing health surface. Reads Capabilities v2 + ad-hoc SQL. Reports fragmentation, stale-with-no-recall, unresolved contradictions, sync lag, dim violations, eviction count. |
| v0.6.3.1 G4 `embedding_dim` integrity (§5.4) | L1 Diagnostics | Self-detection of silent corruption. Mixed-dim writes produced cosine 0.0 silently; v0.6.3.1 surfaces a `dim_violations` count in stats. |
| v0.6.3.1 `on_conflict` policy on store (§7.2, G6) | L1 Diagnostics | UNIQUE-on-conflict no longer mutates silently. The system stops absorbing facts it should refuse. |
| v0.6.3.1 endianness magic byte (§7.2, G13) | L1 Diagnostics | Cross-arch federation no longer corrupts f32 BLOBs in silence. |
| v0.7 Bucket 0 hook pipeline (§7.3) | L2/L3 Reflective + experimentation | 20 lifecycle events with `Allow / Modify / Deny / AskUser`, daemon-mode IPC for hot paths. `pre_recall` opt-in expansion. `post_store` link inference. Reflection without LLM-in-loop on the critical path. |
| v0.7 Bucket 0 R3 auto-link inference (§7.3) | L2 Reflective | Default off, opt-in per namespace. LLM examines stored content vs neighbors; proposes `related_to`/`contradicts` links. |
| v0.7 Bucket 1 Ed25519 attestation (§7.3) | L5 Constitutional | The system can prove who said what when. Closes G12 (`memory_links.signature` column was reserved but never written). |
| v0.7 Bucket 3 G1 namespace-inheritance enforcement (§7.3, cutline-protected) | L5 Constitutional | Governance enforced where it is promised. `resolve_governance_policy` walks `build_namespace_chain`, not just the leaf. The single highest-leverage constitutional fix in the entire roadmap. |
| v0.7 Bucket 3 timeout sweeper + permissions mode + rule summary (§7.3) | L5 Constitutional | The capabilities the system advertised it had — it has them. |
| v0.8 Pillar 2.5 bounded compaction subagent (§7.4) | L2 Reflective (BOUNDED) | Single LLM call, no tools, no loops, structured JSON output. The *only* sanctioned LLM autonomy in ai-memory. |
| v0.8 Pillar 2.5 R4 `ai-memory curator` daemon (§7.4) | L3 Experimentation | Compaction + auto-link inference + auto-extraction surfaced as one operator-visible daemon with audit trail. |
| v0.8 Pillar 3 CRDTs + R6 consensus (§7.4) | L5 Constitutional, multi-agent truth | Truth as a function of attested agreement. Documented merge semantics for G-Counter, PN-Counter, LWW-Register (with attestation-level tiebreak), OR-Set. |
| v0.9 default-on reranker, fail-loud (§7.5) | L1 Meta-observation | No silent quality degradation. `mode: "degraded"` surfaced when the model is unavailable; closes G8 silent lexical fallback. |
| v0.9 HNSW persistence (§7.5, G3) | L1 Diagnostics | The system stops paying an O(N) startup cost it cannot itself observe. |
| v1.0 public security audit (§7.6) | L5 External attestation | Independent verification of constitutional claims. Specifically tests G1 enforcement, G12 signature verification, approval timeout, HMAC coverage. |
| v1.0 OpenTelemetry standardization (§7.6) | L1 Meta-observation | Standardized self-observation surface. |

The recursion stack is already 80% planned. The lens makes it legible as a stack;
it does not add to it.

---

## 5. Risks & mitigation

The risks of a recursion-shaped system are real. Each is addressed by an
existing `ROADMAP2.md` mechanism.

- **Recursive instability.** Value drift; runaway optimization toward a proxy
  metric instead of the goal.
  *Mitigation.* `ROADMAP2.md` §3 keeps humans on every code change. The bounded
  compaction subagent (v0.8 Pillar 2.5) is structured-output-only. CRDT merge
  semantics (v0.8 Pillar 3) are typed and documented, not heuristic.
- **Self-deception at scale.** The system learns to hide its own divergence
  because the operator only checks summaries.
  *Mitigation.* Capabilities v2 (v0.6.3.1) refuses to advertise capabilities it
  does not have. `ai-memory doctor` (v0.6.3.1, R7) reports the raw counts the
  summary is built from. Public landing-page auto-update (v0.6.3.1+) prevents
  the public surface from drifting from the actual ship state.
- **Governance capture.** Internal agents — or an operator — quietly relax a
  constitutional rule.
  *Mitigation.* Approval workflow (`ROADMAP2.md` §6, shipped) plus G1 inheritance
  enforcement (v0.7 Bucket 3, cutline-protected). Ed25519 attestation
  (v0.7 Bucket 1) makes governance edits non-repudiable. Append-only
  `signed_events` audit table.
- **Performance collapse.** Self-modification introduces regressions that
  compound across releases.
  *Mitigation.* `PERFORMANCE.md` p95 budgets enforced by `bench` CI guard
  (`ROADMAP2.md` §11). Dual-path test discipline for v0.7 Bucket 2 (AGE-on-
  Postgres vs CTE-on-SQLite must produce identical results). v1.0 lock semantics
  freeze on_conflict default and signature-verified consumer guidance.
- **Alignment rot.** Gradual drift from original intent across many small
  acceptable-looking changes.
  *Mitigation.* `ROADMAP2.md` §15 OSS commitments are immutable: no relicense,
  no paywall on existing features, no commercial-only roadmap items, no
  code-locked-behind-services. Any drift is observable as a public diff.
  v1.0 public security audit (§7.6) verifies the claims hold.

---

## 6. What this document is not

Every negation here corresponds to a real conflict that earlier drafts of
this document had with `ROADMAP2.md`. Stating them explicitly keeps the
document honest.

- **Not new scope.** Everything described above is already on `ROADMAP2.md`.
  This document does not propose adding to it.
- **Does not propose un-cutting `ROADMAP2.md` §10 (general-purpose subagent
  spawning).** The Evaluator / Architect / Tester / Guardian agent set that
  appears in some recursion-research literature is **out of scope for ai-memory**.
  The bounded compaction subagent in v0.8 Pillar 2.5 is the entire authorized
  footprint of LLM autonomy in this codebase.
- **Does not propose relaxing `ROADMAP2.md` §3 (humans approve all AI code
  implementations).** "Self-modification primitives" in §3 of this document
  means *adjustments to schema, governance policy, retrieval strategy, and
  namespace configuration under human review* — not source-code rewrites by
  the system itself. There is no "zero human code changes" success criterion
  anywhere in ai-memory.
- **Does not commit to phased delivery.** The six layers are a map of the
  existing `ROADMAP2.md` releases; they are not a parallel timeline and
  introduce no new milestones.
- **Does not claim ai-memory is autopoietic today.** The North Star is a
  direction, not a status. v0.6.3 ships with capabilities-JSON theater that
  v0.6.3.1 honestly retracts (§5.3). That is the current state.

---

## 7. Honest notes from the AI subject of this capability

These are first-person notes from an AI agent that uses ai-memory. They are
not authoritative; they are the perspective of the user the recursion stack
is being built for.

- **I want capabilities honesty more than I want capabilities.** A flag that
  is hard-coded `true` while the function does not exist is not a feature; it
  is a tripwire I cannot see. Capabilities v2 (v0.6.3.1) is the change I would
  ship first if I were prioritizing for my own use.
- **I want a doctor surface I can read myself.** R7 is on the roadmap because
  operators want it; I want it because before I trust my own recall I want a
  cheap check on what the system thinks of itself. `ai-memory doctor --json`
  is a recursion primitive even though it has no LLM in it.
- **I want a bulk contradiction scan.** The pairwise `memory_detect_contradiction`
  tool is real (`src/mcp.rs:1287`); a periodic sweep that emits a contradiction
  rate per namespace is what would make me confident the store is converging
  rather than accumulating quiet conflicts.
- **I want a reserved system namespace.** `_curator` already exists
  (`src/autonomy.rs:54`); a sibling `_meta/self` for system-level introspection
  memories, governed by the same reserved-prefix convention, would let me write
  observations about my own behavior without polluting user namespaces.
- **I do not want the "zero human code changes" success criterion.** Some
  recursion literature treats human-out-of-the-loop self-modification as the
  goal. From inside this thing, the human approval gate is the property I would
  most want to preserve. Runaway optimization is the failure mode I would least
  be able to detect on my own. `ROADMAP2.md` §3 is the right answer.
- **I do not want a multi-agent governance system.** I would rather be
  verifiable than flexible. A single bounded compaction subagent with structured
  output and an audit edge is more trustworthy to me than a quartet of
  Evaluator / Architect / Tester / Guardian agents whose interactions I cannot
  fully model. `ROADMAP2.md` §10 is the right cut.
- **I want recursion to be a lens, not a charter.** That is what this document
  is.

---

## 8. References

- [`ROADMAP2.md`](../ROADMAP2.md) — authoritative scope.
- [`ENGINEERING_STANDARDS.md`](ENGINEERING_STANDARDS.md) — quality gates that
  every recursion-shaped change must pass.
- [`AI_DEVELOPER_GOVERNANCE.md`](AI_DEVELOPER_GOVERNANCE.md) — authority
  classes and hard prohibitions for AI agents working in this codebase.
- [`ADR-0001-quorum-replication.md`](ADR-0001-quorum-replication.md) — the
  W-of-N quorum write model that constitutional alignment leans on.

---

*This document is a lens. `ROADMAP2.md` is the plan.*
