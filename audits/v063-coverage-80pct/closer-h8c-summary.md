# Closer H8c — handlers.rs agents/pending/consolidate (W8)

Branch: `cov-90pct-w8/handlers-agents`
Base: `origin/cov-90pct-w7/integration-tests` (`eafaf84`)

## Command (verbatim)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --no-fail-fast --json -- --test-threads=2 > /tmp/closer-h8c-cov.json 2>&1
```

## Headline numbers

| Surface         | Pre-W8 (W7)  | Post-H8c | Delta    |
|-----------------|--------------|----------|----------|
| Codebase line   | 85.85%       | 86.53%   | +0.68 pp |
| Codebase fn     | 84.49%       | 84.93%   | +0.44 pp |
| Codebase region | 86.18%       | ~86.6%   | ~+0.4 pp |
| `handlers.rs` line | 81.09%    | 84.26%   | +3.17 pp |
| `handlers.rs` fn   | n/a       | 93.13%   | n/a      |

**Lines covered (codebase):** 29,016 / 33,534.
**Lines covered (`handlers.rs`):** 7,429 / 8,817.

H8c is one of four parallel W8 lanes carving handler-level coverage gaps;
H8a (archive) / H8b (inbox/subs) / H8d (qs+fanout) ship in parallel and
will compose with this lane's delta at the final W8 merge.

## Tests added

Per-handler distribution (32 new tests, all driving the live Axum
router via `tower::ServiceExt::oneshot`):

| Handler                  | Count | Notes                                                      |
|--------------------------|-------|------------------------------------------------------------|
| `list_agents`            | 3     | empty / two-rows / types+capabilities echoed              |
| `register_agent`         | 5     | happy 201, missing field 4xx, invalid id 400, idempotent re-register, capabilities round-trip |
| `list_pending`           | 4     | with-actions, status=pending filter, status=rejected filter, oversized limit clamp |
| `approve_pending`        | 5     | happy execute Store, invalid id 400, already approved → 403, executor-records-decided_by, response carries memory_id of executed write |
| `reject_pending`         | 3     | happy mark+no-execute, already rejected → 404, invalid id 400 |
| `consolidate_memories`   | 6     | 2-into-1 happy, single id 400, invalid namespace, invalid agent_id, max-id-count cap (101), missing source 500 |
| `detect_contradictions`  | 4     | empty, synth-link for shared title, namespace filter isolates, invalid namespace 400 |
| `get_capabilities`       | 2     | expected shape, version matches CARGO_PKG_VERSION         |

Plus the W7 baseline: lib total **916 → 948** passing tests.

## Surprises / deviations

- The W7 W7 W8 spec mentions `consolidate_memories` "governance-pending
  defers" as one bullet; in this codebase the consolidate handler does
  NOT route through governance (no enforce_governance call in
  `handlers::consolidate_memories`), so I substituted a **missing-source
  500** test which exercises the post-validation error arm of the
  handler and gives equivalent line/region coverage to the spec'd test.
  All six consolidate buckets are still covered.
- `validate_id` only rejects control characters (not spaces / `!` / etc.)
  — `bad%01id` (SOH) is the canonical "invalid id" path, and that's
  what the new approve/reject tests use.
- `approve_pending` "already approved" returns **403** (not 409 as the
  spec sketches): `ApproveOutcome::Rejected("already decided …")` flows
  through the FORBIDDEN arm. The test matches the actual contract.
- The Store-payload pending tests had to include the full Memory shape
  (`id`, `created_at`, `updated_at`, `access_count`) because
  `db::execute_pending_action` does `serde_json::from_value::<Memory>`
  which has no `serde(default)` on those fields.

## Quality gates

- `cargo fmt --check`: ✓
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic`: ✓
- `cargo test --lib -- --test-threads=2`: ✓ (948 passed, 0 failed)

## Commit

- `49a2e1f` test(handlers): W8/H8c — agents/pending/consolidate gap-closing
