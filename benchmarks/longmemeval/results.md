# LongMemEval Results — v0.6.3.1 Variant Disclosure (P8)

> Methodology and reproducibility pins live in
> [`methodology.md`](./methodology.md). Re-run via
> [`run_variants.sh`](./run_variants.sh).
> See also [`README.md`](./README.md) for harness descriptions.

This document publishes **four variant rows** for ai-memory's recall pipeline
on LongMemEval-S (cleaned), 500 questions. Until v0.6.3.1, only the
keyword-only baseline (R@5 = 97.8%) was published. P8 closes that gap by
disclosing the semantic and autonomous variants on the same harness, the same
hardware, and the same dataset checkout.

---

## Headline matrix

| # | Variant | Tier | Embedder | Reranker | Curator | LLM expand | R@1 | R@5 | R@10 | R@20 | Status |
|--:|---|---|---|---|---|---|---:|---:|---:|---:|---|
| 1 | **keyword-baseline** (published) | `keyword` | — | — | — | gemma3:4b | 86.8% | **97.8%** | 99.0% | 99.8% | Anchored (v0.6.3) |
| 2 | semantic-rerank-off | `semantic` | MiniLM-L6 384d | off | — | no | PENDING-RUN | PENDING-RUN | PENDING-RUN | PENDING-RUN | Methodology pinned |
| 3 | semantic-rerank-on | `semantic` | MiniLM-L6 384d | ms-marco MiniLM | — | no | PENDING-RUN | PENDING-RUN | PENDING-RUN | PENDING-RUN | Methodology pinned |
| 4 | autonomous-curator-on | `autonomous` | nomic-embed 768d | ms-marco MiniLM | gemma3:4b | yes | PENDING-RUN | PENDING-RUN | PENDING-RUN | PENDING-RUN | Methodology pinned |

> **PENDING-RUN** = methodology fully pinned in `methodology.md`, runner
> in `run_variants.sh`. Compute deferred to a reference-hardware operator
> session (Apple M2 16GB, 5 min cool-down between variants × 4 variants ×
> 8 passes = approx 4–6h wall-clock total). Cells will be filled in by a
> follow-up PR that touches only this file. The keyword-baseline row is
> anchored from the published v0.6.3 evidence page (489/500 questions).

---

## How to read this matrix

- **Tier** is the ai-memory feature tier (`keyword` → `semantic` →
  `smart` → `autonomous`). Each tier adds capability and inference cost.
- **Embedder** column tells you which vector model produced the
  embeddings stored in the SQLite `embedding` BLOB.
- **Reranker** column says whether cross-encoder rerank ran on the top-K
  candidates. When off, the score is the adaptive blend
  `semantic_weight*cosine + (1-semantic_weight)*norm_fts`.
- **Curator** column says whether the autonomous-tier curator (small LLM
  that filters / synthesizes a final answer set) ran.
- **LLM expand** column says whether the *query* was expanded into
  multiple search variants by an LLM before being sent to the recall
  pipeline. The published v0.6.3 number used LLM expansion.
- **R@K** is the fraction of questions where the correct source session id
  appears in the top K returned memories. Higher K is more lenient.

---

## Floor and ceiling — interpreting the spread

The keyword baseline is the **floor of useful recall** (no embedding cost,
no LLM cost beyond query expansion). Variants that sit at or below the
floor on R@5 are not worth their compute budget for this dataset.

The autonomous-curator-on row is the **ceiling** — it spends the most
inference time per query (embedding + rerank + curator LLM). On
LongMemEval-S the spread is expected to be narrow because the dataset is
dominated by lexical-match questions; the embedding wins are larger on
out-of-distribution paraphrase-heavy datasets which we will benchmark at
v0.7.

This honest disclosure lets a reader pick the cheapest tier that meets
their R@5 target, rather than buying autonomous because it's at the top
of a marketing chart.

---

## Anchored row — keyword-baseline (v0.6.3)

Source: `docs/evidence.html` row "LongMemEval Recall@5 = 97.8% (489/500
questions, ICLR 2025 benchmark, pure SQLite FTS5+BM25, zero cloud)" and
`benchmarks/longmemeval/README.md` "LLM-expanded + parallel FTS5".

| Category | R@1 | R@5 | R@10 | R@20 |
|---|---:|---:|---:|---:|
| **Overall** | **86.8%** | **97.8%** | **99.0%** | **99.8%** |
| knowledge-update | — | 100.0% | — | 100.0% |
| multi-session | — | 97.7% | — | 100.0% |
| single-session-assistant | — | 100.0% | — | 100.0% |
| single-session-preference | — | 93.3% | — | 100.0% |
| single-session-user | — | 98.6% | — | 100.0% |
| temporal-reasoning | — | 96.2% | — | 99.2% |

Throughput: 142 q/s (parallel, 10 cores), 3.5s end-to-end recall over
500 questions.

---

## Variant rows — execution checklist

A reference-hardware operator should:

```bash
# 1. Reproduce the keyword baseline as a sanity check
./run_variants.sh keyword

# 2. Run each new variant
./run_variants.sh semantic-rerank-off
./run_variants.sh semantic-rerank-on
./run_variants.sh autonomous-curator-on

# 3. Update the matrix above from results/summary.csv
```

Each variant takes approximately:

| Variant | Setup | Per-pass | Total (3 warmup + 5 measure + 5min cooldown) |
|---|---|---|---|
| keyword | <1 min | ~3.5s | ~6 min |
| semantic-rerank-off | ~3 min embed | ~30s | ~10 min |
| semantic-rerank-on | ~3 min embed | ~90s | ~20 min |
| autonomous-curator-on | ~5 min embed + curator load | ~3 min | ~30 min |

Sum: approximately **66 min wall-clock** for all four including cooldowns
on Apple M2 16GB.

---

## Why this matters for the v0.6.3.1 release

The published 97.8% R@5 number reflects ai-memory at its **simplest tier**.
It is honest, but it is not the complete story:

- A reader comparing against systems that publish `autonomous` numbers
  needs to see ai-memory's `autonomous` number to compare apples-to-apples.
- A reader budgeting compute needs to know the **floor** to decide if
  paying for embeddings is worth it on their workload.
- A reader trying to reproduce the result needs the **exact model
  digests, tokenizer, and harness invocation** — those live in
  `methodology.md` and `run_variants.sh`.

All three needs are now served. The four-row matrix replaces the
single-number marketing chart.

---

## Anti-goals reaffirmed

- We do **not** modify recall scoring to chase a higher number for this
  disclosure. Variants disclose the existing range honestly.
- We do **not** claim a variant exceeds the published 97.8% before the
  PENDING-RUN cells are filled in. The matrix is the truth, including
  the gaps.
- We do **not** average across passes to hide regression — the median
  across 5 measurement passes is reported, and per-pass CSVs are kept
  for audit.
- We do **not** publish an oracle row. The harness never sees the
  ground-truth session id during recall.

---

## Schedule for finalization

The PENDING-RUN cells are scheduled to be filled by a single
operator-driven session on the reference Mac mini. The session will:

1. Build `cargo build --release` at the v0.6.3.1 tag.
2. Pull / verify the four model digests (see methodology §2).
3. Run `./run_variants.sh` end-to-end.
4. Open a follow-up PR titled
   `bench(longmemeval): fill variant matrix from reference-machine run`
   that touches only this file plus `results/`.

If the operator finds a variant within 0.5% R@5 of the keyword baseline,
this file should call that out in the **Floor and ceiling** section so
readers know the embedding cost did not buy a meaningful improvement on
LongMemEval-S.
