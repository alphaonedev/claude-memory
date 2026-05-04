# Closer H8d — W8 handlers.rs qs+fanout lane

Branch: `cov-90pct-w8/handlers-qs-fanout`
Base: `origin/cov-90pct-w7/integration-tests` (`eafaf84`)

## Command (verbatim)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --no-fail-fast --json -- --test-threads=2 > /tmp/closer-h8d-cov.json 2>&1
```

## Headline numbers

| Surface          | Pre-W8 (W7 base) | Post-H8d | Delta     |
|------------------|------------------|----------|-----------|
| Codebase line    | 85.85%           | 86.82%   | +0.97 pp  |
| Codebase region  | 86.18%           | 87.01%   | +0.83 pp  |
| Codebase fn      | 84.49%           | 85.53%   | +1.04 pp  |
| handlers.rs line | 81.09%           | 85.04%   | +3.95 pp  |
| handlers.rs reg. | n/a              | 88.52%   | n/a       |
| handlers.rs fn   | n/a              | 95.39%   | n/a       |

**Lines covered (codebase):** 28,907 / 33,295.
**Lines covered (handlers.rs):** 7,295 / 8,578.

The handlers.rs surface picked up ~250 newly-covered lines from the
27 tests in this batch — most landing on the `*_qs` arms and the 503
quorum-not-met response builders inside `set_namespace_standard_inner`,
`clear_namespace_standard_inner`, and `fanout_or_503`.

## Tests added (27)

All appended to `src/handlers.rs::tests`. No production code touched.

### QS-form namespace handlers (12)

`get_namespace_standard_qs` — 5
1. `http_get_namespace_standard_qs_returns_standard_for_existing_ns`
2. `http_get_namespace_standard_qs_returns_null_for_missing_ns_record`
3. `http_get_namespace_standard_qs_falls_through_to_list_on_missing_param`
4. `http_get_namespace_standard_qs_inherit_flag_returns_chain`
5. `http_get_namespace_standard_qs_invalid_namespace_returns_400`

`set_namespace_standard_qs` — 4
6. `http_set_namespace_standard_qs_happy_path_creates_placeholder`
7. `http_set_namespace_standard_qs_missing_namespace_returns_400`
8. `http_set_namespace_standard_qs_invalid_governance_returns_400`
9. `http_set_namespace_standard_qs_nested_standard_payload_works`

`clear_namespace_standard_qs` — 3
10. `http_clear_namespace_standard_qs_happy_path_after_set`
11. `http_clear_namespace_standard_qs_idempotent_on_unset`
12. `http_clear_namespace_standard_qs_missing_namespace_returns_400`

### `fanout_or_503` / quorum_not_met matrix (15)

Mock-peer driven, modeled on the W3 `federation::tests::mock_peer`
pattern. Adds 503/400/Hang behaviours used by the matrix.

13. `http_set_qs_fanout_503_when_all_peers_down`
14. `http_set_qs_fanout_503_payload_shape_includes_quorum_fields`
    (asserts `error="quorum_not_met"`, `got`, `needed`, `reason`)
15. `http_set_qs_fanout_503_includes_retry_after_header`
16. `http_set_qs_fanout_quorum_met_with_one_peer_down` (W=2, N=3)
17. `http_set_qs_fanout_quorum_not_met_strict_n_equals_w` (W=N=2)
18. `http_set_qs_fanout_quorum_w_equals_one_any_success_writes_succeed`
19. `http_set_qs_fanout_503_when_peer_hangs_past_deadline`
20. `http_set_qs_fanout_503_when_peer_returns_503` (peer-side 503)
21. `http_set_qs_fanout_503_when_peer_returns_4xx` (peer-side 400)
22. `http_set_qs_fanout_503_partition_minority_fails` (N=4, W=3,
    minority up)
23. `http_set_qs_fanout_majority_tolerates_minority_partition`
    (N=4, W=3, majority up)
24. `http_clear_qs_fanout_503_when_peer_down` (clear-side 503 +
    Retry-After via `broadcast_namespace_meta_clear_quorum`)
25. `http_set_qs_fanout_no_federation_returns_201_without_peers`
    (no-fed short-circuit branch)
26. `http_set_qs_fanout_peer_called_at_least_once_on_quorum_failure`
27. `http_set_qs_fanout_peer_receives_post_on_happy_path`

## Per-handler distribution

```
get_qs            = 5
set_qs            = 4
clear_qs          = 3
fanout_or_503     = 15  (set-side: 13, clear-side: 1, no-fed: 1)
TOTAL             = 27
```

## Quality gates

- `cargo fmt --check` — clean.
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` — clean.
- `cargo test --lib -- --test-threads=2` — **943/943 pass** (916 → 943; +27).

## Tests gated `#[ignore]`

None.

## Surprises / deviations

The brief mentioned a "request-id collision" idempotency assertion and
"request-id format" tests (missing → generated, malformed → 400). The
fanout path in this codebase does **not** carry a per-request id at the
HTTP boundary — `broadcast_store_quorum` passes a `Memory.id` through
`sync_push` payloads, but there is no `X-Request-ID` header or
similar request-scoped idempotency token at the federation layer.
Adding such a contract would require production-code changes, which
is out-of-scope for this test-only lane. Per the brief's pivot
clause ("If `fanout_or_503` is hard to test... reduce scope; pivot
the freed budget to MORE qs-form handler tests"), I redirected the
~4 budgeted request-id tests into:

- Two extra QS-form coverage paths (`?inherit=true` GET, nested
  `standard.namespace` POST) that hit otherwise-uncovered branches
  in `flatten_standard_body` and `handle_namespace_get_standard`.
- Two no-fed / fanout-attempted assertions that pin the
  short-circuit (no-fed → 201) and the leader-side POST attempt.

The other deviation worth surfacing: `set_namespace_standard_inner`
runs **two** quorum fanouts — first `fanout_or_503` for the standard
memory, then `broadcast_namespace_meta_quorum` for the namespace_meta
row. The set-side 503 tests collapse both into a single
`SERVICE_UNAVAILABLE` assertion because the FIRST failing fanout
short-circuits the response. This is a faithful reflection of the
production code — there is no observable distinction at the HTTP
boundary between which of the two fanouts triggered the 503. The
clear-side `http_clear_qs_fanout_503_when_peer_down` test uniquely
exercises `broadcast_namespace_meta_clear_quorum`'s 503 path, since
the clear handler has only one fanout call.

## Commits

```
46e743f test(handlers): W8/H8d — qs-form namespace handlers + fanout_or_503 matrix
```
