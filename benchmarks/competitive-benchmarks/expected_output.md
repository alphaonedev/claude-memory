# Expected Output — Competitive Benchmarks Comparison Table

This file pins the **format** the v0.7.0 launch-day runner targets. It is the contract between `harness.sh` (which writes per-stack CSVs into a results directory) and the launch-day publication step (which renders this table into `alphaonedev.github.io/ai-memory-mcp/competitive-benchmarks/v0.7.0.html`).

The numbers below are **placeholders**. Real numbers land at v0.7.0 launch after `harness.sh` is fully wired.

## Master comparison row (procurement-grade summary)

| Stack       | Version  | R@5     | R@10    | MRR     | Tokens / query (p50) | Latency p95 | Attestation surface |
|-------------|----------|---------|---------|---------|----------------------|-------------|---------------------|
| ai-memory   | v0.7.0   | *TBD*   | *TBD*   | *TBD*   | *TBD*                | *TBD*       | **Yes** (Ed25519, signed audit chain) |
| agentmemory | *pinned* | *TBD*   | *TBD*   | *TBD*   | *TBD*                | *TBD*       | No |
| mem0        | *pinned* | *TBD*   | *TBD*   | *TBD*   | *TBD*                | *TBD*       | No |
| Letta       | *pinned* | *TBD*   | *TBD*   | *TBD*   | *TBD*                | *TBD*       | No (token budgeting only) |

Source corpus: 240-observation slice of LongMemEval (40 obs × 6 question categories). Hardware: Apple M2, 16 GB RAM. Each stack run 5×, median row reported.

## Per-category breakdown (R@5)

| Category                       | ai-memory | agentmemory | mem0   | Letta  |
|--------------------------------|-----------|-------------|--------|--------|
| knowledge-update               | *TBD*     | *TBD*       | *TBD*  | *TBD*  |
| multi-session                  | *TBD*     | *TBD*       | *TBD*  | *TBD*  |
| single-session-assistant       | *TBD*     | *TBD*       | *TBD*  | *TBD*  |
| single-session-preference      | *TBD*     | *TBD*       | *TBD*  | *TBD*  |
| single-session-user            | *TBD*     | *TBD*       | *TBD*  | *TBD*  |
| temporal-reasoning             | *TBD*     | *TBD*       | *TBD*  | *TBD*  |

## Ingest profile

| Stack       | Ingest wall-clock (240 obs) | On-disk footprint | Notes |
|-------------|-----------------------------|-------------------|-------|
| ai-memory   | *TBD*                       | *TBD*             | SQLite WAL, FTS5 |
| agentmemory | *TBD*                       | *TBD*             | Chroma persistent |
| mem0        | *TBD*                       | *TBD*             | Default vector backend |
| Letta       | *TBD*                       | *TBD*             | Server mode, postgres backend |

## CSV schema (per-stack runners emit this)

Each runner writes one CSV at `$OUT_DIR/<stack>.csv` with the following columns:

```
question_id,category,top_k_session_ids,correct_session_id,rank,reciprocal_rank,token_bytes,latency_ms
```

- `question_id` — LongMemEval question id (e.g. `lme_001`).
- `category` — one of the six LongMemEval categories.
- `top_k_session_ids` — comma-separated, ordered by stack's ranking. Length up to 20.
- `correct_session_id` — ground-truth source session for the question (from the LongMemEval gold mapping).
- `rank` — 1-indexed position of `correct_session_id` inside `top_k_session_ids`. Empty string if not present.
- `reciprocal_rank` — `1.0 / rank` if `rank` non-empty, else `0`.
- `token_bytes` — `cl100k_base` token count of the LLM-facing payload the stack returns to its consumer.
- `latency_ms` — wall-clock for the single recall call.

The roll-up step (also part of the launch-day wire-up) reads all four CSVs and emits the master table above.

## Attestation column — note on substance

The "Attestation surface" column is the row procurement teams cite when walking away from a competitor. The substance:

- **ai-memory** ships Ed25519 keypair-per-agent, signed audit-trail entries per write, signed federation transcripts. Audit chain is verifiable offline against the public key of the writing agent. See [`docs/SECURITY.md`](../../SECURITY.md) and [`docs/production-deployment.md`](../../docs/production-deployment.md) §2-3.
- **agentmemory, mem0** do not emit a signed write log. There is no way to prove, after the fact, which agent authored a memory or whether a memory has been mutated since it was written.
- **Letta** ships token budgeting (the R1 parity check from the recovered-commitments audit) but no signed audit chain.

This column does not change between runs; it is a substrate-level property, not a benchmark output.

## See also

- [`README.md`](README.md) — methodology, target competitors, launch-day plan
- [`harness.sh`](harness.sh) — driver script that produces the CSVs feeding this table
- [`../longmemeval/results.md`](../longmemeval/results.md) — single-stack LongMemEval baseline for ai-memory
