# v0.7.0 test configuration

> **Status:** living doc. Operator-stamped configuration for future
> test runs against the v0.7.0 substrate.

## xAI model id for cross-LLM AI NHI evaluation

**Canonical model id:** `grok-4.3`

Operator note 2026-05-15. Replaces `grok-4.20-0309-reasoning` (used for
the Phase E AI NHI cross-LLM verdict at the v0.7.0 ship campaign,
2026-05-14, doc `docs/v0.7.0/ai-nhi-verdict-claude-vs-grok.md`).

Applies to:

- Re-runs of the LongMemEval benchmark (`docs/benchmarks/longmemeval-reflection.md`)
- PersonaMem benchmark engagement (companion doc; targets v0.7.0+)
- Any future cross-LLM AI NHI evaluation against the substrate
- AI NHI workflow brief construction: any agent dispatching xAI API
  calls should use `grok-4.3` in the model field

API endpoint and key conventions are unchanged
(`https://api.x.ai/v1/chat/completions`, `XAI_API_KEY` env). Only the
`"model"` field in the request body changes.

## `reasoning_effort` parameter

`grok-4.3` is a reasoning model and supports a `reasoning_effort`
parameter controlling how much thinking the model does before
responding. Operator-stamped guidance:

| Effort | Behavior | Use when |
|---|---|---|
| `"none"` | Disables reasoning entirely; no thinking tokens used | Simple use cases needing near-instant response |
| `"low"` (default) | Some reasoning tokens, still fast | General agentic use, tool calling |
| `"medium"` | More thinking, less latency-sensitive | Complex data analysis, long-context reasoning |
| `"high"` | Deep thinking | Very challenging problems, complex math, multi-step logic, competition-level tasks |

For ai-memory's cross-LLM AI NHI evaluation (15-scenario substrate
evaluation pattern from Phase E), **`"medium"` is the recommended
default** — scenarios require the model to reason about substrate
behaviour over the course of an evaluation; `"low"` undershoots on the
harder cases (S5 recursive reflection, S6 evidence-packet integrity)
while `"high"` adds latency without a verdict-quality improvement at
the scenario shapes ai-memory exercises.

### Incompatible parameters

When using `grok-4.3`, the following request parameters **return an
error** (do not include them):

- `presence_penalty`
- `frequency_penalty`
- `stop`

If the upstream xAI SDK or any wrapper sets these by default, override
them to `None` / strip them before dispatch.

### Summarized reasoning content

`grok-4.3` exposes summarized reasoning via `chunk.reasoning_content`
when streaming. For audit-honest evaluation reports (per the
audit-honest discipline of Phase E), the reasoning summary SHOULD be
captured alongside the final response — operators and procurement
reviewers reading the verdict report benefit from seeing the
model's stated reasoning path, not just its conclusion.

Sample stream pattern (Python xAI SDK):

```python
chat = client.chat.create(model="grok-4.3", reasoning_effort="medium", messages=[...])
for response, chunk in chat.stream():
    if chunk.reasoning_content:
        print(chunk.reasoning_content, end="", flush=True)
    if chunk.content:
        print(chunk.content, end="", flush=True)
```

## Multi-agent variant: `grok-4.20-multi-agent`

There's also a multi-agent variant. The `reasoning.effort` parameter
on `grok-4.20-multi-agent` controls **agent count**, NOT reasoning
depth:

| `reasoning.effort` | Agent count |
|---|---|
| `"low"` / `"medium"` | 4 |
| `"high"` / `"xhigh"` | 16 |

ai-memory cross-LLM evaluation does not use the multi-agent variant
by default. Single-agent `grok-4.3` at `medium` reasoning is the
canonical pattern for v0.7.0+ test runs.

## Cost discipline

Reasoning tokens are billed as part of total consumption. For
multi-agent variants, ALL tokens from both the leader agent and
sub-agents bill — 16 agents (`"high"` / `"xhigh"`) uses significantly
more tokens than 4 agents.

For ai-memory's standard 15-scenario evaluation:
- `grok-4.3` `"low"` effort: ~5-10× baseline tokens
- `grok-4.3` `"medium"` effort: ~15-25× baseline tokens
- `grok-4.3` `"high"` effort: ~40-80× baseline tokens
- `grok-4.20-multi-agent` `"high"` (16 agents): ~150-300× baseline

These are rough multipliers — operator should size budget at
~$3-5 USD per cross-LLM 15-scenario run on `grok-4.3` `"medium"` at
v0.7.0 release pricing.

## Future test runs that will use grok-4.3 @ medium

- v0.7.0.1 post-tag verification (if any)
- v0.8.0 ship campaign cross-LLM verdict
- Benchmark engagements (WideSearch, SWE-bench, AA-LCR, PersonaMem)
  per the companion benchmark doc

## Why this matters

Phase E AI NHI evaluation derived its convergent-favorable verdict
from Claude Opus 4.7 + Grok 4.20-0309-reasoning. Re-running the same
scenarios on a newer model is meaningful only if both LLMs are
identified by their canonical ids AND the reasoning depth is held
constant. This note pins both so the agent dispatching scenarios
doesn't inadvertently hit a stale SKU or use the wrong reasoning
budget.
