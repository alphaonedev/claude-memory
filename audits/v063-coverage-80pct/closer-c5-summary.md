# Closer C5 — CLI CRUD/Link/Forget/Promote (Wave 5b) Summary

**Branch:** `cov-90pct-w5b/cli-crud`
**Base:** `cov-90pct-w5a/cli-foundation`
**Date:** 2026-04-26

## What Closer C5 did

Wave 5b/C5 migrates the next batch of CLI handlers out of `src/main.rs`
behind the `CliOutput` contract S5 published, and lifts the
`enforce_governance` match block out of every governed `cmd_*` into a
single shared helper.

Five NEW modules:

| Module | Purpose |
|--------|---------|
| `src/cli/governance.rs` | Shared `enforce` helper (Allow/Pending/Deny outcome) — caller owns `process::exit` |
| `src/cli/crud.rs`       | `cmd_get` / `cmd_list` / `cmd_delete` + `GetArgs` / `ListArgs` / `DeleteArgs` |
| `src/cli/link.rs`       | `cmd_link` / `cmd_resolve` + `LinkArgs` / `ResolveArgs` |
| `src/cli/forget.rs`     | `cmd_forget` + `ForgetArgs` |
| `src/cli/promote.rs`    | `cmd_promote` + `PromoteArgs` |

`main.rs` now only contains `Cli` / `Command` clap derives and the
per-arm dispatch shim (each arm constructs a `CliOutput` from
`stdout().lock()` / `stderr().lock()` and calls into `cli::*`).

## Public API surface (governance helper)

```rust
// src/cli/governance.rs
pub enum GovernanceOutcome { Allow, Pending, Deny }

pub fn enforce(
    conn: &Connection,
    action: GovernedAction,
    namespace: &str,
    caller_agent_id: &str,
    memory_id: Option<&str>,
    memory_owner: Option<&str>,
    payload: &serde_json::Value,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<GovernanceOutcome>;
```

The helper writes the print-side of Pending (text + JSON shapes) and
Deny (stderr) and returns the `GovernanceOutcome`. **It does NOT call
`std::process::exit`** — exits stay inline at the call-site so the
helper is fully testable in-process.

## Tests added

| Surface | File | Count |
|---------|------|------:|
| `cli::governance::enforce` (Allow/Pending/Deny x text+JSON) | `src/cli/governance.rs` | 6 |
| `cmd_get` / `cmd_list` / `cmd_delete` | `src/cli/crud.rs` | 15 |
| `cmd_link` / `cmd_resolve` | `src/cli/link.rs` | 7 |
| `cmd_forget` | `src/cli/forget.rs` | 6 |
| `cmd_promote` | `src/cli/promote.rs` | 7 |
| **Total NEW** | | **41** |

Lib-test count: **757 → 798** (Δ +41).

## Coverage measurement

### Exact command (verbatim, re-runnable)

```sh
LLVM_COV=/Users/fate/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin/llvm-cov \
LLVM_PROFDATA=/Users/fate/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin/llvm-profdata \
AI_MEMORY_NO_CONFIG=1 cargo llvm-cov --lib --bins --summary-only --ignore-run-fail
```

### Results

| Surface | Pre (S5) | Post (C5) | Δ |
|---------|----------|-----------|----|
| **Codebase line %** | 72.71% | **74.97%** | +2.26 pp |
| **Codebase region %** | 73.38% | **75.34%** | +1.96 pp |
| `src/main.rs` (line) | 3.11% (65/2090) | 3.56% (65/1824) | denominator shrinks (extraction) |
| `src/main.rs` (regions) | 4.01% (133/3315) | 4.38% (133/3035) | — |
| `src/cli/crud.rs`        | NEW | **95.93% (401/418)** | — |
| `src/cli/link.rs`        | NEW | **96.76% (179/185)** | — |
| `src/cli/forget.rs`      | NEW | **98.08% (153/156)** | — |
| `src/cli/promote.rs`     | NEW | **94.49% (257/272)** | — |
| `src/cli/governance.rs`  | NEW | **97.45% (267/274)** | — |
| `src/cli/store.rs`       | 89.10% | 94.28% | helper amendment lifted store.rs from 89.10 → 94.28 |

**main.rs note:** the line% percentage barely moves because the
denominator was already mostly clap-derive boilerplate. The functional
delta is **lines extracted: 360** (3470 → 3110, raw `wc -l`).

## Quality gates

| Gate | Status |
|------|--------|
| `cargo fmt --check` | clean |
| `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic` | clean (lib + bin; matches CI) |
| `cargo test --lib` | 798 pass (was 757, +41 new) |
| `cargo build --bin ai-memory` | builds |
| Smoke (`store` then `list` against a fresh tempdb) | output unchanged from baseline |

## store.rs amendment (S5's lane)

`src/cli/store.rs` was originally S5's territory. The shared
`cli::governance::enforce` helper this lane introduces makes it
attractive to dedupe `store::run`'s inline governance match. **One
small edit applied:** the inline `match db::enforce_governance(...)`
block in `store::run` is replaced with a 3-arm match over
`GovernanceOutcome::{Allow, Pending, Deny}`. Behaviour is unchanged
(both Pending text and Pending JSON shapes are byte-for-byte identical;
Deny still exits 1; payload field set is identical).

This amendment is documented here per the C5 brief — the abstraction
is C5's lane, and the call-site update keeps store.rs from carrying a
parallel-but-different governance printer.

## Files

- `src/cli/governance.rs` (NEW) — shared enforce helper + 6 unit tests
- `src/cli/crud.rs`       (NEW) — cmd_get/list/delete + 15 unit tests
- `src/cli/link.rs`       (NEW) — cmd_link/resolve + 7 unit tests
- `src/cli/forget.rs`     (NEW) — cmd_forget + 6 unit tests
- `src/cli/promote.rs`    (NEW) — cmd_promote + 7 unit tests
- `src/cli/mod.rs`        — added `pub mod {crud, forget, governance, link, promote};`
- `src/cli/store.rs`      — governance match block replaced with helper call (S5 amendment)
- `src/main.rs`           — 7 cmd_* fns deleted, 7 args structs deleted, dispatch arms wired

Other lanes (`cli/store.rs` body except the governance amendment,
`cli/update.rs`, `cli/io.rs`, `cli/io_writer.rs`, `cli/test_utils.rs`,
`cli/helpers.rs`, `cli/mod.rs` re-exports) were left intact.

## Surprises / deviations

- **`process::exit(1)` retained inline at call-sites.** As in S5,
  restructuring exit calls into anyhow returns is a behavioural change
  (exit-code shift); kept out of scope. The pre-exit messages are now
  `writeln!(out.stderr, ...)?` and the helper-printed text is
  testable; the literal `exit(1)` line is what stays under integration-
  suite coverage.
- **The governance helper owns the Pending/Deny print formats.** Text
  and JSON shapes are identical to the pre-extraction inline blocks
  for `store`, `delete`, and `promote`. Verified by re-running the
  W5a `test_store_governance_pending_writes_pending_status` case
  unchanged (now exercises the helper indirectly).
- **`test_promote_governance_deny` rewired** to call the governance
  helper directly. The original cmd_promote Deny branch hits
  `process::exit(1)` which would tear down the test runner; the helper
  is the testable surface, so the deny-print contract is asserted
  there + in `cli::governance::tests::test_governance_deny_writes_reason_to_stderr`.
- **`test_promote_nonexistent_exits_nonzero` proxies via validate_id**
  with a null-byte input so the validator-error branch fires before
  the not-found `process::exit(1)`. The exact "not found" exit path is
  covered by the integration suite that spawns the binary.
- **Hierarchical-namespace promotion test uses `parent/child`** (not
  `parent-child`). `db::promote_to_namespace` walks `namespace_ancestors`
  which splits on `/`, not `-`. The hyphenated form is parent
  *auto-detection* via `namespace_meta.parent_namespace`, which is
  separate from ancestor-walk validation.
- **MCP test flake (`tools_call_emits_span_with_tool_name_and_elapsed_ms`)**
  is pre-existing on the W5a baseline and unrelated. The flake fires
  intermittently when the lib test suite runs alongside `--bins`. All
  C5 tests pass; the lib-only run is clean.
