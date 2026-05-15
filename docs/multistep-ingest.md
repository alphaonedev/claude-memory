# Multi-step ingest (Form 3)

*v0.7.0 — issue [#756](https://github.com/alphaonedev/ai-memory-mcp/issues/756)*

The Batman framework audit's Form 3 covers the discipline of breaking ingest
into a deterministic-where-possible plus LLM-where-necessary pipeline, with
prompt-cache reuse across stages and an explicit-trust contract that tells the
LLM to lean on pre-computed helper output rather than re-deriving it. Before
this ship, every LLM call in `ai-memory` was single-shot; the substrate had
no surface for multi-step ingest. Form 3 closes that gap.

## Batman exemplars

Two reference patterns shaped this substrate:

- **Understand-Anything (six-of-nine)**: phase one runs a deterministic
  helper script that emits structured JSON; phase two calls the LLM with a
  prompt that ends with `Do NOT re-run discovery commands or re-count
  lines, trust the script's results entirely`. The model does not re-do
  the deterministic work; it consumes the JSON and produces the
  synthesis output.

- **OpenKB four-step pipeline**: stages are `load_context → classify →
  enrich → emit`, and every LLM stage shares a *system prompt prefix*
  so the prompt-cache key stays stable across stages within a single
  run. The cache hit rate is the deciding factor for whether a
  multi-stage pipeline is operationally affordable.

The Form 3 module reproduces both patterns as named pipeline variants:
`two_phase` (Understand-Anything) and `four_step` (OpenKB).

## Pipeline shape

A `Pipeline` is an ordered list of `Stage`s. Each stage is either a
deterministic helper or an LLM call:

```rust
enum Stage {
    Helper { kind: HelperKind, params: HelperParams },
    LlmCall {
        prompt_template: String,
        trust_inputs: Vec<HelperOutputRef>,
        output_schema: serde_json::Value,
        label: String,
    },
}
```

The executor (`crate::multistep_ingest::executor::IngestExecutor`) walks the
descriptor in order:

1. Every `Helper` stage runs first. Helpers are pure functions of their
   `HelperParams`, so the executor can parallelise them (the current
   implementation runs them serially for deterministic traces; the path
   is wired for `rayon` if a future ship needs it).
2. Each `LlmCall` stage prepends the *shared prefix* (the variant tag +
   the pipeline's `system_prompt`) to the stage-specific prompt body.
   Trust slots are rendered verbatim under the explicit-trust banner.
3. The shared prefix is identical across LLM stages in a single run, so
   the prompt-cache key derived from it (SHA-256 of the prefix bytes)
   stays stable.

## Helpers

Three deterministic helpers ship at v0.7.0:

| Kind                | Output                                                  |
| ------------------- | ------------------------------------------------------- |
| `jaccard_overlap`   | Top-N candidate IDs ranked by token-set overlap.        |
| `cosine_pre_filter` | Candidate set above a cosine threshold (default 0.20).  |
| `fts_classifier`    | Coarse fact-kind tag (`procedural`/`declarative`/`episodic`). |

Helper outputs are `serde_json::Value` payloads that thread directly
into the LLM prompt's trust slot. The MCP tool surface and the executor
both treat them as authoritative — the explicit-trust contract is the
substrate's promise to the LLM.

## Two-phase variant walkthrough

`two_phase_default()` returns a `Pipeline` with three stages:

1. `Helper(FtsClassifier)` — labels the incoming content as
   procedural / declarative / episodic.
2. `Helper(JaccardOverlap)` — scores existing candidates against the
   incoming content.
3. `LlmCall(synthesise)` — produces `{title, summary, tags, atoms}`.
   Both helper outputs land in trust slots.

This is the recommended entry point for callers that want one LLM
round-trip with strong deterministic preconditions.

## Four-step variant walkthrough

`four_step_default()` returns a `Pipeline` with five stages:

1. `Helper(FtsClassifier)` (`load_context` part 1).
2. `Helper(JaccardOverlap)` (`load_context` part 2).
3. `LlmCall(classify)` — emits `{fact_kind, confidence}`. Trust slot
   carries the FTS classifier output.
4. `LlmCall(enrich)` — emits `{entities, claims, relations}`. Trust
   slot carries the Jaccard overlap output.
5. `LlmCall(emit)` — emits the final memory envelope.

All three LLM stages share the same shared-prefix, so they hit the
same prompt-cache key. The acceptance test
`prompt_cache_key_consistent_across_stages_within_a_run` pins this
invariant.

## Prompt-cache reuse mechanics

The shared-prefix bytes are SHA-256 hashed into a `CacheKey`. The
executor records each LLM stage's cache key into a
`PromptCacheTelemetry` recorder; the resulting trace exposes both the
distinct-key set (length 1 on a healthy run) and a single
`prompt_cache_consistent` boolean.

Two consecutive runs of the same variant on the same machine will
produce the same cache key because the prefix bytes are identical.
Runs of different variants produce different cache keys because the
variant tag is part of the prefix. The MCP tool's response surfaces
both fields so operators can verify cache reuse without parsing
logs.

## Operator interfaces

### MCP tool

`memory_ingest_multistep` (Family::Power, smart+ tier) accepts:

| Argument            | Type     | Default      | Notes                                          |
| ------------------- | -------- | ------------ | ---------------------------------------------- |
| `content`           | string   | (required)   | Content to ingest.                             |
| `namespace`         | string   | `"global"`   | Routing hint for the FTS classifier.           |
| `pipeline_variant`  | string   | `"two_phase"`| One of `two_phase` / `four_step`.              |
| `pipeline_override` | object   | (none)       | Full `Pipeline` JSON; overrides `pipeline_variant`. |

The response carries the stage-by-stage trace, the distinct cache-key
set, the `prompt_cache_consistent` boolean, and the final structured
output emitted by the last LLM stage. `ingested_memory_ids` is
reserved for the follow-up wave that wires a substrate writer behind
the emit stage; the initial Form 3 closeout returns the structured
trace for callers to route themselves.

The keyword tier short-circuits with the standard tier-locked
advisory envelope (`{tier-locked, current_tier, required_tier}`)
before any pipeline execution.

### Cookbook

`cookbook/multistep-ingest/01-two-phase.sh` drives the example binary
`examples/multistep_ingest_roundtrip.rs` against both default
variants with a `MockLlmDispatch`. The recipe asserts that each run's
report carries `prompt_cache_consistent: true` and exactly one
distinct cache key. No Ollama dependency.

### Namespace policy

Form 3 does not introduce a namespace-policy surface of its own. The
caller's `namespace` argument flows through to the FTS classifier's
`HelperParams.namespace` field for routing hints only. Namespace
policy enforcement (auto-atomise, scoped recall, etc.) remains the
job of the existing substrate pre-store/post-store hooks; Form 3
sits upstream of memory persistence, so policy fires on the
downstream write side when the caller routes the synthesis output
through `memory_store` or `memory_atomise`.

## Audit honesty

- The LLM dispatch trait (`LlmDispatch`) is implemented by
  `OllamaDispatch` (production) and `MockLlmDispatch` (tests +
  cookbook). The production binding wraps the project's existing
  `OllamaClient::generate`; no new circuit-breaker / timeout
  discipline is added at the Form 3 layer.
- The substrate's pipeline-cache telemetry observes cache *keys*, not
  Ollama's server-side cache hits. The invariant `cache_key
  stable across stages within a run` is necessary but not sufficient
  for actual server-side reuse; it is the substrate's contribution to
  the cache-friendliness contract.
- The cookbook recipe stubs the LLM. The acceptance tests stub the
  LLM. The live-LLM exercise (Gemma 4 via Ollama) is operator
  dogfooding on the `OllamaDispatch` path; no automated test
  exercises a real model.

## See also

- `src/multistep_ingest/` — module source.
- `tests/form_3_multistep_ingest.rs` — acceptance suite.
- `cookbook/multistep-ingest/01-two-phase.sh` — operator demo.
- `examples/multistep_ingest_roundtrip.rs` — driver used by the cookbook.
