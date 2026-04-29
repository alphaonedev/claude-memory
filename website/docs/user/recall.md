---
sidebar_position: 4
title: Recall
description: How ai-memory ranks and returns memories.
---

# Recall

Recall blends two signals into a single ranked result list:

1. **FTS5 keyword** (BM25-like) — exact-word matches
2. **Cosine similarity** — semantic distance from your query (semantic tier and above)

Final score blends them adaptively:

```
final = semantic_weight × cosine + (1 − semantic_weight) × normalized_fts
```

Where `semantic_weight` varies **0.50 → 0.15** as content grows from short (≤500 chars) to long (≥5000 chars). Embeddings lose information on long text, so the keyword signal weights more.

## 6-factor scoring

Every recall is ranked by:

1. **FTS relevance** (text match strength)
2. **Priority** × 0.5
3. **Access count** × 0.1
4. **Confidence** × 2.0
5. **Tier bonus** — long +3.0, mid +1.0, short 0.0
6. **Recency decay** — newer wins

## Side effects (recall is never read-only)

Every recall **mutates** the database:

- Increment `access_count`
- Extend TTL: short +1h, mid +1d
- **Auto-promote** mid → long at 5 accesses
- Increment `priority` every 10 accesses (max 10)

This means your most-used memories naturally bubble up over time.

## Hierarchy-aware recall (v0.6.0+)

Pass `--as-agent` to scope recall to your agent's namespace chain:

```bash
ai-memory recall "what we know about X" \
  --as-agent acme/engineering/platform/agent-1
```

This returns memories from `agent-1` itself + `platform` (team) + `engineering` (unit) + `acme` (org), with proximity boost (closer namespaces score higher).

## Context-budget recall (v0.6.0+)

```bash
ai-memory recall "what fits in a 4K context" --budget-tokens 4000
```

Returns the top-ranked memories whose cumulative tokens fit within `N`. **No competitor has this.** LLMs have finite context windows; this matches what they can actually use.
