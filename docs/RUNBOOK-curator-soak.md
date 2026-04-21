# Runbook — Week-long curator soak against a production corpus

Status: **runbook (executable pending wallclock)**.
Date: 2026-04-19
Depends on: #278, #281 (curator + full autonomy loop merged). #265
sync-hooks.

This runbook is the concrete, step-by-step procedure for the
"Run the curator against a real corpus for a week and publish the
audit trail" caveat from the post-v0.6.0 trident review.

It turns the "$5 — week-long curator run" caveat from a subjective
claim into an executable script with a published audit trail.

## What the soak proves (if run to completion)

> **"Over one week against a N-memory corpus, the curator made M
> autonomous actions (consolidations + forgets + priority
> adjustments), with R% reversed on operator review. No data loss.
> No runaway cost."**

That's the defensible claim. It replaces the overclaim "100%
autonomous" with a measured activity + reversal + cost profile.

## Prerequisites

1. Production corpus snapshot — a representative DB with at least
   10 000 memories across 5+ namespaces.
2. One soak host (VM or dedicated machine):
   - 4 vCPU, 16 GB RAM minimum
   - Ollama running with an embedding-capable model (default: Gemma
     4 E2B for `feature_tier = smart`)
   - Outbound network to pull pgvector/pgvector:pg16 and the
     ai-memory release binary
3. A snapshot pinned to the soak commit SHA so the audit trail is
   reproducible.

## Deployment

```sh
# 1. Restore the corpus snapshot.
ai-memory restore --from ./corpus-snapshot.db --skip-verify  # verify yourself first

# 2. Start the HTTP daemon (optional but useful for observability).
ai-memory serve --host 127.0.0.1 --port 9077 --tls-cert … &

# 3. Start the curator in daemon mode.
AI_MEMORY_AUTONOMOUS_HOOKS=1 \
ai-memory curator --daemon \
    --interval-secs 3600 \
    --max-ops 100 \
    2>&1 | tee -a curator.log &

# 4. Capture a baseline report.
ai-memory stats --json > baseline.json
ai-memory list --namespace _curator/rollback --limit 1 --json > baseline-rollback.json
```

## The soak

Let the curator run for **168 hours (7 × 24)**. One cycle per hour
× 7 days = 168 cycles. Each cycle writes a self-report memory in
`_curator/reports/<ts>`; accumulate ≥168 of these over the window.

During the soak:

- Do NOT restart the curator unless it panics. If it does, capture
  the stack trace and the stderr log, restart, and record the gap.
- Do NOT add or remove memories outside the curator — we're
  measuring autonomous behaviour against a fixed corpus.
- Monitor Prometheus metrics. Alert if:
  - `ai_memory_curator_cycles_total` stops incrementing.
  - `ai_memory_curator_operations_total{result="err"}` exceeds 5% of
    total operations.

## Post-soak audit trail

At T+168 h, produce the audit trail:

```sh
# Every curator action is in _curator/rollback; every cycle report
# is in _curator/reports.
ai-memory list --namespace _curator/rollback --limit 10000 --json \
    > audit-actions.json
ai-memory list --namespace _curator/reports --limit 10000 --json \
    > audit-cycles.json

# Aggregate cycle reports for the headline numbers.
# Field shapes map to src/curator.rs::CuratorReport + src/autonomy.rs::AutonomyPassReport:
#   - Top-level: auto_tagged, contradictions_found, operations_attempted,
#     operations_skipped_cap, errors (Vec<String>), autonomy (nested).
#   - Nested under .autonomy: clusters_formed, memories_consolidated,
#     memories_forgotten, priority_adjustments, rollback_entries_written,
#     errors (Vec<String>).
#   - There is NO `errors_total` scalar; errors are always arrays —
#     aggregate with `(.errors | length)`.
jq '[.memories[].content | fromjson] as $reports | {
    cycles: ($reports | length),
    total_auto_tagged:        ([$reports[].auto_tagged // 0] | add),
    total_contradictions:     ([$reports[].contradictions_found // 0] | add),
    total_ops_attempted:      ([$reports[].operations_attempted // 0] | add),
    total_ops_skipped_cap:    ([$reports[].operations_skipped_cap // 0] | add),
    total_consolidated:       ([$reports[].autonomy.memories_consolidated // 0] | add),
    total_forgotten:          ([$reports[].autonomy.memories_forgotten // 0] | add),
    total_priority_adjusts:   ([$reports[].autonomy.priority_adjustments // 0] | add),
    total_rollback_entries:   ([$reports[].autonomy.rollback_entries_written // 0] | add),
    total_curator_errors:     ([$reports[].errors // [] | length] | add),
    total_autonomy_errors:    ([$reports[].autonomy.errors // [] | length] | add)
}' audit-cycles.json > headline.json
```

## Operator review

The operator samples `audit-actions.json` and marks each action as:

- **correct** (keep)
- **incorrect** (reverse via `ai-memory curator --rollback <id>`)

Aim to sample at least 100 actions, stratified across consolidate /
forget / priority-adjust. Record the reversal rate R:

```
R = (reversed actions / sampled actions) * 100
```

## Pass / fail criteria

**Pass criterion** (what we commit to publishing on v0.7.0 GA):

- `cycles >= 160` (allow 8 missed-hour margin for panics, restarts,
  Ollama hiccups).
- `(total_curator_errors + total_autonomy_errors) <= 0.05 *
  total_ops_attempted` — aggregate error rate ≤ 5% of attempted
  operations. Computed directly from `headline.json`.
- `R <= 10%` — operator agrees with at least 90% of curator
  decisions on the stratified sample.
- Zero unreversible corruption: every `_reversed`-tagged entry still
  has a matching recoverable snapshot in its content.

**Soft-fail — document but don't block release**:

- `R in (10%, 20%]` → publish with caveat, tune the curator
  thresholds (Jaccard threshold, priority-adjust triggers), re-run.

**Hard-fail — block release**:

- `R > 20%` → curator decisions are unreliable; do not advertise
  "100% autonomous".
- Any unrecoverable memory loss (rollback snapshot lacks sufficient
  info to restore).

## Publication

On pass, the soak report lands as `docs/CURATOR-SOAK-v0.7.0.md`
with:

- Date, commit SHA, corpus stats (N memories, N namespaces, total
  content bytes).
- `headline.json` + the stratified sample with operator marks
  attached (redacted for any private content).
- Reversal rate R with the 95% CI.
- Explicit methodology note: "autonomous curator decisions
  reviewed by a human operator; R = reversal rate, not a loss
  probability".

## Why this is a runbook, not a test

- Runtime is 168 hours; inappropriate for per-PR CI.
- Requires an LLM (Ollama) and real model weights.
- Requires a production-shaped corpus, not synthetic data.
- Results are meaningful only on the release candidate commit.

The in-tree unit + integration tests in #281 prove the curator's
mechanics (correctness on 5-memory synthetic corpora, rollback
roundtrips). The soak proves the curator's **judgement** at scale —
that's what the "100% autonomous" claim actually requires.
