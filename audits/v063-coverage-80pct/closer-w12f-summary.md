# Closer W12-F — daemon_runtime.rs deeper coverage

Branch: `cov-90pct-w12/daemon-deeper`
Base: `origin/cov-90pct-w11/consolidated`

## Command (verbatim)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --features sal --no-fail-fast --json -- --test-threads=2 \
  > /tmp/closer-w12f-post.json
```

## Headline numbers

| Surface              | Pre (W11) | Post (W12-F) | Delta    |
|----------------------|-----------|--------------|----------|
| `daemon_runtime.rs`  | 85.73%    | 93.43%       | +7.70 pp |
| Codebase line        | 89.75%    | 90.09%       | +0.34 pp |
| `daemon_runtime.rs` regions | 82.71% | 90.70%   | +7.99 pp |
| `daemon_runtime.rs` functions | 85.71% | 92.36% | +6.65 pp |

(Pre-numbers are the full lib + integration test run including W11.)

## Tests added (29)

In `src/daemon_runtime.rs::tests`. APPEND only — no production code touched.

### `bootstrap_serve` variants (3)

- `test_bootstrap_serve_federation_enabled_attaches_config` — quorum=1
  + one peer + catchup_interval=0 (covers the `if let Some(ref fed)` and
  `else { catchup loop disabled }` branches).
- `test_bootstrap_serve_federation_enabled_with_catchup_loop` — quorum=1
  + catchup_interval=3600 (covers the `if args.catchup_interval_secs > 0`
  + `federation::spawn_catchup_loop` branch).
- `test_bootstrap_serve_federation_invalid_peer_errors` — duplicate peer
  URL hits `FederationConfig::build`'s #341 guard, exercising the
  `.context("federation config")` failure path.

### `build_vector_index` populated DB (1)

- `test_build_vector_index_populated_db_returns_built_index` — covers the
  `Ok(entries) if !entries.is_empty() => VectorIndex::build` arm that
  the empty-DB test never hits.

### Background tasks (2)

- `test_spawn_gc_loop_purges_expired_memories` — seeds an expired memory
  and `archive_max_days=Some(1)` so the gc loop's `Ok(n) if n > 0` and
  `auto_purge_archive` branches both fire (lines 822-828).
- `test_spawn_wal_checkpoint_loop_runs_multiple_cycles` — advances
  paused time across four cycles to exercise the loop body more than
  once (line 849 logging arm).

### `urlencoding_minimal` (1)

- `test_urlencoding_minimal_round_trip` — covers line 1559 (the
  reserved-char `_ =>` arm) and verifies sync-daemon RFC3339-shaped
  inputs round-trip correctly.

### `run` dispatch arms (15)

Each parses an argv via `Cli::try_parse_from`, hands the resulting `Cli`
to `daemon_runtime::run`, and asserts the dispatch path returned `Ok`:

- `test_run_dispatch_stats_command`, `_namespaces_command`,
  `_export_command`, `_list_command`, `_search_command`,
  `_archive_list_command`, `_agents_list_command`,
  `_pending_list_command`, `_completions_command`, `_man_command`,
  `_get_command`, `_resolve_command` — read-only / no-write paths.
- `test_run_dispatch_gc_triggers_post_run_checkpoint`,
  `_promote_triggers_write_checkpoint` — write-class commands which
  exercise the post-run WAL checkpoint branch (lines 638-644).
- `test_run_with_db_passphrase_file_exports_env` — covers lines 371-375
  (the `--db-passphrase-file` arm calling `passphrase_from_file` +
  `env::set_var`).

### Bench dispatch (2)

- `test_run_dispatch_bench_smoke_runs_one_iteration` — iter=1, warmup=0
  human-readable output (top-to-bottom of `cmd_bench`).
- `test_run_dispatch_bench_json_with_history` — `--json` + `--history`
  branches, including `bench::append_history` invocation and the
  history-file write side effect.

### Migrate dispatch (2, sal feature)

- `test_run_dispatch_migrate_sqlite_to_sqlite_dry_run` — `--dry-run` +
  human-readable text output.
- `test_run_dispatch_migrate_json_output` — `--json` output branch.

### Tracing init (1)

- `test_init_tracing_is_idempotent` — verifies repeated calls don't
  panic (the `try_init` ignored-Err path).

### `serve_http_with_shutdown_future` (2)

- `test_serve_http_with_shutdown_future_serves_then_stops` — full
  in-process serve + shutdown round trip on a free port.
- `test_serve_http_with_shutdown_future_bind_failure_errors` — bind
  failure surfaces via the `with_context("bind {addr}")` chain.

## Quality gates

- `cargo fmt --check`: pass
- `cargo clippy --bin ai-memory --lib --features sal -- -D warnings -D clippy::all -D clippy::pedantic`: pass
- `cargo test --lib --features sal -- --test-threads=2`: 1191/1191 pass

(The `cargo clippy --tests` run reports preexisting issues in
`tests/integration.rs`, `tests/serve_integration.rs`,
`tests/proptest_*.rs`, and `tests/cli_integration.rs` — none in
`daemon_runtime.rs`. Per W12-F charter those files are out of scope.)

## Surprises / deviations

The `test_bootstrap_serve_federation_invalid_peer_errors` test originally
used `.unwrap_err()` which requires `T: Debug`; `ServeBootstrap` doesn't
derive `Debug` (production code is out of scope to modify), so the test
uses a `match res { Ok(_) => panic!(...), Err(e) => e }` shape instead.

Lines remaining uncovered in `daemon_runtime.rs` after W12-F (the 6.57%
gap to 100%):

- `serve()` rustls/axum-server TLS branch (lines 1041-1099) — requires a
  real TLS handshake against a bound TCP socket plus a client; the W12-F
  charter explicitly leaves the `bind_and_serve` body alone.
- `cmd_bench` regression-baseline + budget-failure error branches —
  require a baseline JSON file shaped like a previous run.
- `sync_cycle_once` PUSH/PULL paths against a real peer — covered by the
  existing in-process `serve_http_with_shutdown` integration tests at
  the system level, not by unit tests.
- A few `_ => {}` guard-fallthrough arms inside the embedder error-log
  block which only fire on a live HuggingFace network failure.

## Commits

(see `git log --oneline cov-90pct-w11/consolidated..HEAD`)
