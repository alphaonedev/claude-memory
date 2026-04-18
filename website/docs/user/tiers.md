---
sidebar_position: 3
title: Tiers
description: Memory tiers (short/mid/long) + feature tiers (keyword/semantic/smart/autonomous).
---

# Tiers

Two different things are both called "tiers" in ai-memory. This page explains both.

## 1. Memory tiers — how long a memory lives

| Tier | Default TTL | Use case | Analogy |
|---|---|---|---|
| **short** | 6 hours | throwaway context like current debugging state | sticky note on the monitor |
| **mid** | 7 days | working knowledge like sprint goals and recent decisions | whiteboard |
| **long** | permanent | facts and decisions you'll still need next year | filing cabinet |

TTLs are configurable via `[ttl]` in `config.toml`. A mid-tier memory recalled 5+ times **auto-promotes** to long.

## 2. Feature tiers — how smart recall is

| Tier | Recall method | Extra features | Requirements |
|---|---|---|---|
| **keyword** | FTS5 only | none | none — works on any computer |
| **semantic** *(default)* | FTS5 + cosine similarity (hybrid) | MiniLM-L6-v2 embeddings, HNSW index, 26 tools | ~256 MB RAM |
| **smart** | hybrid + LLM query expansion | + nomic-embed-text + Gemma 4 E2B + auto-tag + contradiction detect | Ollama + ~1 GB RAM |
| **autonomous** | hybrid + LLM + cross-encoder reranking | + Gemma 4 E4B + ms-marco-MiniLM + memory reflection | Ollama + ~4 GB RAM |

Set the tier via `--tier`:

```bash
ai-memory mcp --tier semantic       # default
ai-memory mcp --tier smart          # adds Gemma 4 E2B
ai-memory mcp --tier autonomous     # adds Gemma 4 E4B + reranker
```

### Gemma 4 models in the upper tiers

| Feature tier | Model | Disk size | Speed |
|---|---|---|---|
| smart | `gemma4:e2b` | ~7.2 GB | ~46 tok/s |
| autonomous | `gemma4:e4b` | ~9.6 GB | ~26 tok/s |

Both are served through **Ollama** — install Ollama, then `ollama pull gemma4:e2b` (or `e4b`) and you're done.

### Decoupling tier and model

Feature tier and model choice are independent. Run autonomous-tier features with the smaller, faster model:

```toml
# ~/.config/ai-memory/config.toml
tier = "autonomous"
llm_model = "gemma4:e2b"
```

## Benchmarks

Evaluated on [LongMemEval-S](https://github.com/xiaowu0162/LongMemEval) (500 questions, 6 categories):

| Tier | Recall@5 | Speed | Dependencies |
|---|---|---|---|
| keyword | 97.0% | 232 q/s | none |
| semantic | 97.4% | 45 q/s | embedding model (~100MB) |
| smart | **97.8%** | 12 q/s | Ollama + Gemma 4 E2B |

All inference runs locally — zero cloud API calls, zero per-query cost.

## Picking the right tier

- **Start at semantic** (the default) — works everywhere, no extras
- **Move to smart** when you want auto-tagging, query expansion, contradiction detection
- **Move to autonomous** when you want an AI second-guessing its own recall quality
- The same database works across all 4 — change at any time
