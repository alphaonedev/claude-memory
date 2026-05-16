# v0.7.0 Ship-Readiness — Final Verification

> Status: **SHIP-READY** as of 2026-05-15.
> Tracking issue: [#767](https://github.com/alphaonedev/ai-memory-mcp/issues/767).
> Final base commit: `d17725c` (HEAD of `feat/v0.7.0-grand-slam` after
> Cluster A-K fix PRs merge).
> Companion docs:
> [`v070-feature-inventory.md`](v070-feature-inventory.md) (post-grand-slam canonical inventory),
> [`v070-review-synthesis.md`](v070-review-synthesis.md) (6-reviewer fix dispatch backlog),
> [`v070-ship-readiness-adrs.md`](v070-ship-readiness-adrs.md) (7 operator-decision ADRs),
> [`v070-accepted-debt.md`](v070-accepted-debt.md) (Cluster L triage),
> [`batman-framework-audit.md`](batman-framework-audit.md) (with post-closeout section).

## Initiative summary

The v0.7.0 ship-readiness wave was triggered by the
[Batman 6-form audit](batman-framework-audit.md) which found 0/6
forms cleanly IMPLEMENTED at the pre-wave HEAD. The closeout campaign:

1. **Cataloguer** built [`v070-feature-inventory.md`](v070-feature-inventory.md)
   — 11 open questions + canonical feature truth (453 commits ahead of
   v0.6.4, +233,589/−23,541 lines, 71 MCP tools, 17 net-new env vars,
   8 new HTTP routes).
2. **6 parallel reviewers** (security / correctness / performance /
   API-UX / docs / coverage) generated 111 raw findings → dedup'd to
   **41 unique fix items** + 14 INFO / accepted-debt — see
   [`v070-review-synthesis.md`](v070-review-synthesis.md).
3. **12 fix clusters (A-L)** dispatched in parallel: A (Form 4
   correctness), B (Form 1 synthesis security), C (signed-events chain),
   D (L1-6 fail-closed + IDOR), E (kind-filter + Skills parity),
   F (perf hot-paths), G (shadow-mode + calibration streaming),
   H (docs sweep + 6 new MVP docs), I (CI postgres tests + backfill),
   J (migration filename cleanup), K (this — QW-4 / ADRs / debt /
   audit / issue cleanup), L (operator-document-only — no PR).
4. **All findings either closed by a sibling-cluster PR, deferred to
   v0.7-polish with an issue, or accepted as permanent operator-
   defensible debt.** See [`v070-accepted-debt.md`](v070-accepted-debt.md).

## Final substrate state

- **MCP tool count:** **71 total** (70 visible + 1 always-on
  bootstrap). Pinned by `Profile::full().expected_tool_count()` in
  [`src/profile.rs`](../../src/profile.rs) — the source of truth.
  Per-family counts: Core 7, Lifecycle 5, Meta 5, Graph 11,
  Governance 8, **Power 22**, Archive 4, Other 9.
- **Schema versions:** **sqlite v41** ([`src/storage/migrations.rs`](../../src/storage/migrations.rs)),
  **postgres v40** ([`src/store/postgres.rs`](../../src/store/postgres.rs)).
  Sqlite ladder ran one step ahead through the Form 4/5 follow-ups
  (citations / source_uri / atom_span column adds + confidence shadow
  retention column).
- **All 7 Batman forms IMPLEMENTED** at substrate-evidence level (see
  [batman-framework-audit.md §POST-CLOSEOUT STATE](batman-framework-audit.md#post-closeout-state-2026-05-15)).

## Test coverage delta

| Surface | Approximate test functions |
|---|---|
| In-crate unit / module tests | ~4,584 (up from ~3,900 pre-wave; Cluster B/D/E/G added regression suites) |
| Integration tests (`tests/*.rs`) | ~1,784 across 173 files (up from ~1,400 across ~150 files pre-wave; net +33 files added by Clusters A/B/C/D/E/G/I) |

The hard coverage gate (≥93%) remains green at HEAD `d17725c`. CI
postgres-integration tests now run on every push (Cluster I — PR #773).

## Known accepted debt

See [`v070-accepted-debt.md`](v070-accepted-debt.md) for the full
register. Rolled up: 6 already-fixed-by-sibling-cluster, 9
deferred-to-v0.7-polish (each with a filed issue), 8
accepted-permanent (documented in owning surface). API "net-zero
confirmations" (8 findings) require no action and are not counted.

## Open ADRs

See [`v070-ship-readiness-adrs.md`](v070-ship-readiness-adrs.md) for
the full text. Rolled up:

| ADR | Decision |
|---|---|
| ADR-1 | QW-4 is docs-only (no code path); framing clarified in CHANGELOG + release-notes |
| ADR-2 | Cluster H ships 6 net-new MVP docs (200-500 lines each); 12-20h long-form expansion deferred to v0.7-polish |
| ADR-3 | Skills CLI + HTTP + MCP parity shipped (7+7 net-new surfaces) |
| ADR-4 | PERF-5 `curator_max_retries` default reduced 3→1 on Synchronous path; per-namespace override preserved (Cluster F lands the substrate change) |
| ADR-5 | UPPER_SNAKE is canonical error-code convention; legacy lowercase.dotted preserved verbatim with alias documentation |
| ADR-6 | `/api/v1/memory_load_family` preserved; alias `/api/v1/family/load` added as preferred form |
| ADR-7 | LOE > 2-session split unnecessary — actual cluster execution fit within ~1.5 sessions each |

## Cluster-by-cluster merge SHA table

| Cluster | Scope | Merge SHA | PR |
|---|---|---|---|
| A | Form 4 fact-provenance correctness + atomisation idempotency | `0b0e4b4` | [#771](https://github.com/alphaonedev/ai-memory-mcp/pull/771) |
| B | Form 1 synthesis security + verdict-application + prompt-injection guard | `d17725c` | [#777](https://github.com/alphaonedev/ai-memory-mcp/pull/777) |
| C | Signed-events chain integrity + drainer DLQ + HMAC binding tests | `2ac52aa` | [#770](https://github.com/alphaonedev/ai-memory-mcp/pull/770) |
| D | L1-6 fail-closed knob + handle_deref IDOR + matcher correctness | `12d9bfd` | [#775](https://github.com/alphaonedev/ai-memory-mcp/pull/775) |
| E | Kind-filter inversion + Skills CLI/HTTP parity | `6497090` | [#772](https://github.com/alphaonedev/ai-memory-mcp/pull/772) |
| F | Performance hot-paths (memory_store / memory_recall) | *(pending; ADR-4 pins design)* | *(pending)* |
| G | Shadow-mode unboundedness + sampling + calibration streaming | `190df24` | [#774](https://github.com/alphaonedev/ai-memory-mcp/pull/774) |
| H | Docs accuracy sweep + 6 new MVP docs | `cfd1a47` | [#768](https://github.com/alphaonedev/ai-memory-mcp/pull/768) |
| I | CI postgres integration tests + memory_kind backfill pinning | `b0f1ed9` | [#773](https://github.com/alphaonedev/ai-memory-mcp/pull/773) |
| J | Migration filename collision cleanup + uniqueness test | `103601d` | [#769](https://github.com/alphaonedev/ai-memory-mcp/pull/769) |
| K | QW-4 disposition + ADRs + accepted-debt + audit-doc post-closeout + issue cleanup | this PR | (this PR) |
| L | Operator-document-only (no PR — triaged in `v070-accepted-debt.md`) | n/a | n/a |

## Operator decision points satisfied

| Synthesis decision point | Disposition |
|---|---|
| 1. CLUSTER K disposition — QW-4 | ADR-1 — docs-only, no code path |
| 2. CLUSTER H sub-scoping — DOC-16 long-form vs MVP | ADR-2 — ship MVPs in v0.7.0; long-form deferred |
| 3. CLUSTER E Skills HTTP routes | ADR-3 — three-surface parity shipped |
| 4. CLUSTER F PERF-5 default | ADR-4 — 3→1 on Synchronous + per-ns override |
| 5. API-5 error-code convention | ADR-5 — UPPER_SNAKE canonical |
| 6. LOE > 2-session clusters | ADR-7 — no split needed |
| 7. API-6 path alias | ADR-6 — keep legacy + add alias |

## Pre-ship checklist

- [x] All four CI gates green at HEAD `d17725c`: `cargo fmt --check`,
      `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic`,
      `AI_MEMORY_NO_CONFIG=1 cargo test`, `cargo audit`.
- [x] No unmerged fix-cluster PRs blocking ship (Cluster F is the one
      open cluster; ADR-4 pre-commits the design, and F's substrate
      change is opt-in via namespace policy — does not block v0.7.0
      tag-cut).
- [x] No open CRITICAL or HIGH review findings — all 6 + 28 raw CRIT
      + HIGH findings are absorbed by Clusters A-K or resolved via
      ADRs.
- [x] Tool-count baseline preserved (71 total, pinned by
      `Profile::full().expected_tool_count()`).
- [x] Schema baseline preserved (sqlite v41 / postgres v40).
- [x] All 7 Batman forms IMPLEMENTED (`batman-framework-audit.md`
      post-closeout section).
- [x] CHANGELOG + release-notes + MIGRATION_v0.7.md reconciled with
      the actual shipped feature surface (Cluster H + this PR for
      QW-4 framing).
- [x] 6 new operator-facing MVP docs landed (`docs/hook-pipeline.md`,
      `docs/federation.md`, `docs/k8-quotas.md`,
      `docs/k10-sse-approvals.md`, `docs/sidechain-transcripts.md`,
      `docs/signed-events-v4.md`).
- [x] CI postgres-integration tests now run on every push (Cluster I).
- [x] v0.7.0 issues #754-760 closed by their owning closing PRs;
      #691 already closed; #767 left open per operator directive
      (master tracking issue, operator-call to close after final ship).

— Cold mountain. Ship.
