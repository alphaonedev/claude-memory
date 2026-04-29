# Closer I7 — W7 binary-spawn integration tests

Branch: `cov-90pct-w7/integration-tests`
Base: `origin/cov-90pct-w6/daemon-runtime` (`ad6690d`)

## Command (verbatim)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --no-fail-fast --json -- --test-threads=2 > /tmp/closer-i7-cov.json 2>&1
```

## Headline numbers

| Surface         | Pre-W7  | Post-W7 | Delta    |
|-----------------|---------|---------|----------|
| Codebase line   | 85.61%  | 85.85%  | +0.24 pp |
| Codebase region | n/a     | 86.18%  | n/a      |
| Codebase fn     | n/a     | 84.49%  | n/a      |

**Lines covered:** 27,805 / 32,386.

The deferred subprocess-spawn tests this lane added do **not** attribute
coverage to the parent `cargo-llvm-cov` run by design — the child binary
reports its own counters under its own pid that the parent never reaps.
The +0.24 pp delta vs. W6 is incidental drift from the new lib-side
helper code in the test files (the `free_port`/`spawn_serve` helpers
that link a few lines of std::net into the test crates). The lane is a
regression-guard lane, not a coverage-driver lane.

## Files added

- `tests/cli_integration.rs` — 20 tests, ~470 lines.
- `tests/serve_integration.rs` — 6 tests, ~370 lines.
- `tests/mcp_integration.rs` — 3 tests, ~270 lines.

`Cargo.toml` adds `assert_cmd = "2"`, `predicates = "3"`, and a unix-only
`libc = "0.2"` dev-dep (used by the SIGINT graceful-shutdown test).
`reqwest`'s `blocking` feature was already enabled on the production dep.

## Tests added

### `cli_integration` (20)

1. `binary_help_succeeds`
2. `binary_version_succeeds`
3. `each_subcommand_help` — parametrised over the 33 subcommands listed
   in `Cli::Command` (every single one must accept `--help` and exit 0).
4. `store_then_get_roundtrip`
5. `store_then_recall` — `--tier keyword` to skip embedder cold-start.
6. `store_then_list`
7. `store_then_search`
8. `store_then_delete` — confirms get-after-delete returns exit 1.
9. `store_with_stdin_content` — `-c -` reads from stdin.
10. `export_import_roundtrip` — store 5, export to JSON, import into a
    fresh DB, list shows 5.
11. `stats_empty_db`
12. `stats_with_data`
13. `namespaces_command`
14. `forget_by_namespace`
15. `link_two_memories_then_get_links`
16. `consolidate_three_into_one`
17. `shell_quit_immediately`
18. `invalid_subcommand_errors_with_useful_message`
19. `missing_required_arg_errors`
20. `invalid_tier_errors_with_validation_message`

### `serve_integration` (6)

1. `serve_health_endpoint_returns_200`
2. `serve_metrics_endpoint_at_root_path` — checks Prometheus content
   type + `# HELP` / `# TYPE` markers.
3. `serve_metrics_endpoint_at_v1_path`
4. `serve_create_then_get_memory` — POST then GET via real HTTP.
5. `serve_api_key_required_when_configured` — drops a `config.toml`
   under a fake `HOME` (because `api_key` is config-only, not
   env-driven) and asserts 401 without the header, 200 with it.
6. `serve_graceful_shutdown_on_sigterm` — sends `SIGINT` (the daemon's
   wired signal — `tokio::signal::ctrl_c()`), asserts graceful exit
   within 10s. Test name kept from the brief; comment in the test body
   documents the SIGINT vs SIGTERM choice.

### `mcp_integration` (3)

1. `mcp_initialize_handshake_succeeds` — JSON-RPC `initialize`
   round-trip; asserts `serverInfo.name == "ai-memory"`.
2. `mcp_list_tools_returns_expected_count` — `tools/list` returns at
   least 40 tools (lib unit test pins 43; integration test uses a
   lower bound so a future tool addition doesn't break this lane).
3. `mcp_call_memory_store_then_memory_recall_roundtrip` — stores via
   `tools/call memory_store`, recalls via `tools/call memory_recall`
   with the same unique token in `--tier keyword` mode.

All read paths use a worker thread + `mpsc::Receiver::recv_timeout`
(10s) so a hung response surfaces as a test failure, not a CI hang.

## Quality gates

- `cargo fmt --check` — clean.
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` — clean.
- `cargo test --lib -- --test-threads=2` — 916/916 pass.
- `cargo test --bin ai-memory` — 11/11 pass.
- `cargo test --test integration -- --test-threads=2` — 210/210 pass.
- `cargo test --test cli_integration -- --test-threads=2` — 20/20 pass.
- `cargo test --test serve_integration -- --test-threads=2` — 6/6 pass.
- `cargo test --test mcp_integration -- --test-threads=2` — 3/3 pass.

Pedantic clippy on the test crates emits a handful of style warnings
(e.g. `unused_self`, `needless_pass_by_value` on test-only helpers).
The brief-mandated clippy gate is `--bin ai-memory --lib`, which is
clean.

## Tests gated `#[ignore]`

None. All 29 tests run on every `cargo test`.

## Bugs found / deferred

None. Two minor notes worth surfacing:

- `serve` only logs the *input* address ("listening on http://0.0.0.0:0")
  rather than the actual bound port, which is why the integration tests
  use a `free_port()` bind-and-drop helper instead of parsing stdout.
  Fixing this is a one-line change in `daemon_runtime::serve` (after
  `axum::serve`'s listener binds, log `listener.local_addr()`) but
  out-of-scope for I7 — the brief explicitly excludes any `src/`
  modifications. Filing as a follow-up issue is appropriate.

- `api_key` is loaded from `~/.config/ai-memory/config.toml` only; the
  brief suggested an `AI_MEMORY_API_KEY` env var that doesn't exist in
  the codebase. The auth test compensates by writing a `config.toml`
  under a fake `HOME` and is documented in-test.

## Surprises / deviations

The biggest surprise was that `--port 0` (clap's default-value-of-0
behaviour) is *accepted* by the serve command but useless for tests
because the daemon never logs the OS-assigned port — the serve
infrastructure only formats and logs the input string `"127.0.0.1:0"`.
This is the right architectural call (logging happens before the bind
in the non-TLS code path, where `serve_http_with_shutdown_future` owns
the actual `TcpListener::bind`), but it forces tests into a pre-bind
port-finder pattern. I documented the trade-off inline. The SIGINT vs
SIGTERM choice for the shutdown test is similar — daemon listens for
`ctrl_c` (SIGINT on unix), not SIGTERM, so the test renames its target
signal to match what production actually wires while keeping the brief's
test name. Otherwise everything in the brief landed verbatim.
