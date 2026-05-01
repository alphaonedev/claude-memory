# Performance Budgets

ai-memory publishes an explicit latency contract for every hot-path
operation. **For an MCP server that fires on every conversation, load
time IS the user experience.** Operators must be able to know — without
reading source code — what each tool is allowed to cost and whether the
build still meets that cost.

This document is the authoritative budget table. The CI guard
(`.github/workflows/bench.yml`, Stream F) and the `ai-memory bench`
subcommand (Stream E) both read from these targets — every pull
request against `main`, `develop`, or `release/**` runs the bench
workload on `ubuntu-latest` and fails when any p95 exceeds its
target by more than the published 10% tolerance.

## Budget Table

Rows marked **\*[advisory]\*** are published targets that do not yet have a
corresponding bench in `src/bench.rs` — they are operator-facing performance
contracts pending the Stream E embedder fixture and related follow-ups (see
Status table below). Rows without the marker are exercised by
`ai-memory bench` on every PR via `.github/workflows/bench.yml`.

In the current table: **7 of 14 rows are bench-verified**; the remaining 7
are advisory targets.

| Operation | Target (p95) | Target (p99) | Notes |
|---|---|---|---|
| `memory_session_start` hook | < 100 ms | < 200 ms | *[advisory]* Claude Code hook critical path |
| `memory_recall` (hot, depth=1) | < 50 ms | < 150 ms | Felt during agent reasoning |
| `memory_recall` (cold, full hybrid) | < 200 ms | < 500 ms | *[advisory]* First-query path |
| `memory_recall` (budget, `budget_tokens=4096`) | < 90 ms | < 200 ms | *[advisory]* v0.6.3.1 R1 — autonomous tier budget. Adds cl100k_base BPE tokenization on the survivors only; budget-unset path is unchanged (skips BPE, falls back to a byte heuristic for the `tokens_used` tally). The first call in a process pays a one-shot ~200 ms BPE table parse, amortized away from the steady-state p95. |
| `memory_store` (no embedding) | < 20 ms | < 50 ms | Pure write |
| `memory_store` (with embedding) | < 200 ms | < 500 ms | *[advisory]* Includes ONNX/Ollama call |
| `memory_search` (FTS5) | < 100 ms | < 250 ms | Keyword baseline |
| `memory_check_duplicate` | < 50 ms | < 150 ms | *[advisory]* Pre-write check |
| `memory_kg_query` (depth ≤ 3) | < 100 ms | < 250 ms | New v0.6.3 |
| `memory_kg_query` (depth ≤ 5) | < 250 ms | < 500 ms | New v0.6.3, tail case |
| `memory_kg_timeline` | < 100 ms | < 250 ms | New v0.6.3 |
| `memory_get_taxonomy` (full tree) | < 100 ms | < 250 ms | *[advisory]* New v0.6.3 |
| `curator cycle` (1k memories) | < 60 s | < 120 s | *[advisory]* Background |
| `federation ack` (W=2 quorum) | < 2 s | < 5 s | *[advisory]* Multi-machine |

## CI Guard Threshold

The `bench.yml` workflow (Stream F) runs `ai-memory bench` on every
PR against `main`, `develop`, or `release/**` and on every push to
those branches. It **fails the build when any operation's measured
p95 exceeds its target by more than 10%.** The full table lands in
the workflow run summary; the JSON document is uploaded as a
`bench-results` artifact for downstream tooling.

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
| `ai-memory bench` subcommand | ✅ landed | `src/bench.rs` — covers `memory_store` (no embedding), `memory_search` (FTS5), `memory_recall` (hot, depth=1), `memory_kg_query` (depth=1, depth=3, depth=5), `memory_kg_timeline` |
| Per-tool MCP `tracing` spans | ✅ landed | `src/mcp.rs` `handle_request` — `mcp_tool_call` span carries `tool` + `rpc_id`; `elapsed_ms` emitted at exit |
| KG operations in `bench` | ✅ landed | `src/bench.rs` — fan-out fixture (50 × 4 outbound, every link `valid_from`-stamped) drives `kg_query` depth=1 + `kg_timeline`; chain fixture (50 chains × 5 hops) drives `kg_query` depth=3 + depth=5 |
| Embedding-bound operations in `bench` | 🚧 Stream E follow-up | needs an embedder fixture decision (opt-in flag vs cfg(test) fake vs pre-cached model) — see iter-0017 handoff |
| `bench.yml` CI workflow | ✅ landed | `.github/workflows/bench.yml` — gates every PR and trunk push on `ubuntu-latest`; uploads `bench-results` artifact (JSON + table) |
| Measured numbers in CI history | ✅ collecting | each workflow run's summary carries the table; the JSON artifact is retained per GitHub Actions retention policy |

The status table is updated as each Stream lands within the v0.6.3
cycle. When measurements begin, this file will gain a "Latest measured"
column alongside each target.

## Operator Self-Verification

The `ai-memory bench` subcommand seeds an in-memory disposable
SQLite database (the operator's main DB is untouched) and reports
per-operation p50/p95/p99 against the budgets above. Exit code is
non-zero when any p95 exceeds its budget by more than the published
10% tolerance — the same binary the `bench.yml` CI guard runs on
every pull request.

```
$ ai-memory bench
Operation                       Target (p95)   Measured (p95)   p50      p99      Status
─────────────────────────────────────────────────────────────────────────────────────────
memory_store (no embedding)     <   20 ms           0.4 ms         0.3      0.5    PASS
memory_search (FTS5)            <  100 ms           0.5 ms         0.5      0.5    PASS
memory_recall (hot, depth=1)    <   50 ms           4.8 ms         4.2      5.3    PASS
memory_kg_query (depth=1)       <  100 ms           0.5 ms         0.5      0.5    PASS
memory_kg_query (depth=3)       <  100 ms           0.6 ms         0.6      0.6    PASS
memory_kg_query (depth=5)       <  250 ms           0.7 ms         0.6      1.0    PASS
memory_kg_timeline              <  100 ms           0.1 ms         0.1      0.1    PASS
```

`--iterations` and `--warmup` (clamped to `[1, 100_000]` and
`[0, 10_000]` respectively) tune the sample size. `--json` emits the
same numbers as a single JSON document for downstream tooling.

The KG rows seed two in-process fixtures so every traversal runs
end-to-end with no external service:

- A **fan-out fixture** (50 source memories × 4 outbound links each,
  every link `valid_from`-stamped) drives `memory_kg_query` at depth=1
  and `memory_kg_timeline`.
- A **chain fixture** (50 chains × 5 hops each = 300 memories +
  250 links) drives `memory_kg_query` at depth=3 (the deepest hop in
  the "depth ≤ 3" 100 ms budget bucket) and depth=5 (the tail-case
  "depth ≤ 5" 250 ms bucket). Every chain head reaches three follow-on
  nodes at depth=3 and all five at depth=5, so the recursive CTE is
  exercised at the documented depth ceiling rather than collapsing to
  a single hop.

Embedding-bound paths (`memory_store` with embedding, `memory_recall`
cold/full hybrid), the curator daemon, and the federation ack path are
not yet wired in — they each need fixtures or external services that
don't belong on the hot path of a `cargo test` run. They land in a
follow-up Stream E iteration alongside the canonical 1000-memory
workload at `benchmarks/v063/canonical_workload.json`.

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

## Response Shape Overhead

### v0.6.3.1 — `memory_recall.meta` block (P3)

Every `memory_recall` response now carries a `meta` block reporting which
recall path executed (`hybrid` vs `keyword_only`), which reranker scored
the final ordering (`neural` / `lexical` / `none`), the per-stage
candidate counts (`fts`, `hnsw`), and the average semantic blend weight.
Closes audit gaps G2 / G8 / G11 by making silent-degrade paths visible at
request time.

The block is small — a representative serialization is:

```json
"meta": {
  "recall_mode": "hybrid",
  "reranker_used": "neural",
  "candidate_counts": { "fts": 8, "hnsw": 12 },
  "blend_weight": 0.42
}
```

That's **~110 bytes wire-side** (closer to ~50 bytes after gzip on the
HTTP path). The block is constant-size — it does not grow with the
number of memories returned. Counter accumulation in
`db::recall_hybrid_with_telemetry` adds two `usize` increments per
candidate plus a single `f64` push to a `Vec`, none of which moves the
needle on the `< 50 ms` p95 budget for `memory_recall (hot, depth=1)`.
Local measurements on the M4 reference baseline show no detectable
shift in the recall row of `ai-memory bench`; the published budget
holds with margin.

## Forward References

- Stream E (bench tool): `src/bench.rs`, charter §"Stream E —
  Performance Instrumentation"
- Stream F (CI guard): `.github/workflows/bench.yml`, charter
  §"Stream F — Performance Budgets + CI Guard"
- Hardware notes: charter §"Performance Budgets (Authoritative)"
