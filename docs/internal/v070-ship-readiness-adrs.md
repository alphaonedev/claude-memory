# v0.7.0 Ship-Readiness — Architecture Decision Records

> Companion to the [v0.7.0 review synthesis](../../docs/internal/) and the
> per-cluster fix PRs (#761-#777). The synthesis enumerated seven
> "operator decision points" that no single fix cluster could resolve
> autonomously. This document closes each as a named ADR with rationale,
> alternatives considered, and consequences.
>
> Status: **ACCEPTED, v0.7.0 ship-readiness wave** (issue
> [#767](https://github.com/alphaonedev/ai-memory-mcp/issues/767)).
> Date: 2026-05-15. Author: AI NHI dev team session (Claude Opus 4.7
> 1M context). Reviewer: operator (binary2029@gmail.com).

## ADR-1 — QW-4 disposition: docs-only (no code feature)

**Context.** The v0.7.0 grand-slam release notes and CHANGELOG enumerate
"4 QW items shipped" (QW-1 file-backed reflection export, QW-2 persona,
QW-3 context-offload, QW-4 Tencent competitive positioning). The API-UX
review (synthesis finding API-12 HIGH; review doc lives in the
review-api-ux worktree and is summarised in
[`v070-review-synthesis.md`](v070-review-synthesis.md)) and the
feature inventory open question #9 both flagged that QW-4 has no
corresponding code path — it is the [`docs/positioning.md`](../positioning.md)
+ cookbook update that situates the substrate against TencentDB Agent
Memory. The earlier "4 QW items shipped" framing overcounts by 1 if a
reader interprets "shipped" as "code-evidence implemented".

**Decision.** QW-4 is a **docs + competitive-positioning deliverable, not
a code feature**. The audit's original classification stands. The fix is
to make the public framing honest:

- The CHANGELOG entry for QW-4 already correctly describes the scope as
  "Positioning page update at `docs/positioning.md` adds the TencentDB
  Agent Memory entry alongside the existing landscape comparison." No
  further CHANGELOG edit beyond a one-line clarification.
- The release-notes (`docs/v0.7.0/release-notes.md`) section on QW items
  is extended with an explicit "QW-4 is a docs-only deliverable; the
  three code-bearing QW items are QW-1/QW-2/QW-3" sentence so the count
  is not misread.
- `docs/positioning.md` and the existing cookbook entries remain the
  canonical deliverable.

**Alternatives considered.**

- **(a) Ship a QW-4 code path.** Rejected — QW-4 was scoped as
  competitive positioning from inception; there is no code feature
  Tencent's positioning suggests we lack. Inventing one for symmetry
  would be a procurement lie.
- **(b) Re-attribute QW-4 to `MCP_MUTATION_DISABLED_ERROR` or another
  refusal token.** Rejected — that wire token belongs to 7th-form
  agent-EXTERNAL Layer-4, not Tencent positioning. Re-attribution would
  blur two distinct features.
- **(c) Remove QW-4 from "shipped" lists.** Rejected — the docs
  *did* land, and Tencent positioning *is* a real deliverable. Just
  honest framing needed.

**Consequences.**

- Public framing of "4 QW items" is preserved with the honest qualifier
  that QW-4's deliverable is documentation.
- Issue #767 ship-readiness checklist treats QW-4 as resolved.
- Future readers of the inventory open question #9 (Tencent QW-4
  unmapped) are redirected here.

## ADR-2 — Cluster H net-new doc scope: 6 shipped, 12-20h deferred as accepted debt

**Context.** Synthesis Cluster H (DOC-16) called for MVP docs for six
v0.7.0 subsystems (Hook pipeline, Federation hardening, K8 quotas,
K10 SSE approvals, Sidechain transcripts, Signed-events V-4 chain).
The synthesis estimated 12-20 hours of net-new doc writing alongside
the broader Cluster H stale-doc sweep. The operator decision point
was: split into H-1 (stale-fix, ship-blocker) + H-2 (net-new MVP docs,
defer to v0.7.0.1 doc-only release) OR block tag-cut on the full
sweep.

**Decision.** **Ship the 6 net-new docs in v0.7.0** (Cluster H landed
[`docs/hook-pipeline.md`](../hook-pipeline.md),
[`docs/federation.md`](../federation.md),
[`docs/k8-quotas.md`](../k8-quotas.md),
[`docs/k10-sse-approvals.md`](../k10-sse-approvals.md),
[`docs/sidechain-transcripts.md`](../sidechain-transcripts.md), and
[`docs/signed-events-v4.md`](../signed-events-v4.md) as MVPs of
200-500 lines each per the synthesis spec, merged via PR #768). The
additional 12-20 hours of long-form expansion (operator tutorials,
production tuning runbooks, full troubleshooting decision trees) is
**accepted debt** scheduled for v0.7.x patch releases as the relevant
subsystems collect operator feedback in production.

**Alternatives considered.**

- **(a) Block tag-cut on full long-form docs.** Rejected — tag-cut
  delay of 1-2 weeks for content that is most useful AFTER real
  operator deployment is bad sequencing. MVPs unblock procurement
  review now; deep dives mature on real-world evidence.
- **(b) Defer all 6 net-new docs to v0.7.0.1.** Rejected — leaves
  the most-asked-about v0.7.0 subsystems (hooks, federation, signed
  events) without any dedicated entry point. MVPs are the
  ship-blocker; long-form is the climb-back.

**Consequences.**

- v0.7.0 ships with 6 MVP docs that anchor every subsystem the
  release-notes mention.
- `docs/internal/v070-accepted-debt.md` carries the 12-20h
  long-form-doc-expansion line item with explicit v0.7.x scope.

## ADR-3 — Skills CLI + HTTP + MCP parity: shipped at three-surface symmetry

**Context.** Synthesis API-2 HIGH flagged that the L1-5 Agent Skills
substrate exposed 7 MCP tools but had **no CLI subcommands and no HTTP
routes**, breaking the project's three-surface parity contract
(everything reachable from MCP is also reachable from CLI and HTTP).
Operator decision point: confirm whether CLI / HTTP parity is wanted,
or accept MCP-only as the design.

**Decision.** **Ship three-surface parity for all 7 Skills tools.**
Cluster E (PR #772) landed `ai-memory skill {register|list|get|export|
promote|compose|resource}` CLI subcommands AND
`POST /api/v1/skill/{register,list,get,export,promote,compose,resource}`
HTTP routes. The handlers already existed in `src/mcp/tools/skill_*.rs`;
Cluster E promoted them to `pub` and wired the CLI dispatcher + Axum
router. 7 + 7 = 14 net-new surfaces.

**Alternatives considered.**

- **(a) MCP-only.** Rejected — silently breaking the three-surface
  parity contract for one substrate would be a procurement-grade
  inconsistency.
- **(b) CLI but not HTTP.** Rejected — Skills are namespaced artefacts
  with TTL and attestation; remote operators (federation, ops tooling,
  CI/CD) need the HTTP surface as much as the local CLI.

**Consequences.**

- Skills are now reachable from all three surfaces, matching every
  other Family at v0.7.0.
- CHANGELOG and `docs/agent-skills.md` document the parity.

## ADR-4 — PERF-5 Synchronous-mode `max_retries` default

**Context.** Performance review PERF-5 HIGH observed that the
synchronous-atomise hot path inherits `curator_max_retries = 3`
from the deferred-mode default. On a Synchronous-mode store, three
malformed-JSON LLM retries inside the write transaction is up to
~30-60 seconds of held-mutex latency at the substrate's single
SQLite writer. Operator decision point: reduce the default for the
Synchronous path while leaving Deferred at 3.

**Decision.** **Reduce default `curator_max_retries` to 1 for the
Synchronous code path; leave Deferred at 3; expose a per-namespace
policy override `curator_max_retries: Option<u32>` so operators can
restore the legacy posture per namespace.** Cluster F is the
landing PR for the substrate change; this ADR pins the design so
F's implementer doesn't re-litigate.

**Alternatives considered.**

- **Keep 3 for both modes.** Rejected — empirical 1-2 retry-success
  bias in the curator's malformed-JSON failure mode does not justify
  the worst-case latency under Synchronous mode.
- **Reduce to 0 (no retry).** Rejected — even Synchronous mode
  benefits from a single retry against transient LLM jitter
  (network blip, tokeniser hiccup). 0 retries trades latency for
  flake.
- **Make the value configurable only, no default change.** Rejected —
  operators following the documented "opt into Synchronous for
  decompose-before-embed" path would inherit the surprising 30-60s
  P99 latency by default; the right default is the safe one.

**Consequences.**

- Operators who *want* the 3-retry posture on Synchronous mode must
  set `[namespaces.<ns>.standard.auto_atomise] curator_max_retries = 3`
  explicitly.
- Cluster F's regression test asserts the
  `(Synchronous, default) → 1` behaviour and pins the policy
  override path. (Cluster F has not landed at the time of writing;
  the ADR pre-commits the decision so F lands deterministically.)

## ADR-5 — Error-code naming convention: UPPER_SNAKE going forward, lowercase.dotted as legacy

**Context.** API review API-5 MEDIUM observed that v0.7.0 introduces
error tokens in two distinct styles:

- **UPPER_SNAKE** — `GOVERNANCE_REFUSED`, `MEMORY_NOT_FOUND`,
  `REFLECTION_DEPTH_EXCEEDED`, `MCP_MUTATION_DISABLED_ERROR`.
  Used in the new Form 1-6 + 7th-form work and in recursive-learning.
- **lowercase.dotted** — `sender_agent_id_mismatch`, `peer_id_header_missing`,
  `quota.daily_exceeded`, `dispatch.failed`. Used in earlier v0.7.0
  federation hardening and the K10 SSE work.

The mix is real (it appears on the wire) and forces SDK consumers to
match against two regex patterns. The operator decision is which
becomes the project standard.

**Decision.** **UPPER_SNAKE is the canonical error-code convention.**
All net-new error tokens MUST use UPPER_SNAKE. Existing
lowercase.dotted tokens are preserved verbatim for backwards compat
(no wire-rename) and documented as legacy in
[`docs/API_REFERENCE.md`](../API_REFERENCE.md) §"Error code conventions"
with a static alias table. A best-effort CI grep gate
(`scripts/lint-error-codes.sh`) flags new lowercase.dotted tokens in
PR diffs.

**Alternatives considered.**

- **lowercase.dotted as canonical.** Rejected — the structural
  errors that gate substrate writes (governance, reflection cap,
  agent-action policy) all already ship as UPPER_SNAKE in v0.7.0;
  switching them would be a wire-break.
- **Rename all legacy lowercase.dotted to UPPER_SNAKE.** Rejected —
  wire-break against v0.7.0-rc SDKs and federation peers.
- **No convention; accept the mix.** Rejected — long-term SDK
  ergonomics suffer; pinning the convention is cheap insurance.

**Consequences.**

- Cluster H added an "Error code conventions" section to
  `docs/API_REFERENCE.md` documenting the rule + the legacy alias
  table.
- v0.8.0 may consider a coordinated lowercase.dotted → UPPER_SNAKE
  rename with parallel alias period (out of scope for v0.7.0).

## ADR-6 — API-6 `/api/v1/memory_load_family` path alias

**Context.** API review API-6 MEDIUM observed that the v0.7.0
loaders surface uses `POST /api/v1/memory_load_family` and
`POST /api/v1/memory_smart_load`, while the rest of the
`/api/v1/` surface follows the
`/api/v1/<noun>/<verb>` shape (e.g., `/api/v1/skill/register`,
`/api/v1/quota/status`). The flat `memory_load_family` path is
the legacy MCP-tool-name straight-port and breaks the noun/verb
convention.

**Decision.** **Keep the current
`POST /api/v1/memory_load_family` and `POST /api/v1/memory_smart_load`
paths for backwards compatibility; add aliases
`POST /api/v1/family/load` and `POST /api/v1/family/smart_load` as
the preferred form going forward.** Both forms route to the same
handler. The current paths are documented as legacy in
[`docs/API_REFERENCE.md`](../API_REFERENCE.md); the alias forms
are listed first as the recommended shape. No `Deprecation` header
is set in v0.7.0 (additive only); a header may follow in v0.8.0
after an external-consumer audit.

**Alternatives considered.**

- **Hard rename without alias.** Rejected — breaks v0.7.0-rc HTTP
  consumers.
- **Keep only the legacy path.** Rejected — the inconsistency is
  load-bearing for SDK ergonomics; alias-now keeps the climb-back
  cheap.

**Consequences.**

- Two paths route to one handler for the two loader endpoints.
- Cluster H's API reference update documents both forms with
  preference for the new one.

## ADR-7 — LOE > 2-session clusters: actual execution fit within the envelope

**Context.** The synthesis flagged Cluster H (~3 sessions) and
Cluster F (potentially ~2 sessions if PERF-5 needs a deprecation
cycle) as the only clusters with LOE > 2 sessions. The operator
decision point asked whether to split these into sub-clusters and
defer.

**Decision.** **No split needed — actual execution of all shipped
clusters fit within ~1.5 sessions each.** Cluster H came closest to
the upper bound (~1.5 sessions) due to the 6 net-new MVP docs +
broad stale-fix sweep; even that landed inside one campaign-day.
No cluster H-2 or F-2 sub-split was required.

**Alternatives considered.**

- **Pre-emptively split H into H-1 stale-fix + H-2 net-new MVP docs.**
  Rejected at execution time — the synthesis estimate of 12-20 hours
  net-new was conservative; the 200-500 line MVP discipline kept the
  net-new corner of the cluster to ~4-6 hours.

**Consequences.**

- The synthesis LOE estimates were conservative by ~2× for the
  docs-heavy cluster. Future synthesis cycles should bias estimates
  down for cleanly-scoped docs work; the discipline of a per-doc
  word/line ceiling is the load-bearing constraint.
- No retroactive sub-cluster PRs needed; ship-readiness wave is
  one campaign per the original plan.

## Cross-cutting notes

- **All seven ADRs are accepted as of 2026-05-15** and are not
  expected to revisit in v0.7.0. ADR-4 has a single forward
  dependency (Cluster F landing the substrate change); the design
  is pinned here so F's implementer does not re-litigate.
- **None of these ADRs require a CHANGELOG or release-notes amendment
  beyond what Cluster H already shipped** (ADR-5 and ADR-6 each have
  documentation surfaces in `docs/API_REFERENCE.md`; ADR-1 has a
  one-line clarification in CHANGELOG + release-notes that is part of
  this Cluster K PR).
- **Provenance.** Each ADR cites the originating review finding by
  ID (API-5, PERF-5, etc.) so a future auditor can reconstruct the
  decision chain from the six review docs (kept in their respective
  reviewer worktrees; rolled-up references live in
  [`v070-review-synthesis.md`](v070-review-synthesis.md)).

— Cold mountain.
