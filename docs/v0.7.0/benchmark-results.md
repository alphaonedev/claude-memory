# v0.7.0 Benchmark Results — Phase G

SHIP CAMPAIGN Phase G — procurement-grade performance evidence for ai-memory at v0.7.0.

- HEAD: `dfa4847` on `feat/v0.7.0-grand-slam`
- Host: Mac Mini, Apple Silicon
- Date: 2026-05-14
- Release binary: `target/release/ai-memory`
- Method: audit-honest — fresh SQLite per tier, real subprocess invocations, `time.perf_counter_ns` around each call; no mocks, no shortcuts. Backing JSON: `.local-runs/phase-g/results.json`.

## Summary verdict

**GREEN.** Every tier hits R@5 = R@10 = R@20 = 1.0 on the canonical recall workload. All LongMemEval-Reflection (L3-1, #674) gates pass with throughput 296× the target. Every cost-test metric is below the operator-relevant single-digit-to-double-digit-millisecond budget except autonomous recall (sub-second), which is on-target for the MLX-backed Ollama path that includes cross-encoder rerank.

## G.1 — LongMemEval-Reflection (L3-1, issue #674)

Reflection benchmark over 6 scenarios (3 with depth-2 siblings). Source: `benches/longmemeval_reflection.rs`.

| Metric              |     Value | Target  | Status |
| ------------------- | --------: | ------: | :----: |
| coverage_d1         |     1.000 |  ≥ 0.80 |  PASS  |
| accuracy_d1         |     1.000 |  ≥ 0.75 |  PASS  |
| coverage_d2         |     1.000 |  ≥ 0.70 |  PASS  |
| accuracy_d2         |     1.000 |  ≥ 0.65 |  PASS  |
| depth_violations    |         0 |     = 0 |  PASS  |
| sig_verify_rate     |     1.000 |   = 1.0 |  PASS  |
| throughput (scen/m) | 2 966.78  |    ≥ 10 |  PASS  |
| wall_seconds        |     0.121 |       – |   –    |

## G.2 — Recall accuracy + latency per tier

For each tier: fresh DB, seed 200 representative memories (10 rotating topics × 20 fixtures each), run 100 recall queries with `--limit 20`, time the full subprocess round-trip. Topics are identifiable by title substring so R@k can be measured deterministically.

| Tier         |  R@5 | R@10 | R@20 |  p50 (ms) |  p95 (ms) |  p99 (ms) | mean (ms) |
| ------------ | ---: | ---: | ---: | --------: | --------: | --------: | --------: |
| `keyword`    | 1.00 | 1.00 | 1.00 |       9.7 |      14.0 |      14.8 |      10.7 |
| `semantic`   | 1.00 | 1.00 | 1.00 |     222.0 |     321.3 |     365.6 |     287.1 |
| `smart`      | 1.00 | 1.00 | 1.00 |     280.4 |     689.0 |     962.3 |     380.7 |
| `autonomous` | 1.00 | 1.00 | 1.00 |     632.7 |     945.9 |   1 177.8 |     675.8 |

Notes:

- `semantic`, `smart`, and `autonomous` use the local Ollama embedder (`nomic-embed-text-v1.5`, 768-dim) on the Apple Silicon GPU via MLX.
- `autonomous` additionally invokes the `cross-encoder/ms-marco-MiniLM-L-6-v2` reranker — the dominant cost above 500 ms.
- All four tiers achieve perfect recall on this fixture set because the topic markers are present both in titles (keyword signal) and content (semantic signal). The latency separation is the procurement-relevant number.
- semantic's `max_ms = 6746` is the first call (embedder lazy-load); steady-state p95 is the reliable figure.

## G.3 — Cost tests (millisecond metrics)

Single-process latency budgets that operators tune around. All measured against the release binary, fresh DBs in `.local-runs/phase-g/`.

| Cost test                              | Value (ms) |
| -------------------------------------- | ---------: |
| `ai-memory boot` cold-start (fresh DB) |       17.96 |
| `ai-memory boot --quiet` warm p50      |       11.85 |
| First-store (fresh DB)                 |       18.05 |
| First-recall (after one store)         |       39.90 |
| signed_events append + store p50       |       13.08 |
| signed_events append + store p95       |       13.86 |
| signed_events append + store p99       |       16.15 |
| signed_events append + store mean      |       13.26 |
| Federation fan-out W=2 of N=4 p50      |       40.20 |
| Federation fan-out W=2 of N=4 p95      |       67.84 |
| MCP tool dispatch (tools/list) p50     |       26.92 |
| MCP tool dispatch min                  |       26.08 |

Setup details:

- **Boot**: includes manifest emit, DB open, migration check. Cold-start path creates a new SQLite file; warm path re-opens.
- **signed_events**: 100 sequential stores after a single seed; each store walks the V-4 prev_hash + sequence pipeline. p50 ≈ 13 ms means the per-store hash-chain cost is bounded by the round-trip overhead, not the cryptographic work.
- **Federation fanout**: 10 POSTs against live test-cell alice (`https://127.0.0.1:9077`) over mTLS. Alice is configured with `--quorum-writes 2` and peers `[bob, charlie, dave]` (W=2 of N=4). The 40 ms p50 is the full end-to-end including TLS handshake, ed25519 sign, broadcast, and quorum ack.
- **MCP dispatch**: single `tools/list` JSON-RPC over stdio in a one-shot subprocess. Includes binary cold-start, so the bound on the per-tool steady-state is tighter than the number shown.

## Reproduction

```bash
# G.1
cd /Users/fate/v07/grand-slam
export TMPDIR=/Users/fate/v07/v07-fixes/.local-runs/tmp
export CARGO_TARGET_DIR=/Users/fate/v07/v07-fixes/.cargo-shared-target
AI_MEMORY_NO_CONFIG=1 cargo bench --bench longmemeval_reflection -- --test

# G.2 — recall per tier
python3 /Users/fate/v07/v07-fixes/.local-runs/phase-g/bench_tiers.py

# G.3 — cost tests
python3 /Users/fate/v07/v07-fixes/.local-runs/phase-g/cost_tests.py
```

Raw artefacts:

- `.local-runs/phase-g/longmemeval.json`
- `.local-runs/phase-g/recall_per_tier.json`
- `.local-runs/phase-g/cost_tests.json`
- `.local-runs/phase-g/results.json` (consolidated)
- `.local-runs/phase-g/bench_tiers.log`
- `.local-runs/phase-g/cost_tests.log`

## Verdict block

```
Phase G Benchmarks — HEAD dfa4847
─────────────────────────────────────
G.1 LongMemEval-Reflection:
  coverage_d1:  1.000 (target >= 0.80)  PASS
  accuracy_d1:  1.000 (target >= 0.75)  PASS
  coverage_d2:  1.000 (target >= 0.70)  PASS
  accuracy_d2:  1.000 (target >= 0.65)  PASS
  depth_violations: 0                   PASS
  sig_verify_rate:  1.000               PASS
  throughput:    2966.78 scen/min       PASS

G.2 Recall per tier:
  keyword:    R@5=1.00 R@10=1.00 R@20=1.00 p50=9.7ms   p95=14.0ms  p99=14.8ms
  semantic:   R@5=1.00 R@10=1.00 R@20=1.00 p50=222.0ms p95=321.3ms p99=365.6ms
  smart:      R@5=1.00 R@10=1.00 R@20=1.00 p50=280.4ms p95=689.0ms p99=962.3ms
  autonomous: R@5=1.00 R@10=1.00 R@20=1.00 p50=632.7ms p95=945.9ms p99=1177.8ms

G.3 Cost tests:
  Boot cold-start:        17.96 ms
  Boot warm p50:          11.85 ms
  First-recall:           39.90 ms
  First-store:            18.05 ms
  signed_events append:   p50 13.08 / p95 13.86 / p99 16.15
  Federation fanout W2N4: p50 40.20 / p95 67.84
  MCP dispatch:           p50 26.92 / min 26.08

VERDICT: GREEN
```
