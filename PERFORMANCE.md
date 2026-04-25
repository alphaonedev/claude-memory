# Performance Budgets

ai-memory publishes an explicit latency contract for every hot-path
operation. **For an MCP server that fires on every conversation, load
time IS the user experience.** Operators must be able to know — without
reading source code — what each tool is allowed to cost and whether the
build still meets that cost.

This document is the authoritative budget table. The CI guard
(`.github/workflows/bench.yml`, Stream F) and the `ai-memory bench`
subcommand (Stream E) will both read from these targets once they land.
Until then, this file establishes the contract; later patches under
v0.6.3 wire the measurement and enforcement.

## Budget Table

| Operation | Target (p95) | Target (p99) | Notes |
|---|---|---|---|
| `memory_session_start` hook | < 100 ms | < 200 ms | Claude Code hook critical path |
| `memory_recall` (hot, depth=1) | < 50 ms | < 150 ms | Felt during agent reasoning |
| `memory_recall` (cold, full hybrid) | < 200 ms | < 500 ms | First-query path |
| `memory_store` (no embedding) | < 20 ms | < 50 ms | Pure write |
| `memory_store` (with embedding) | < 200 ms | < 500 ms | Includes ONNX/Ollama call |
| `memory_search` (FTS5) | < 100 ms | < 250 ms | Keyword baseline |
| `memory_check_duplicate` | < 50 ms | < 150 ms | Pre-write check |
| `memory_kg_query` (depth ≤ 3) | < 100 ms | < 250 ms | New v0.6.3 |
| `memory_kg_query` (depth ≤ 5) | < 250 ms | < 500 ms | New v0.6.3, tail case |
| `memory_kg_timeline` | < 100 ms | < 250 ms | New v0.6.3 |
| `memory_get_taxonomy` (full tree) | < 100 ms | < 250 ms | New v0.6.3 |
| `curator cycle` (1k memories) | < 60 s | < 120 s | Background |
| `federation ack` (W=2 quorum) | < 2 s | < 5 s | Multi-machine |

## CI Guard Threshold

The `bench.yml` workflow (Stream F, scheduled to land later in v0.6.3)
runs `ai-memory bench` on every PR against `release/v0.6.3` and the
trunk branches and **fails the build when any operation's measured p95
exceeds its target by more than 10%.**

p99 targets in the table above are **informational** until the v0.6.3
soak window closes. They are recorded here to make the long-tail goal
explicit and to give operators a number to compare their own
measurements against, but a p99 breach does not fail CI during the
v0.6.3 cycle. Promotion of p99 to a hard gate is tracked as a v0.7
follow-up.

## Hardware Baseline

The targets in the table above are calibrated for:

- **Local dev / reference baseline:** Apple M4, 32 GB unified memory,
  NVMe SSD, Tier-1 thermals (no sustained throttling).
- **CI:** GitHub-hosted Linux x86_64 runners (`ubuntu-latest`),
  comparable single-thread performance to the M4 baseline within the
  10% guard band. macOS and Windows runners are exercised for
  correctness but are not the latency reference.

If you measure on materially slower hardware (older laptops, heavily
contended cloud instances, ARM developer boards) and see numbers above
the targets, that is expected — these are *target* budgets for
reference hardware, not absolute floors for every machine.

## Status

| Component | State | Where |
|---|---|---|
| Published budgets | ✅ landed | this file |
| `ai-memory bench` subcommand | ✅ landed (scaffold) | `src/bench.rs` — covers `memory_store` (no embedding), `memory_search` (FTS5), `memory_recall` (hot, depth=1) |
| Embedding-bound + KG operations in `bench` | 🚧 Stream E follow-up | next iterations of v0.6.3 |
| `bench.yml` CI workflow | 🚧 Stream F | `.github/workflows/bench.yml` (planned) |
| Measured numbers in CI history | ⏳ pending | populated once `bench.yml` lands |

The status table is updated as each Stream lands within the v0.6.3
cycle. When measurements begin, this file will gain a "Latest measured"
column alongside each target.

## Operator Self-Verification

The `ai-memory bench` subcommand seeds an in-memory disposable
SQLite database (the operator's main DB is untouched) and reports
per-operation p50/p95/p99 against the budgets above. Exit code is
non-zero when any p95 exceeds its budget by more than the published
10% tolerance, so the same binary is suitable for use in a CI guard
once `bench.yml` (Stream F) wires it up.

```
$ ai-memory bench
Operation                       Target (p95)   Measured (p95)   p50      p99      Status
─────────────────────────────────────────────────────────────────────────────────────────
memory_store (no embedding)     <   20 ms           0.6 ms         0.4      0.8    PASS
memory_search (FTS5)            <  100 ms           1.9 ms         1.5      2.0    PASS
memory_recall (hot, depth=1)    <   50 ms          16.5 ms        14.4     21.0    PASS
```

`--iterations` and `--warmup` (clamped to `[1, 100_000]` and
`[0, 10_000]` respectively) tune the sample size. `--json` emits the
same numbers as a single JSON document for downstream tooling.

Operations that depend on the embedder (`memory_store` with
embedding, `memory_recall` cold/full hybrid), the KG (`memory_kg_query`,
`memory_kg_timeline`), the curator daemon, and the federation ack path
are not yet wired into `bench` — they each need fixtures or external
services that don't belong on the hot path of a `cargo test` run.
They land in a follow-up Stream E iteration alongside the canonical
1000-memory workload at `benchmarks/v063/canonical_workload.json`.

## Why Publish These at All

Three reasons, in order of importance:

1. **Trust signal.** An MCP server that fires on every conversation
   start cannot afford silent latency. Publishing budgets — even
   before all measurements are live — signals operational maturity
   and gives operators a number to argue with.
2. **Regression guard.** A Rust binary can quietly get slower over
   many releases. Explicit per-operation budgets, gated in CI, make
   regressions visible in the PR that introduces them.
3. **Capacity planning.** Operators choosing where to host
   ai-memory (laptop, VPS, beefy server) need a comparison point.
   "p95 < 100 ms on M4" beats "should be fast enough."

## Forward References

- Stream E (bench tool): `src/bench.rs`, charter §"Stream E —
  Performance Instrumentation"
- Stream F (CI guard): `.github/workflows/bench.yml`, charter
  §"Stream F — Performance Budgets + CI Guard"
- Hardware notes: charter §"Performance Budgets (Authoritative)"
