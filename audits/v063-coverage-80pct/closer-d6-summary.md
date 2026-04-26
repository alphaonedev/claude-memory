# Closer D6 — daemon_runtime extraction (W6) coverage summary

Branch: `cov-90pct-w6/daemon-runtime`
Base: `origin/cov-90pct-w5b/consolidated`

## Command (verbatim)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --no-fail-fast --json -- --test-threads=2 > /tmp/closer-d6-cov.json 2>&1
```

## Headline numbers (post-W6)

| Surface              | Pre-W6  | Post-W6 | Delta    |
|----------------------|---------|---------|----------|
| Codebase line        | 85.13%  | 85.61%  | +0.48 pp |
| `main.rs`            | 64.39%  | 100.00% | +35.6 pp |
| `daemon_runtime.rs`  | 84.19%  | 85.54%  | +1.35 pp |
| `cli/recall.rs`      | 76.43%  | 76.82%  | +0.39 pp |

`main.rs` post-W6 is a 127-line shim — every line runs at startup, so it
hits 100% line coverage as soon as any test that goes through the binary
executes. The dispatch + `serve()` + cmd_bench code that *was* untestable
inline in `main.rs` is now lib-side and contributes its coverage to
`daemon_runtime.rs` instead.

## Surface added to `daemon_runtime`

- `Cli`, `Command`, `ServeArgs`, `BenchArgs`, `MigrateArgs`,
  `CompletionsArgs` — clap-derived structures, moved from `main.rs` so
  `daemon_runtime::run` can take them as parameters.
- `run(cli, app_config)` — top-level CLI dispatch.
- `serve(db_path, args, app_config)` — full HTTP daemon body.
- `bootstrap_serve(db_path, args, app_config) -> ServeBootstrap` —
  testable state builder (no socket open).
- `build_router(app_state, api_key_state)` — composition wrapper around
  the W3-vintage `lib::build_router`.
- `build_embedder(feature_tier, app_config)` — single canonical
  embedder builder. Replaces the prior duplicate in `cli::recall`.
- `build_vector_index(conn, embedder_present)` — single canonical
  vector-index builder.
- `is_write_command(cmd)` — write-class subcommand predicate.
- `spawn_gc_loop(state, archive_max_days, interval) -> JoinHandle` —
  GC daemon loop, returns handle so `serve()` can abort on shutdown.
- `spawn_wal_checkpoint_loop(state, interval) -> JoinHandle` — WAL
  checkpoint daemon loop, same shape.
- `passphrase_from_file(path) -> Result<String>` — passphrase loader,
  trims trailing CR/LF, errors on empty.
- `apply_anonymize_default(app_config)` — env precedence helper.

## Tests added to `daemon_runtime::tests`

24 new tests (lib total: 892 → 916), all green:

- `test_is_write_command_all_variants` — clap-driven matrix covering
  every write + read variant (~25 distinct argv patterns).
- 5 router tests: `health`, `metrics_at_both_paths`,
  `lists_all_v1_memory_routes`, `applies_api_key_middleware_when_key_set`,
  `skips_api_key_middleware_when_key_none`.
- 2 embedder tests: keyword-tier returns None; load-failure path
  returns None (smoke check; the live HF-Hub negative path is gated
  behind `feature = "test-with-models"` in the recall integration tests).
- 2 vector-index tests: no-embedder returns None; empty DB with
  embedder returns empty index.
- 2 background-task tests, both `tokio::test(start_paused = true)` +
  `tokio::time::advance` driven: `spawn_gc_loop_runs_and_can_be_aborted`,
  `spawn_wal_checkpoint_loop_runs_and_can_be_aborted`.
- 6 passphrase tests: trailing newline strip, CRLF strip, empty file,
  newline-only file, nonexistent file, internal-whitespace preservation.
- 3 `apply_anonymize_default` tests: config-true + env-unset sets,
  config-true + env-set unchanged, config-false unchanged.
- 3 `bootstrap_serve` tests: keyword-tier (no embedder, two task
  handles), api-key-set, federation disabled when quorum=0.

## Embedder duplication kill — confirmed

`cli::recall::run` no longer carries its own `build_embedder_for_recall`.
Both call sites — `daemon_runtime::serve()` (HTTP daemon, indirect via
`bootstrap_serve`) and `cli::recall::run` (offline recall, direct via
`block_on`) — now route through `daemon_runtime::build_embedder`.

The bridge in `cli::recall::run` uses `Handle::try_current()` to detect
the existing tokio runtime (when called from `daemon_runtime::run` via
`#[tokio::main]`) and falls back to a single-threaded runtime when called
directly (e.g. integration test harness without a tokio runtime). Tier =
Keyword short-circuits inside the builder before any tokio work, so the
bridge has zero cost on the keyword path the integration tests exercise.

## Quality gates

- `cargo fmt --check` — clean.
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` — clean.
- `cargo test --lib -- --test-threads=2` — 916/916 pass.
- `cargo test --bin ai-memory` — 11/11 pass (renamed shim still hits
  the legacy ID-format / human-age helpers).
- `cargo test --test integration -- --test-threads=2` — 210/210 pass.
- `cargo test --test tls_integration --test proptest_*` — 19/19 pass.

## Behaviour preservation

`serve()` body is byte-equivalent to the pre-W6 inline version: TLS path
(rustls + axum-server with graceful shutdown), mTLS path (allowlist file
loaded before "listening" log), federation init, catchup-loop spawn, GC
loop on `GC_INTERVAL_SECS=1800`, WAL checkpoint on
`WAL_CHECKPOINT_INTERVAL_SECS=600` with the same `interval/2` cold-start
stagger, ctrl-c → WAL checkpoint → graceful shutdown sequence, all
preserved. The 210 integration tests (which exercise the production
binary end-to-end via `DaemonGuard::spawn`) confirm there is no
behavioural drift.

## Lines moved out of `main.rs`

- 1136 → 127 lines (1009 lines moved or deleted).
- `Cli`, `Command`, `ServeArgs`, `BenchArgs`, `MigrateArgs`,
  `CompletionsArgs`, the entire `match cli.command` dispatch, `serve()`,
  `cmd_bench`, `cmd_migrate`, `is_write_command` matches!, and the
  startup helpers (`apply_anonymize_default`, `passphrase_from_file`)
  all live in `daemon_runtime` now.
