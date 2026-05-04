# Closer W12-C — Wave 12 Coverage Summary (subscriptions.rs deep)

**Branch:** `cov-90pct-w12/subscriptions-deep`
**Date:** 2026-04-26
**Owner:** Closer W12-C
**Files:** `src/subscriptions.rs` (test-only appends inside `mod tests`)

## Coverage delta

| Metric                       | Pre (W11/L10b) | Post (W12-C) | Δ      | Target |
|------------------------------|---------------:|-------------:|-------:|-------:|
| subscriptions.rs lines       | 75.00%         | **97.61%**   | +22.61 |  90%+  |
| subscriptions.rs regions     | 72.90%         | **97.11%**   | +24.21 |   —    |
| subscriptions.rs functions   | 78.26%         | **100.00%**  | +21.74 |   —    |
| Codebase lines               | 85.30%         | **90.11%**   |  +4.81 |   —    |

(The brief cited a 77.85 % starting point — that figure post-dates the
L10b run captured in `closer-l10b-coverage.json`. Either way, the
post-W12-C figure clears 90 % comfortably.)

## Tests added (32 total, all in `subscriptions::tests`)

The pre-W12 test surface covered URL validation thoroughly (W10 L10b)
but left the DB-touching paths (`insert`, `delete`, `list`,
`record_dispatch`, `load_secret_hash`) and the HTTP send path (`send`,
`dispatch_event` thread plumbing) at near-zero coverage. The new
tests close those gaps using `tempfile::NamedTempFile` for an on-disk
SQLite (so dispatch threads can re-open the DB via
`Connection::open(db_path)`) and `wiremock` for HTTP (already a
dev-dep from W3 / W10).

### insert / delete / list (7 tests)

1. `insert_persists_and_list_returns_row` — end-to-end persist + read.
2. `insert_rejects_invalid_url` — `validate_url` failure short-circuits.
3. `insert_hashes_secret_before_persisting` — plaintext never lands
   in the DB; `secret_hash == sha256_hex(plaintext)`.
4. `insert_no_secret_stores_null` — `None` secret persists as NULL.
5. `delete_returns_true_when_row_removed` — happy path.
6. `delete_returns_false_when_row_missing` — idempotent path.
7. `list_orders_by_created_at_desc` — most-recent-first ordering.

### HMAC / sha256 helpers (4 tests)

8. `sha256_hex_known_vector` — RFC vectors for `""` and `"abc"`.
9. `hex_decode_round_trip_and_invalid` — even-length hex round-trips;
   odd-length and non-hex return `None`.
10. `hmac_long_key_is_hashed_to_fit_block` — exercises the key-too-long
    HMAC pre-step (key longer than the SHA-256 block size of 64 bytes).
11. `hmac_invalid_hex_key_falls_back_to_raw_bytes` — exercises the
    hex-decode fallback branch.

### matches_filters edge cases (2 tests)

12. `matches_filters_event_with_whitespace_and_star` — whitespace
    trim + wildcard inside comma list.
13. `matches_filters_agent_filter_requires_some` — agent-set + event
    has no agent → reject.

### record_dispatch / load_secret_hash (6 tests)

14. `record_dispatch_increments_counts_on_success` — two ok dispatches
    bump `dispatch_count` to 2 and leave `failure_count` at 0.
15. `record_dispatch_increments_failure_on_err` — failed dispatch
    bumps both counters by 1.
16. `record_dispatch_nonexistent_id_does_not_panic` — UPDATE with
    no matching row is a no-op; subsequent queries still work.
17. `record_dispatch_unopenable_db_path_is_noop` — exercises the
    early-`let-Err` short-circuit when `Connection::open` fails.
18. `load_secret_hash_returns_stored_hash` — happy path.
19. `load_secret_hash_missing_id_errs` — `query_row` Err is wrapped
    via `.context()`.

### dispatch_event thread plumbing (3 tests)

20. `dispatch_event_no_subs_is_noop` — empty table → early return.
21. `dispatch_event_filter_mismatch_skips_send` — events filter
    rejects → no thread spawned, counters unchanged.
22. `dispatch_event_namespace_filter_mismatch_skips` — namespace
    filter rejects → counters unchanged.

### send() — wiremock-driven HTTP tests (7 tests)

23. `send_returns_true_on_2xx` — happy path; mock asserts `expect(1)`.
24. `send_returns_false_on_5xx` — 5xx is a permanent failure inside
    `send` (no internal retry).
25. `send_returns_false_on_4xx` — 4xx is a permanent failure.
26. `send_signature_header_set_when_provided` — wiremock matcher
    asserts `x-ai-memory-signature: sha256=<sig>` and timestamp
    header.
27. `send_no_signature_header_when_secret_absent` — captured request
    must have no signature header but still must have the timestamp.
28. `send_rejects_ssrf_url_without_network` — guard short-circuits
    (no HTTP attempt, no server needed).
29. `send_rejects_invalid_scheme_without_network` — `ftp://` rejected
    by `validate_url` before any HTTP attempt.

### dispatch_event end-to-end (3 tests)

30. `dispatch_event_e2e_increments_dispatch_count_on_2xx` — full path
    from `dispatch_event` through the spawned thread, the DB
    `record_dispatch` write, and back to a poll-based DB read.
31. `dispatch_event_e2e_increments_failure_count_on_5xx` — same with
    a 5xx server.
32. `dispatch_event_e2e_signature_present_when_secret_set` — mock
    asserts `x-ai-memory-signature` header is set when the
    subscription has a secret.

## Quality gates

- `cargo fmt --check` ✓
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` ✓
- `cargo test --lib -- --test-threads=2` ✓ — **1179 passed**, 0 failed,
  0 ignored (was 1147 before this lane)

## Surprises / deviations

- **Test count overshoot.** Brief asked for ~15+; landed 32. The
  extra cases were cheap once the tempfile / wiremock fixtures were
  in place, and they push subscriptions.rs from 75 % to 97.61 %
  rather than the 90 % floor.
- **Brief's "retry-on-5xx" mention does not match production.** The
  current `send()` makes a single attempt and returns false on
  any non-2xx — there is no retry loop. The W12-C tests assert the
  documented single-attempt behaviour for 4xx/5xx and do not
  manufacture a retry path. If retry-with-backoff is desired, that's
  a production change.
- **End-to-end dispatch_event tests rely on poll-based observation.**
  `dispatch_event` spawns detached `std::thread`s with no join
  handles, so the e2e tests poll the DB / mock state with a 5 s budget
  (50 × 100 ms). The polls succeed in well under a second on the
  developer machine and the timeout exists only to bound flaky CI.
- **Disjoint from the seven other W12 closers.** No edits outside
  `src/subscriptions.rs`. Coverage gain on subscriptions.rs is +22.6
  pts; codebase-wide +4.8 pts.

## Commits

- `39c0eca` test(subscriptions): W12-C — deep coverage on
  dispatch/send/persistence
