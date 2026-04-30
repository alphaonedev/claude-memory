# Recall

`memory_recall` (MCP) / `POST /api/v1/recall` (HTTP) / `ai-memory recall`
(CLI) returns the highest-ranked memories matching a query context.
Recall is multi-stage and **never read-only** — every successful recall
mutates the database (touch, TTL extension, auto-promotion).

## Pipeline

1. **FTS5 keyword search** — fuzzy OR query over the bundled SQLite
   `memories_fts` virtual table; scored by
   `fts.rank + priority*0.5 + access_count*0.1 + confidence*2.0 + tier_bonus + recency_factor`.
2. **Semantic search** — cosine similarity via the in-memory HNSW index
   (or a linear scan fallback when no index is loaded). Cosine gate is
   `> 0.2`; relaxed from 0.3 in v0.6.2 Patch 2 after scenario-18 caught
   real-world misses at 0.25–0.29 cosine.
3. **Adaptive blend** —
   `final = semantic_weight * cosine + (1 - semantic_weight) * norm_fts`.
   `semantic_weight` lerps 0.50 (≤500 chars) → 0.15 (≥5000 chars):
   embeddings lose information on long text; FTS stays precise.
4. **Cross-encoder rerank** (autonomous tier only) — bi-encoder
   candidates run through a BERT cross-encoder for fine-grained pair
   scoring.
5. **Hierarchy proximity boost** — when `namespace` is hierarchical
   (contains `/`), recall broadens to the ancestor chain and applies a
   small score bonus to memories nearer the queried leaf.
6. **Token-budget greedy fill** (Phase P6 / R1) — see below.
7. **Touch** — increment `access_count`, extend TTL, auto-promote
   mid → long at 5 accesses, increment priority every 10. Touch runs
   only on memories that survive every prior stage, including the
   budget cut.

---

## `budget_tokens` — Context-Budget-Aware Recall (R1)

> **"Give me the most relevant memories that fit in 4 K tokens."**

Recall accepts an optional `budget_tokens: u32` parameter. When set,
recall returns the highest-ranked memories whose cumulative content
tokens fit under the budget, using the deterministic OpenAI
`cl100k_base` BPE tokenizer (`tiktoken-rs` 0.7) — the same tokenizer
Claude / GPT use for context-window accounting, so the budget the
caller sets matches the budget the LLM enforces.

This was the prior phased ROADMAP's "killer feature" ("no competitor
has this") that Letta has and we did not. v0.6.3.1 R1 recovers it.

### Algorithm

After scoring, reranking, and proximity boost, recall iterates the
ranked list in order and greedily fills until the next memory would
exceed the budget:

```text
let mut total = 0u32;
let mut out = vec![];
for memory in ranked.iter() {
    let tokens = cl100k_base(memory.content).count();
    if total + tokens > budget && !out.is_empty() { break; }
    out.push(memory.clone());
    total += tokens;
}
```

The R1 contract has two non-obvious properties:

- **Always return at least one** — if the highest-ranked memory alone
  exceeds the budget, it is returned anyway with
  `meta.budget_overflow = true`. A successful recall with at least one
  matching memory never returns an empty result, even under a
  pathologically tight budget.
- **`budget_tokens=0` returns zero memories** with
  `meta.budget_overflow = false`. The caller explicitly asked for
  nothing; this is the documented escape hatch for "I want only the
  meta block."

### Response shape

```jsonc
{
  "memories": [ /* … */ ],
  "count": 3,
  "mode": "hybrid",
  "tokens_used": 1284,        // legacy top-level — unchanged from v0.6.3
  "budget_tokens": 4096,      // legacy top-level — unchanged from v0.6.3
  "meta": {
    "budget_tokens_used": 1284,
    "budget_tokens_remaining": 2812,
    "memories_dropped": 5,
    "budget_overflow": false
  }
}
```

The legacy top-level `tokens_used` and `budget_tokens` fields are
preserved verbatim; pre-P6 callers continue to work byte-for-byte.
The `meta` block is always present when a budget was supplied.

### Tokenizer choice

`cl100k_base` is OpenAI's standard BPE for GPT-4 / GPT-3.5-turbo and
the de facto context-window tokenizer for Claude as well. The BPE
table is bundled in `tiktoken-rs` (~1.7 MB), so the count is
**offline-deterministic** across all hosts — there is no network call
or model download.

### Performance

When `budget_tokens` is **unset**, recall does not run cl100k_base at
all — `tokens_used` falls back to a fast `content.len() / 4` byte
heuristic to preserve the v0.6.3 "observe the cost without enforcing
it" contract. This keeps the bench harness's `recall_hot` p95 budget
(< 50 ms) intact.

When `budget_tokens` is **set**, cl100k_base runs on the post-rank
candidate set (typically ≤ 50 memories). The first call in a process
pays a one-shot ~200 ms BPE table parse; subsequent calls reuse a
process-wide `OnceLock<CoreBPE>` and run in single-digit ms even on
large candidate sets.

The autonomous-tier budget for `memory_recall (budget,
budget_tokens=4096)` is **< 90 ms p95** — see PERFORMANCE.md for the
full table.

### Scoring is not affected

Token-budget filtering is a **post-rank** filter. The blend, decay,
proximity boost, and cross-encoder rerank all run before the budget
fill, so the surviving subset is the highest-ranked subset. Two
recalls of the same query with different budgets produce a strict
prefix-of-prefix relationship: dropping the budget can only shrink
the result set, never reorder it.

### Examples

CLI:

```bash
ai-memory recall "what's our refund policy" --budget-tokens 4096
```

MCP:

```jsonc
{
  "method": "tools/call",
  "params": {
    "name": "memory_recall",
    "arguments": {
      "context": "what's our refund policy",
      "budget_tokens": 4096,
      "format": "json"
    }
  }
}
```

HTTP:

```bash
curl -sS -X POST http://localhost:9077/api/v1/recall \
  -H 'content-type: application/json' \
  -d '{"context":"what's our refund policy","budget_tokens":4096}'
```
