# Closer X5 — W5b CLI Longtail

**Branch:** `cov-90pct-w5b/cli-longtail`
**Base:** `cov-90pct-w5a/cli-foundation` (S5 foundation)

## Tests added (73 total)

| Module                  | Count |
| ----------------------- | ----- |
| `cli/consolidate.rs`    | 7     |
| `cli/sync.rs`           | 10    |
| `cli/archive.rs`        | 9     |
| `cli/agents.rs`         | 8     |
| `cli/backup.rs`         | 7     |
| `cli/curator.rs`        | 9     |
| `cli/gc.rs`             | 11    |
| `cli/shell.rs`          | 12    |

## Coverage (cargo llvm-cov --bins --lib --tests)

| File                       | Lines (post)  | Functions     |
| -------------------------- | ------------- | ------------- |
| `cli/consolidate.rs` NEW   | **93.31%**    | 92.98%        |
| `cli/sync.rs`        NEW   | **75.00%**    | 76.52%        |
| `cli/archive.rs`     NEW   | **90.77%**    | 85.63%        |
| `cli/agents.rs`      NEW   | **87.84%**    | 85.02%        |
| `cli/backup.rs`      NEW   | **95.74%**    | 91.89%        |
| `cli/curator.rs`     NEW   | **74.22%**    | 72.69%        |
| `cli/gc.rs`          NEW   | **98.86%**    | 94.27%        |
| `cli/shell.rs`       NEW   | **74.92%**    | 83.28%        |
| `main.rs`                  | **62.24%**    | 65.63%        |
| **TOTAL**                  | **84.20%**    | 84.65%        |

main.rs went from 48.09% → 62.24% (Δ +14.15pp).

## Lines moved out of main.rs

`main.rs`: 3470 → 1843 lines (**−1627 lines**, or 47% reduction).

## Quality gates

- `cargo fmt --check` ✓
- `cargo clippy --bin ai-memory -- -D warnings -D clippy::all -D clippy::pedantic` ✓
- `cargo test` ✓ (830 lib tests pass, 11 bin tests pass, 210 integration tests pass)

## shell.rs uncovered lines

The outer stdin loop (`run` function, lines ~209-238) is intentionally
unreachable from buffer-driven unit tests — its blocking `read_line`
deadlocks a `Vec<u8>` fixture. The line-handling logic was extracted
into `handle_command(parts, &conn, &mut out) -> ShellAction` and is
exhaustively tested. The 25% uncovered surface in `shell.rs` is the
stdin loop + the wrapper around it.

## Surprises / deviations

- **`restamp_agent_id` is duplicated** in `cli/sync.rs` and `cli/io.rs`.
  The original lived in `main.rs` as a private free fn; `cli/io.rs`
  inlined its logic during W5a; rather than introduce a fresh utility
  module, X5 mirrored the inline pattern. A future refactor could
  extract it to `cli/helpers.rs`, but adding to test_utils/helpers was
  out of W5b scope.
- **`SyncPreview` / `MergeOutcome`** moved into `cli/sync.rs` as
  private helpers (preserving original in-file structure). No tests
  target them directly; the dry-run handler tests exercise the
  `classify` + accumulator branches.
- **Curator daemon mode** (`--daemon`) and **sync daemon mode** are
  delegated to `daemon_runtime` per the W3 contract; X5 only owns the
  outer wrapper (arg parsing, shutdown notify, client construction).
  Tests cover the wrapper guards (`--peers` empty, `--insecure` without
  mTLS) but do not start a real daemon loop.
- **Restore workflows in archive.rs** — the `Restore { id }` arm
  branches into `process::exit(1)` when the id is not found in the
  archive table. To avoid terminating the test process, the unit test
  seeds + archives a memory directly via `db::archive_memory(conn, id, None)`
  and then exercises only the success branch. The exit branch is
  hardened in integration tests.
- **No edits to test_utils.rs / helpers.rs / io_writer.rs / mod.rs** —
  X5 only added new modules to `mod.rs`'s pub-mod list (per the
  contract, that did not require a stop-and-report).
