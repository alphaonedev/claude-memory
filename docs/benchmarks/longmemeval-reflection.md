# LongMemEval-Reflection benchmark (v0.7.0 L3-1)

Empirical claim: ai-memory's substrate-native recursive-refinement
primitive (`reflect_with_hooks` + the curator reflection pass) lands
ground-truth-aligned reflections at depth 1 and depth 2 across a
diverse synthetic workload.

Issue: [#674](https://github.com/alphaonedev/ai-memory-mcp/issues/674).
Depends on L2-1 (issue #666) — the curator reflection pass surface
the bench exercises.

## Dataset

A deterministic 50-scenario synthetic dataset. Each scenario carries:

- A **topical anchor** drawn from a fixed list of 20 anchors (cloud
  infrastructure, runtime, observability, ops topics).
- **20 observation memories** sharing the anchor's vocabulary. The
  Jaccard overlap between any two same-scenario observations comfortably
  exceeds the curator's `REFLECTION_JACCARD_THRESHOLD = 0.30`.
- A **ground-truth depth-1 reflection** — the canonical pattern string
  the substrate is expected to surface for those 20 observations.
- A **ground-truth depth-2 reflection** — the canonical meta-pattern
  combining two sibling scenarios' depth-1 reflections. Scenario `i`
  (even) pairs with scenario `i+1`; odd-indexed scenarios are the
  trailing member of the previous pair.

### Reproducibility

The entire dataset is regenerated bit-for-bit from a single 64-bit
seed (`L3_LME_REFLECTION_SEED = 0x4C33_4C4D_4552_4546`). The
materialised snapshot lives at:

```
benchmarks/longmemeval_reflection/data/scenarios.jsonl
```

To regenerate after a seed bump:

```bash
cargo bench --bench longmemeval_reflection -- --regenerate
```

To replay an audit against the committed snapshot (rather than the
in-memory generator):

```bash
cargo bench --bench longmemeval_reflection -- --load-snapshot
```

The unit tests under `dataset.rs` pin determinism (`dataset_is_deterministic`)
and round-trip parsing (`jsonl_roundtrip`).

## Runner

For each scenario:

1. Open a fresh in-memory SQLite via `db::open`.
2. Insert the 20 observations.
3. Drive `curator::reflection_pass::run_reflection_pass` with the
   deterministic LLM stub. This is the same surface the
   `ai-memory curator --reflect` CLI hits.
4. Inspect the persisted reflections via `db::list` + `db::get_links`.
5. Score each depth-1 reflection against the ground-truth pattern with
   the configured [`LlmJudge`] implementation.

After every scenario has produced its depth-1 reflection(s), the
runner re-opens the per-scenario DBs in pair order (`i, i+1`), imports
the sibling's depth-1 reflection, and drives `reflect_with_hooks`
directly with the two depth-1 reflections as sources. The substrate
computes `new_depth = max(source.depth) + 1 = 2`. The runner scores
the persisted depth-2 reflection against the ground-truth depth-2
meta-pattern.

## Metrics

| Metric                | Definition                                                                                       | Target  |
|-----------------------|--------------------------------------------------------------------------------------------------|---------|
| `coverage_d1`         | Fraction of scenarios where at least one depth-1 reflection persisted.                           | ≥ 0.80  |
| `accuracy_d1`         | Fraction of persisted depth-1 reflections whose summary token-Jaccard ≥ 0.50 vs ground truth.    | ≥ 0.75  |
| `coverage_d2`         | Fraction of sibling pairs where a depth-2 reflection persisted.                                  | ≥ 0.70  |
| `accuracy_d2`         | Fraction of persisted depth-2 reflections whose summary token-Jaccard ≥ 0.50 vs ground truth.    | ≥ 0.65  |
| `depth_violations`    | Count of reflections whose `reflection_depth` is inconsistent with the substrate cap.            | 0       |
| `sig_verify_rate`     | Fraction of scenarios where every persisted reflection carries non-empty `metadata.agent_id`.    | = 1.0   |
| `throughput_per_min`  | `60 × scenario_count / wall_seconds`.                                                            | ≥ 10    |

Coverage and accuracy are decoupled deliberately: a scenario can clear
the coverage bar (something persisted) without clearing accuracy (the
summary missed the canonical pattern). The reverse is impossible — an
unpersisted scenario scores `0.0` on both.

The Jaccard match threshold (`0.50`) is the per-reflection gate. The
0.75 / 0.65 accuracy targets above are the population gate: of the
reflections that DID persist, ≥75% (depth-1) / ≥65% (depth-2) must
score above the per-reflection threshold.

## LLM stub vs real LLM

The benchmark separates the **runner** (deterministic, in-substrate,
no network) from the **LLM judge** (pluggable via the [`LlmJudge`]
trait).

| Mode              | LLM stub                          | Judge                              | When                          |
|-------------------|-----------------------------------|------------------------------------|-------------------------------|
| **CI**            | `DeterministicLlmStub`            | `DeterministicJudge` (token-Jaccard) | Every PR; runs in CI.         |
| **Publish-time**  | `OllamaClient` configured for Gemma 4 | A Gemma-4-backed judge impl       | Operator-driven before tag-cut. |

The CI stub returns the canonical ground-truth string verbatim for
each scenario. This is intentional: the CI signal is whether the
**substrate** (the curator pass + `reflect_with_hooks`) correctly
clusters, persists, and links. The real LLM at publish time exercises
the **judge** — does Gemma 4 score a real-world summary above the
threshold? The CI gates are necessary; the publish-time gates are
sufficient.

## Output

The bench writes two artefacts under `target/bench/`:

- `longmemeval-reflection.json` — full structured `RunReport`,
  including per-scenario `ScenarioOutcome` rows.
- `longmemeval-reflection.md` — human-readable markdown summary
  (the same string echoed to stdout).

A non-zero exit indicates one or more spec gates failed; the failing
gate(s) are written to stderr prefixed with `GATE FAIL:`.

## Operator playbook

```bash
# CI smoke (≤6 scenarios, ~3s):
cargo bench --bench longmemeval_reflection -- --test

# Full deterministic-stub run (50 scenarios, ~30s):
cargo bench --bench longmemeval_reflection

# Real LLM run (publish-time, requires Ollama + Gemma 4):
AI_MEMORY_BENCH_REAL_LLM=1 cargo bench --bench longmemeval_reflection

# Regenerate the snapshot after a seed bump:
cargo bench --bench longmemeval_reflection -- --regenerate

# Audit-replay against the committed snapshot:
cargo bench --bench longmemeval_reflection -- --load-snapshot
```

The real-LLM path requires wiring the publish-time Ollama judge into
`benches/longmemeval_reflection.rs`'s `main`. The stub-only CI path
is what the gates measure; the real-LLM run is the operator's
publish-time confidence build.

## Why a synthetic dataset

The published LongMemEval dataset (Wu et al., ICLR 2025) measures
**recall** — does the correct source session appear in the top-K
recalled memories? It carries no ground-truth multi-level reflection
patterns, so there's no public corpus to grade against for the
recursive-refinement claim.

L3-1 generalises the L2-1 30-observation acceptance fixture
(`tests/curator/reflection_pass_test.rs`) to 50 independent scenarios
with explicit ground-truth depth-1 and depth-2 reflections. The naming
preserves the "LongMemEval" family-resemblance — same shape, different
metric.
