# Competitive Benchmarks

**Status:** SCAFFOLDING ONLY. Closes roadmap gap F1-scaffolding (issue #692). The full benchmark run requires installing every competitor in a reproducible environment and is deferred to the **v0.7.0 launch** as a launch-day artifact.

## Why competitive benchmarks

`ai-memory` competes against three meaningfully comparable open-source memory stacks. The procurement question that closes a serious evaluation is *not* "how good is ai-memory in isolation" but "how does ai-memory compare against agentmemory, mem0, and Letta on the same corpus under the same harness."

That comparison is the artifact procurement teams archive. This directory exists to make it reproducible:

- **Same corpus.** Every run uses the same 240-observation slice of LongMemEval (the same corpus the v0.6.4 ship-gate uses; see [`../longmemeval/`](../longmemeval/)).
- **Same harness.** Single shell driver (`harness.sh`) loads the corpus, invokes the under-test memory stack, and emits the comparison row.
- **Same metrics.** R@5, R@10, MRR, token cost per query, latency p95, attestation surface (binary yes/no for the procurement-grade questions).

## Target competitors (v0.7.0 launch run)

| Competitor | Install path | Notes |
|---|---|---|
| `ai-memory` (under test) | `cargo install ai-memory` / `brew install ai-memory` | Reference; v0.7.0 substrate |
| **agentmemory** | `pip install agentmemory` | Python; chroma-backed; no federation, no attestation |
| **mem0** | `pip install mem0ai` | Python; pluggable vector backends; no signed audit chain |
| **Letta** (formerly MemGPT) | `pip install letta` (server mode) | Python; agent runtime + memory; has token budgeting (R1 parity check) |

Selection rationale: these three are the open-source memory layers cited by procurement teams in 2026 alongside `ai-memory`. Closed-source SaaS memory services (Pinecone Assistants, MongoDB Atlas, Astra DB) are out of scope — the comparison must be operator-runnable on the same hardware.

## Methodology

All four stacks run against an identical 240-observation slice of LongMemEval (40 observations × 6 question categories: `knowledge-update`, `multi-session`, `single-session-assistant`, `single-session-preference`, `single-session-user`, `temporal-reasoning`).

For each stack:

1. **Ingest phase.** Load the 240 observations into the stack via its native write API. Record total ingest time and on-disk footprint.
2. **Recall phase.** For each question in the slice's question set, query the stack and capture the top-20 returned memories. Record latency p50 / p95 / p99.
3. **Score.** Compute R@5, R@10, MRR against the LongMemEval ground-truth source-session mapping.
4. **Token cost.** Sum the byte/token cost of the LLM-facing payload (the recall results passed to the downstream LLM, NOT the recall query itself). Token counted via `tiktoken` `cl100k_base`.

Each stack runs five times; the median row goes into the comparison table.

## Metrics emitted

| Metric | Definition | Why procurement cares |
|---|---|---|
| **R@5** | Fraction of questions whose correct source session is in the top 5 recalled | Headline recall quality |
| **R@10** | Same, top 10 | Tolerance for higher-budget consumers |
| **MRR** | Mean reciprocal rank of the correct source session | Quality at the head of the ranking |
| **Token cost / query** | Median LLM-facing token bytes per recall response | Cost-at-scale; tied directly to monthly LLM bill |
| **Latency p95** | 95th-percentile wall-clock time for a single recall | "Will my agent block on memory?" |
| **Attestation surface** | Binary: does the stack emit a signed audit chain per write? | The procurement-grade question competing memory stacks cannot answer "yes" to |

The attestation column is the row procurement teams use to walk away from `agentmemory` / `mem0`. None of them ship a signed chain. `ai-memory` does (Ed25519 per-write since v0.7.0; see [`docs/SECURITY.md`](../../SECURITY.md)).

## Status — what ships in v0.7.0

This commit (v0.7.0 release branch) lands:

- ✅ **`README.md`** — this file. Methodology, target competitors, metrics, expected output schema.
- ✅ **`harness.sh`** — driver script skeleton with `TODO` markers for each competitor's install + invocation surface.
- ✅ **`expected_output.md`** — the comparison-table format the launch-day runner targets, with placeholder rows.

This commit (v0.7.0 release branch) does **not** land:

- ❌ Full installation of the competing stacks (`pip install agentmemory mem0ai letta` would land Python deps; out of scope for a Rust binary's repo and unstable across CI runners).
- ❌ The published numbers themselves (depends on real competitor installs).

**Launch-day plan:** the `harness.sh` driver runs in a dedicated launch-day CI job that provisions a fresh Ubuntu 24.04 container, installs every competitor at a pinned version, runs the methodology above, and publishes the result to `alphaonedev.github.io/ai-memory-mcp/competitive-benchmarks/v0.7.0.html`. The pinned versions of every competitor go into a `requirements-launch.txt` at that time.

## Why deferring the full run is the right call

Three reasons:

1. **Reproducibility.** Each competitor pins a different Python / vector-backend stack; installing all three in the same environment requires careful pin management that is best handled at launch time when the v0.7.0 substrate is also frozen.
2. **Fairness.** Running the comparison against a fast-moving competitor and publishing the result before launch creates an asymmetric advantage if the competitor releases a fix between our run and our publication. Launch-day runs eliminate that window.
3. **Honesty.** Procurement teams trust benchmark rows that the underlying repo lets them re-run. Scaffolding-shipped, full-run-at-launch means anyone reading this README at launch can `cd benchmarks/competitive-benchmarks && ./harness.sh` and reproduce the row.

## See also

- [`../longmemeval/README.md`](../longmemeval/README.md) — single-stack LongMemEval harness (already shipped; the corpus this directory reuses)
- [`expected_output.md`](expected_output.md) — comparison-table format the launch-day runner targets
- [`harness.sh`](harness.sh) — driver script skeleton
- ROADMAP2 §12 — gap F1 line item (issue #692)
- Issue [#692](https://github.com/alphaonedev/ai-memory-mcp/issues/692) — the gap audit this directory closes
