---
sidebar_position: 3
title: Recall pipeline
description: How recall is computed, scored, and mutates state.
---

# Recall pipeline

Recall is **multi-stage** and **never read-only** — every recall mutates the database.

## Stage 1 — FTS5 keyword

Fuzzy OR query against the FTS5 virtual table:

```
score = fts.rank
      + priority × 0.5
      + access_count × 0.1
      + confidence × 2.0
      + tier_bonus       (long +3.0, mid +1.0, short 0.0)
      + recency_factor
```

## Stage 2 — Semantic (semantic+ tier)

Cosine similarity via in-memory HNSW index (or linear scan fallback). Threshold > 0.3.

## Stage 3 — Adaptive blending

```
final = semantic_weight × cosine + (1 − semantic_weight) × normalized_fts
```

Where `semantic_weight` interpolates **0.50 → 0.15** as content grows from short (≤500 chars) to long (≥5000 chars). Embeddings degrade on long text.

## Stage 4 — LLM rerank (autonomous tier only)

Cross-encoder (`ms-marco-MiniLM`) reranks the top-N candidates. Uses Ollama for the LLM call.

## Stage 5 — Touch operations (atomic)

For each returned memory:

- `access_count += 1`
- Extend TTL: short `+1h`, mid `+1d`
- Auto-promote mid → long at 5 accesses
- Increment priority every 10 accesses (max 10)

## Hierarchy-aware recall (v0.6.0+)

When `as_agent` is set:

1. Compute ancestor chain: `[agent_ns, parent_ns, grandparent_ns, ...]`
2. Expand candidate pool to memories at any ancestor namespace
3. Apply `proximity_boost = 1.0 / (1.0 + distance × 0.3)` per ancestor distance
4. Visibility filter (per-memory `metadata.scope`) applied last

## Context-budget recall (v0.6.0+)

When `budget_tokens=N` is set:

1. Compute candidate set as above
2. Sort by final score
3. Iterate, accumulating estimated tokens per memory: `(title.len + content.len) / 4`
4. Stop when budget exhausted
5. Return top-K-fitted-in-budget

## Source

`src/db.rs::recall` and `src/db.rs::recall_hybrid`. Reranking in `src/reranker.rs`. HNSW in `src/hnsw.rs`.
