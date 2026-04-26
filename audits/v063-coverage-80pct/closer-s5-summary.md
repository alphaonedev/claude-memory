# Closer S5 — CLI Foundation (Wave 5a) Summary

**Branch:** `cov-90pct-w5a/cli-foundation`
**Base:** `cov-90pct-w4/consolidated`
**Date:** 2026-04-26

## What Closer S5 did

Wave 5a is the FOUNDATION lane for the CLI extraction. This closer carved
~608 lines of CLI handler logic out of `src/main.rs` into a brand-new
`src/cli/` module tree, behind a stable `pub` API surface that downstream
W5 closers (R5/C5/X5) branch off.

Three NEW shared sub-modules underpin the migration:

| Module | Purpose |
|--------|---------|
| `src/cli/io_writer.rs` | `CliOutput` — output abstraction so handlers route through `&mut dyn Write` instead of `println!` |
| `src/cli/test_utils.rs` | `TestEnv` + `seed_memory` — per-test fixture (`#[cfg(test)]`-gated) |
| `src/cli/helpers.rs` | `id_short` / `auto_namespace` / `human_age` migrated verbatim |

Five handler migrations landed in this lane:

| Handler | Module |
|---------|--------|
| `cmd_store` → `cli::store::run` | `src/cli/store.rs` |
| `cmd_update` → `cli::update::run` | `src/cli/update.rs` |
| `cmd_export` → `cli::io::export` | `src/cli/io.rs` |
| `cmd_import` → `cli::io::import` | `src/cli/io.rs` |
| `cmd_mine` → `cli::io::mine` | `src/cli/io.rs` |

The 4 clap-derived arg structs (`StoreArgs`, `UpdateArgs`, `ImportArgs`,
`MineArgs`) moved with their handlers to `cli/`. `main.rs`'s `Cli`/`Command`
clap structs and the dispatch arms are the only `main.rs` lines touched.

## Public API surface (stable contract for R5/C5/X5)

```rust
// src/cli/io_writer.rs
pub struct CliOutput<'a> {
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}
impl<'a> CliOutput<'a> {
    pub fn from_std(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write) -> Self;
}

// src/cli/test_utils.rs   (#[cfg(test)] only)
pub struct TestEnv {
    pub db_path: PathBuf,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    // _tmp: TempDir — held to keep tempdir alive
}
impl TestEnv {
    pub fn fresh() -> Self;
    pub fn output(&mut self) -> CliOutput<'_>;
    pub fn stdout_str(&self) -> &str;
    pub fn stderr_str(&self) -> &str;
}
pub fn seed_memory(db_path: &Path, namespace: &str, title: &str, content: &str) -> String;

// src/cli/helpers.rs
pub fn id_short(id: &str) -> &str;
pub fn auto_namespace() -> String;
pub fn human_age(iso: &str) -> String;
```

**Borrow-checker note for downstream closers:** `TestEnv::output()` borrows
`self.stdout` and `self.stderr` mutably. Take a snapshot of `db_path`
*before* calling `output()` (`let db = env.db_path.clone();`) so the
borrow checker doesn't trip.

## Tests added

| Surface | File | Count |
|---------|------|------:|
| `CliOutput` | `src/cli/io_writer.rs` | 3 |
| `helpers` (id_short/auto_namespace/human_age) | `src/cli/helpers.rs` | 14 |
| `cmd_store` | `src/cli/store.rs` | 13 |
| `cmd_update` | `src/cli/update.rs` | 6 |
| `cmd_export` / `cmd_import` / `cmd_mine` | `src/cli/io.rs` | 11 |
| **Total NEW** | | **47** |

The 9 inline helper tests in `main.rs::tests` were left in place — they
now resolve via `use ai_memory::cli::helpers::*` and continue to pass,
preserving the existing test count for the bin target.

## Coverage measurement

### Exact command (verbatim, re-runnable)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --no-fail-fast --json -- --test-threads=2 \
  > /tmp/closer-s5-cov.json 2>/tmp/closer-s5-cov.stderr
```

### Results

| Surface | Pre (W4 / T4) | Post (S5) | Δ |
|---------|---------------|-----------|----|
| **Codebase line %** | 81.81% | **82.32%** | +0.51 pp |
| `src/main.rs` | 52.62% (1336/2539) | 48.09% (1005/2090) | denominator −449 lines (extracted), see note |
| `src/cli/io_writer.rs` | NEW | **100.00% (38/38)** | — |
| `src/cli/test_utils.rs` | NEW | **100.00% (48/48)** | — |
| `src/cli/helpers.rs` | NEW | **91.07% (102/112)** | — |
| `src/cli/store.rs` | NEW | **97.12% (303/312)** | — |
| `src/cli/update.rs` | NEW | **89.51% (145/162)** | — |
| `src/cli/io.rs` | NEW | **85.37% (455/533)** | — |

**main.rs note:** the percentage drop is a denominator artefact — the
extracted lines were the heavily-covered cmd_store / cmd_update /
cmd_export / cmd_import / cmd_mine bodies, which now show up under
`cli/*` files. Net codebase coverage is **up** 0.51 pp.

Functions: 2027 / 2467 = 82.16%
Regions: 39941 / 48196 = 82.87%

Lines moved out of main.rs: **608** (4078 → 3470).

## Quality gates

| Gate | Status |
|------|--------|
| `cargo fmt --check` | clean |
| `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` | clean |
| `cargo test --lib -- --test-threads=2` | 757 pass (was 710, +47 new) |
| `cargo test --bin ai-memory` | 11 pass (unchanged) |
| `cargo test --test integration -- --test-threads=2` | 210 pass (unchanged) |

## Files

- `src/cli/mod.rs` (NEW) — module root + re-exports
- `src/cli/io_writer.rs` (NEW) — CliOutput abstraction + 3 unit tests
- `src/cli/test_utils.rs` (NEW, `#[cfg(test)]`-gated) — TestEnv + seed_memory
- `src/cli/helpers.rs` (NEW) — id_short / auto_namespace / human_age + 14 unit tests
- `src/cli/store.rs` (NEW) — cmd_store + StoreArgs + 13 unit tests
- `src/cli/update.rs` (NEW) — cmd_update + UpdateArgs + 6 unit tests
- `src/cli/io.rs` (NEW) — cmd_export/import/mine + ImportArgs/MineArgs + 11 unit tests
- `src/lib.rs` — `pub mod cli;` added
- `src/main.rs` — 5 cmd_* fn defs deleted, 4 args structs deleted, 3 helper fns deleted, dispatch arms updated, imports cleaned
- `Cargo.toml` — `tempfile = "3"` already in `[dev-dependencies]` (no change required)

Production code outside `src/main.rs`, `src/lib.rs`, and the new
`src/cli/` tree is untouched.

## Surprises / deviations

- **`process::exit(1)` retained inline** in `cli::store::run` and
  `cli::update::run` for the governance-deny and not-found branches.
  Restructuring those into Result-returning errors is a behavioural
  change (CLI exit code shifts from 1 to whatever anyhow's main wrapper
  uses). Marked as out-of-scope for W5a — the pre-exit `eprintln!`s
  are now `writeln!(out.stderr, ...)?` and unit-testable; the literal
  `exit(1)` line is the un-testable last 1 line in those branches.
- **Args structs moved alongside handlers** to make the type visible
  across the `lib`/`bin` boundary. Field shapes and clap attributes are
  byte-for-byte identical with the pre-S5 forms; main.rs's `Cli`/`Command`
  clap derives reference them via `use ai_memory::cli::...`.
- **`resolve_content` extraction** in `cli::store` decouples the dash-
  stdin branch from `std::io::stdin()` so `test_store_stdin_content`
  and `test_resolve_content_stdin_dash` can drive it with a fake reader.
  Production behaviour is unchanged — `run_store` calls it with
  `read_stdin_to_string` as the default reader.
- **`import_from_str` extraction** in `cli::io` mirrors the same pattern
  for `cmd_import` so unit tests can supply a literal payload instead
  of redirecting stdin.
- **`source: "test"` rejected by `validate::validate_source`** —
  `seed_memory` and the hand-crafted import payloads in `cli::io`
  tests use `source: "import"` (a member of `VALID_SOURCES`).
- **CliOutput is constructed per-arm in `main.rs`**, not once before
  dispatch. A long-lived `stdout().lock()` would deadlock the
  un-migrated handlers' `println!` macros (they all eventually take
  the same stdout lock). Each migrated arm holds the lock only for the
  body of its handler; the lock is dropped on arm exit.
