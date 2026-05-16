# v0.7.0 Accepted Debt Register

> Companion to [`v070-ship-readiness-adrs.md`](v070-ship-readiness-adrs.md)
> and the per-cluster fix PRs. This document triages every Cluster L /
> INFO finding from the 6-reviewer synthesis
> ([`v070-review-synthesis.md`](v070-review-synthesis.md)) into one of
> three buckets:
>
> 1. **Already-fixed-by-sibling-cluster** — the finding was absorbed
>    into another cluster's fix and is closed at the v0.7.0 ship.
> 2. **Deferred-to-v0.7.x** — a real follow-up; a GitHub issue carries
>    the deferred work with `enhancement` label + v0.7-polish milestone.
> 3. **Accepted-debt-permanent** — operator-defensible documented
>    limitation, scoped out of v0.7.0 by intent. Rationale below.
>
> Date: 2026-05-15. Author: AI NHI dev team session (Cluster K).
> Tracking issue: [#767](https://github.com/alphaonedev/ai-memory-mcp/issues/767).

## Triage outcome (rolled up)

| Bucket | Count |
|---|---|
| Already-fixed-by-sibling-cluster | 7 (6 review findings + 1 cluster-level resolution) |
| Deferred-to-v0.7-polish (issue filed) | 10 findings rolled into 6 issues ([#779](https://github.com/alphaonedev/ai-memory-mcp/issues/779), [#780](https://github.com/alphaonedev/ai-memory-mcp/issues/780), [#781](https://github.com/alphaonedev/ai-memory-mcp/issues/781), [#782](https://github.com/alphaonedev/ai-memory-mcp/issues/782), [#783](https://github.com/alphaonedev/ai-memory-mcp/issues/783), [#784](https://github.com/alphaonedev/ai-memory-mcp/issues/784)) |
| Accepted-debt-permanent | 8 |
| **Total triaged** | **25 findings → 7 sibling + 6 issues + 8 permanent + 4 API net-zero confirmations (no-action)** |

## Already-fixed-by-sibling-cluster

| Finding | Title | Resolved by | Notes |
|---|---|---|---|
| API-5 | Error-code style mix (UPPER_SNAKE vs lowercase.dotted) | Cluster K — ADR-5 | UPPER_SNAKE pinned as canonical going forward; legacy lowercase.dotted preserved verbatim. Doc surface in `docs/API_REFERENCE.md` §"Error code conventions". |
| API-6 | `/api/v1/memory_load_family` path inconsistency | Cluster K — ADR-6 | Alias `POST /api/v1/family/load` + `POST /api/v1/family/smart_load` added; legacy paths preserved. |
| API-20 | 8 unaccounted-for tools in release-notes | Cluster H — PR #768 | Tool-count drift fix authoritatively reconciled MCP tool count to 71 against `Profile::full().expected_tool_count()`. |
| DOC-16 | 6 missing dedicated docs | Cluster H — PR #768 | Six MVP docs (200-500 lines each): `docs/hook-pipeline.md`, `docs/federation.md`, `docs/k8-quotas.md`, `docs/k10-sse-approvals.md`, `docs/sidechain-transcripts.md`, `docs/signed-events-v4.md`. Long-form expansion is separate accepted debt (this register) for v0.7.x. |
| PERF-13 | `deferred_audit` unbounded channel | Cluster C — PR #770 | Cluster C signed-events chain integrity work landed the bounded channel + DLQ path; `deferred_audit::spawn` now honors a configurable capacity with counter-exposed lag metrics. |
| COR-11 | `last_err` unreachable placeholder | Cluster A — PR #771 | Cosmetic; fixed alongside the Form 4 fact-provenance correctness sweep. |
| COR-12 | env-var test races | Cluster G — PR #774 | `serial_test` crate now gates the env-var-touching tests in `confidence/shadow.rs` test fleet; same discipline propagated through Cluster G's calibration test additions. |

## Deferred-to-v0.7.x (issue filed)

Each row below has a corresponding GitHub issue with label `enhancement`
+ milestone `v0.7-polish`. Issue numbers populated by Cluster K after
filing.

| Finding | Title | Severity | Rationale for defer | Issue |
|---|---|---|---|---|
| PERF-16 | `format!` in Form 1 candidate loop (5-iter bound) | LOW | Bounded iteration (5 candidates per synthesis); replacing with `String::new` + `push_str` saves microseconds against an LLM round-trip already in the millisecond-to-second envelope. Real but invisible. | [#779](https://github.com/alphaonedev/ai-memory-mcp/issues/779) |
| COR-11 / SEC-15 | auto-export detached-thread silent failure | LOW | Hook is best-effort by design. Counter add (`auto_export.spawn_failed_total`) is the right defense-in-depth; the surface is the v0.7-polish metric expansion. | [#780](https://github.com/alphaonedev/ai-memory-mcp/issues/780) |
| PERF-8 | `auto_persona` `LIKE %X%` scan | MEDIUM | Requires a schema column extension + backfill (canonical entity-id-as-column). Migration risk is real; defer the schema change to v0.7-polish. Subsumes PERF-17. | [#781](https://github.com/alphaonedev/ai-memory-mcp/issues/781) |
| PERF-11 | Form 3 content duplication across stages | MEDIUM | Cluster B addressed the synthesis-side prompt truncation (PERF-7); Form 3 multi-step ingest carries the same pattern. Refactor lives alongside the Form 3 maturation in v0.7-polish. | [#782](https://github.com/alphaonedev/ai-memory-mcp/issues/782) |
| PERF-17 | `auto_persona resolve_entity_id` JSON parse | LOW | Subordinate to PERF-8 (the entity-id column extension also subsumes this hot path). Tracked under the same issue. | [#781](https://github.com/alphaonedev/ai-memory-mcp/issues/781) |
| COV-15 / COV-16 / COV-17 / COV-18 | opportunistic coverage matrix expansion | LOW | Cluster B/G/E/I baseline acceptance suites pin the v0.7.0 contract; the deeper coverage matrix is the v0.7-polish reviewer-time lane. Rolled up as one issue. | [#783](https://github.com/alphaonedev/ai-memory-mcp/issues/783) |
| Cluster H long-form doc expansion | 12-20h tutorial / tuning / troubleshooting depth | INFO | ADR-2 ships MVPs at 200-500 lines each; the long-form depth matures on real operator deployment feedback. v0.7-polish patch releases. | [#784](https://github.com/alphaonedev/ai-memory-mcp/issues/784) |

## Accepted-debt-permanent

These are real findings scoped out of v0.7.0 by intent. Each is
documented in the appropriate operator-facing surface (security posture
page, design RFC, or subsystem doc) rather than being filed as a
follow-up issue.

| Finding | Title | Severity | Why accepted-permanent |
|---|---|---|---|
| SEC-6 | Single-key HTTP auth | MEDIUM | Documented design. Multi-key + key-rotation is the federation hardening climb-back; single-key is the v0.7.0 posture per the threat model in `docs/federation.md` §"Auth model". |
| SEC-9 | SSRF `validate_url_dns` fails open on DNS hiccup | MEDIUM | Documented design. DNS-resolution dispatch-time re-check is opt-in (`validate_at_dispatch_too`) to preserve operator throughput envelopes that depend on a fast first-resolve. The fail-open posture is documented in `docs/federation.md` §"SSRF defense layers". |
| SEC-14 | `validate_namespace` ".." check is segment-level only | LOW | Defense-in-depth; the filesystem-write sanitisers in `src/utils/path.rs` already reject `..` at the path level. Segment-level check would block legitimate `parent..child` style namespace names operators rely on. |
| SEC-18 | env-prefix redactor case sensitivity | LOW | The redactor pattern is operator-extendable via the `redaction_patterns` config; the case-sensitivity is documented at the call site in `src/utils/redaction.rs`. v0.7.x will add `pass`/`pwd` to the default keyword list as a non-breaking expansion. |
| SEC-20 | K10 SSE 1024 channel capacity, no per-agent rate-limit | INFO | v0.8.0 K10 hardening sprint. Bounded channel preserves liveness at v0.7.0; per-agent rate-limit is in the v0.8.0 scope per `docs/k10-sse-approvals.md` §"Future work". |
| SEC-21 | `publish-sdks.yml` tag-trigger | INFO | Procurement-grade acceptable. Tag-trigger is gated on the operator-manual `release.yml` dispatch per the release procedure documented in `docs/v0.7.0/release-notes.md` §"Release procedure". |
| SEC-22 | Migration 0025 backfill | INFO | Idempotent and verified safe by the migration ladder dry-run discipline. No action. |
| SEC-23 | Form 5 shadow honors contract | INFO | Positive finding — no action. Listed here to close the audit trail. |

## API "net-zero confirmations" (no action needed)

API-9, API-13, API-14, API-15, API-16, API-17, API-18, API-19 were
classified by the API review as "net-zero confirmations" — the
substrate already behaves as the API spec describes; no change needed,
no follow-up to file. These are not counted in the triage rollup above
because they require no work.

## Audit-trail conventions

- Every row above is provenance-linked to either a review-doc finding
  ID or a sibling-cluster PR. A future auditor can reconstruct the
  decision chain from `docs/internal/v070-review-{security,correctness,
  performance,api-ux,docs,coverage}.md` + the merged PR descriptions.
- The v0.7-polish milestone is the operator's queue for the deferred
  items; closure cadence is operator-controlled.
- The accepted-debt-permanent rows do NOT become issues — they are
  closed by virtue of being documented in their owning surface. A
  v0.8.0 reviewer should re-evaluate them against the new threat
  model rather than treating them as open.

— Cold mountain.
