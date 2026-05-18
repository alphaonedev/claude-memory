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

> **See also:** `docs/performance.html` publishes a complementary,
> per-feature-tier view (keyword / semantic / autonomous) of these
> budgets — equal-or-tighter targets stratified by which capabilities
> are loaded. Both surfaces are kept in agreement; this file is the
> canonical aggregate contract that the `bench.yml` CI guard reads.

## Autonomous-Tier Latency Tax — Batman-Active Write Path

> **v0.7.0 Gap #4 (issue #805) attack plan.** Cross-refs #654 (distilled
> hot-path model, TABLED). This section closes the operator-facing gap
> by publishing measured budgets + a concrete remediation queue.

In **Batman-active mode** every `memory_store` runs through:

- **Form 1** — online dedup-and-synthesis LLM call (one prompt; up to 5
  candidates).
- **Form 2** — synchronous atomise-before-embed.
- **Form 6** — `regex_then_llm` kind classification (one prompt).

All three are blocking on the write path. Until #654's distilled
300M hot-path model lands, these are the **measured** budgets against
`gemma4:e4b` on the Apple M4 reference baseline:

| Form | Stage | p50 warm | p95 warm | p99 cold | Knob to bypass |
|------|-------|----------|----------|----------|----------------|
| Form 1 | synthesis batch | 0.5 s | 3 s   | 30 s | `autonomous_hooks=false` (per-namespace) |
| Form 2 | atomise sync    | 0.4 s | 2.5 s | 25 s | `auto_atomise_mode = "deferred"` |
| Form 6 | kind classify   | 0.2 s | 1.5 s | 15 s | `auto_classify_kind = "regex_only"` |
| **End-to-end `memory_store`** | (sum) | **~1.1 s** | **~7 s** | **~70 s** | All three |

The p99 cold ceiling is the load-bearing number — a thinking-mode
gemma cold start blocks an entire 70 s on the worst case. The same
write without Batman-active mode is < 50 ms.

### Operator knobs (interim, while #654 TABLED)

Three documented operator escape hatches let a Batman-active deployment
trade latency for capability without re-compiling:

1. `auto_classify_kind = "regex_only"` (per-namespace `GovernancePolicy`)
   — removes Form 6 entirely. Recovers ~1.5 s p95 / 15 s p99 cold.
2. `auto_atomise_mode = "deferred"` — Form 2 runs in a background
   worker. Recovers ~2.5 s p95 / 25 s p99 cold. The atomise-result
   row appears via the curator sweep within 60 s.
3. `AI_MEMORY_AUTO_CONFIDENCE=0` — disables Form 5 calibration on the
   write path. Recovers ~100 ms p95 (small; Form 5 is the cheapest of
   the four).

A namespace that sets all three knobs falls back to the keyword-tier
write budget (< 50 ms p95).

### v0.7.0 attack plan — measured contributors

The **worst single contributor** measured on `scripts/batman-bench.sh`
is Form 1 synthesis cold start (LLM round-trip + JSON-extract).
Ranked by p99 contribution:

| Rank | Contributor                       | p99 cold | v0.7.1 attack |
|------|-----------------------------------|----------|---------------|
| 1    | LLM cold start (model load)       | ~25 s    | model-keep-alive warmup hook in curator |
| 2    | gemma thinking-mode generation    | ~12 s    | thinking-mode opt-out per Form (Form 1 doesn't need it) |
| 3    | Form 1 JSON re-extract loop       | ~0.8 s   | switch to strict-JSON Ollama mode (already supported); we currently re-extract on the failure path |
| 4    | Form 2 atom de-dup pass           | ~0.6 s   | bench-verified; in scope for v0.7.1 PERF-17 |
| 5    | Form 6 regex pre-pass             | ~0.05 s  | already optimal |

### v0.7.1 work queue

- **PERF-17** — Form 1 strict-JSON Ollama mode (eliminates re-extract
  loop on ~30% of responses).
- **PERF-18** — curator-keep-alive hook (`ollama pull --keep-alive`)
  warms the model behind the write path so a fresh `memory_store`
  never pays the cold-start cost.
- **PERF-19** — per-Form thinking-mode opt-out config knob (Form 1
  doesn't need extended reasoning; Form 3 and Form 5 do).

These three changes target the top-3 contributors and are estimated
at ~150 LOC total. They land in v0.7.1 if #654 stays TABLED past the
v0.7.0 ship date.

### Bench harness

`scripts/batman-bench.sh` produces the JSON measurement table; the
shape is suitable for ingestion by the bench-results artifact already
attached to `bench.yml`. The script is reproducible (operator runs
it locally, on the dogfood node, or in CI nightly).

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

## v0.7 — Apache AGE backend (KG queries)

v0.7.0 introduces an optional **Apache AGE** (Cypher-on-Postgres) backend
for `memory_kg_query` and `memory_find_paths`, selectable at runtime via
`KgBackend::Age`. The default `KgBackend::Cte` (recursive SQLite CTE)
remains unchanged and is the supported single-binary path; AGE is opt-in
for deployments that already run Postgres and benefit from native
graph-traversal acceleration.

The table below records the **v0.7.0 target budgets** for both backends
on the canonical 1000-memory fixture (`benchmarks/v063/canonical_workload.json`).
These are aspirational p95 budgets, not measured numbers — they define
the contract the J8 CI gate enforces against the AGE-vs-CTE bench.

| depth | CTE p95 | AGE p95 | speedup |
|---|---|---|---|
| 1 | 8 ms   | 6 ms  | 1.3x |
| 3 | 35 ms  | 18 ms | 1.9x |
| 5 | 120 ms | 70 ms | 1.7x |

> The **J8 CI gate** (see `.github/workflows/bench.yml`, AGE job)
> enforces **≥ 30% AGE-over-CTE speedup at depth=5** on every PR that
> touches the KG path. If AGE ever fails to clear that bar, the AGE
> backend is dropped per the v0.7 epic exit criteria — the complexity
> only earns its keep when it pays for itself.

**Workload:** 1k canonical memories, fan-out fixture (50 × 4) for
depth=1, chain fixture (50 × 5 hops) for depth=3 / depth=5. Same
fixtures the SQLite-CTE bench rows above use, so the two backends are
measured against an identical traversal shape.

**Reproduce locally:**

```bash
# CTE (default, no extra services)
cargo bench --bench kg_bench

# AGE (requires Postgres + AGE extension on PG_DSN)
cargo bench --bench kg_bench --features=age
```

The AGE bench skips cleanly when the `age` feature is not enabled or
when no `PG_DSN` is exported, so the default CI matrix is unaffected.

Design rationale, dual-path test strategy, and the rollback criterion
live in [`docs/v0.7/rfc-attested-cortex.md`](docs/v0.7/rfc-attested-cortex.md)
and the v0.7 epic Track J entries.

### When to enable AGE

Stay on `KgBackend::Cte` for:

- Single-binary / SQLite-only deployments (the supported default).
- KG depth ≤ 2 workloads, where the recursive CTE is already well
  inside its budget and the AGE round-trip overhead dominates.
- Graphs under ~10 k nodes — CTE comfortably handles these with no
  Postgres dependency.

Consider switching to `KgBackend::Age` when **both** apply:

- Typical `memory_kg_query` depth is **≥ 3** (chain-following workloads,
  multi-hop provenance, `memory_find_paths` over wide graphs).
- The graph has grown past **~10 k nodes** (or ~50 k links), where the
  recursive CTE starts paying for the lack of native graph indexes.

Operators already running Postgres for federation or attestation
audit chains pay near-zero marginal cost to enable AGE; pure-SQLite
operators should not adopt it just to chase the speedup.

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
