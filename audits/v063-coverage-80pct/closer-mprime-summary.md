# Closer M-prime — Coverage Repair Summary

**Branch:** `cov-80pct-w3/main-daemons-tests`
**Commit:** `00f81ac` (test: cover three new W3 daemon_runtime variants)
**Date:** 2026-04-26

## What Closer M-prime did

Closer M migrated production `serve()`, `cmd_sync_daemon()`, `cmd_curator()`
in `src/main.rs` to use `daemon_runtime.rs` helpers — but added zero tests
for the three NEW helper variants the migration required. Closer M's
original coverage measurement also excluded the lib target, where most of
the test suite now lives after the `mod foo` -> `use ai_memory::*` refactor.

This pass adds three integration tests directly targeting the new variants
(production-shaped inputs, parallel-named to the Wave 2 X exemplars), and
re-runs coverage with the correct command that includes lib + bin +
integration targets.

## Tests added (`tests/integration.rs`)

| Test | Variant covered | Status |
|------|-----------------|--------|
| `test_daemon_serve_http_with_shutdown_future_runs_with_custom_cleanup` | `serve_http_with_shutdown_future` (custom Future shutdown) | passing |
| `test_daemon_sync_with_shutdown_using_client_accepts_custom_client` | `run_sync_daemon_with_shutdown_using_client` (caller-built `reqwest::Client`) | passing |
| `test_daemon_curator_with_primitives_runs_with_dry_run_config` | `run_curator_daemon_with_primitives` (primitive-arg flavour) | passing |

The first two were already transitively reachable via the OLD wrapper's
trivial Notify-wrapping happy path; the new tests force the interesting
non-default code paths (a non-trivial async cleanup future; a custom-
configured client). The third variant was *fully dark* prior to this
commit since the typed-CuratorConfig path doesn't dispatch through it.

## Coverage measurement

### Exact command used (verbatim — re-runnable)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --no-fail-fast --json -- --test-threads=2 \
  > /tmp/closer-mprime-cov.json 2>&1
```

(The `LLVM_COV` / `LLVM_PROFDATA` env vars are required because the local
toolchain is Homebrew-installed `rust 1.95.0`, which does not ship
`llvm-tools-preview` under the rustup component name. Homebrew's `llvm`
formula provides them. On a CI box with `rustup`-installed toolchain plus
`rustup component add llvm-tools-preview`, the env-var prefix can be
dropped.)

This is the all-targets form (no `--bin` / `--test` filters), which
attributes coverage from **lib unit tests + bin unit tests + integration
tests** — the form the post-refactor codebase actually requires. Closer M's
original `cargo llvm-cov --bin ai-memory --test integration ...` ran *only*
the bin and integration test executables, missing the 565-test lib suite
where most of the codebase's tests now live.

### Results

| Surface | Lines covered | Total lines | Percent |
|---------|---------------|-------------|---------|
| **Codebase (overall)** | 19825 | 26115 | **75.91%** |
| `src/main.rs` | 1367 | 2767 | **49.40%** |
| `src/daemon_runtime.rs` | 213 | 253 | **84.19%** |

Functions: 1626 / 2160 = 75.28%
Regions: 32792 / 42512 = 77.14%

`daemon_runtime.rs` jumped from **70.36%** (Closer M's snapshot) to
**84.19%** (+13.83 pts) — the three new variants are now exercised
directly, so their bodies are no longer dark. `main.rs` is unchanged at
49.40% as expected (production code untouched). The codebase-wide line
coverage is **75.91%** — within striking distance of the 80% campaign
target, with the post-refactor measurement now reflecting the true state.

## Quality gates

| Gate | Status |
|------|--------|
| `cargo fmt --check` | clean |
| `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` | clean |
| `cargo test --bin ai-memory` | 11 pass |
| `cargo test --lib` | 565 pass |
| `cargo test --test integration -- --test-threads=2` | 210 pass (was 207, +3 new) |

## Files

- Tests: `tests/integration.rs` (lines after the prior trailing curator
  test, additive only — no Wave 2 X tests touched).
- Production code: untouched.
- Coverage data: `audits/v063-coverage-80pct/closer-mprime-coverage.json`
  (curated per-file summary + totals).

## Commit

- `00f81ac` — `test(daemon_runtime): cover three new W3 helper variants`
