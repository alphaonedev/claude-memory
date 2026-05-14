# Recipe 02 — curator-driven reflection

**Script:** [`02-curator-driven-reflection.sh`](02-curator-driven-reflection.sh)

## What this recipe proves

The curator's `--reflect` mode (L2-1, issue
[#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666)) wraps
the bounded `memory_reflect` primitive from recipe 01 into a single
operator-friendly CLI verb: `ai-memory curator --reflect --namespace
<ns>`. The pass clusters co-recalled Observations, synthesises typed
Reflection memories with signed `reflects_on` provenance, and produces
a structured JSON report (`reflection_pass.ReflectionPassReport`)
operators can pipe into CI.

Every reflection the pass mints is walkable via the same
`verify-reflection-chain` verifier from L1-3 — there is no separate
audit surface for "curator-minted" vs "agent-minted" reflections. This
is the substrate guarantee: any reflection, regardless of who minted
it, carries the same Ed25519-signed `reflects_on` edges.

## CI / smoke variant

Production curator runs an LLM (autonomous tier) to do the clustering
and synthesis. The cookbook runs hermetically with
`AI_MEMORY_NO_CONFIG=1` exported, which means no LLM is wired in. The
recipe therefore demonstrates two things side-by-side:

1. **`curator --reflect --dry-run --namespace … --json`** runs cleanly,
   emits a structured `ReflectionPassReport`, and surfaces the no-LLM
   diagnostic in `report.errors[0]` ("no LLM client configured — set a
   feature tier that provides an llm_model"). Exit code 0 — this is an
   operator-actionable surface, not a crash.

2. **`memory_reflect`-driven minting** — the recipe then mints two real
   depth-1 reflections (one per namespace, one per cluster) via the MCP
   `memory_reflect` verb. These reflections carry the exact same signed
   `reflects_on` edges the curator would emit with an LLM available —
   the curator is a pipeline over the primitive, not a different
   primitive.

3. **Inspect + verify each** — `ai-memory get <refl-id>` shows
   `memory_kind=reflection` and `reflection_depth=1`;
   `ai-memory verify-reflection-chain --format json <refl-id>` reports
   `chain_depth=1`, `edges_failed=0`, `edges_verified=3` (one signed edge
   per source observation).

Operators running an autonomous-tier configuration with an Ollama
endpoint can re-run the recipe with `AI_MEMORY_NO_CONFIG` unset and a
configured LLM — the `dry_run_proposals` / `reflections_persisted`
fields in the report then populate with real curator output.

## Why it matters

The L1 primitive (recipe 01) proves the substrate enforces the cap.
This recipe proves the substrate's reflect machinery composes into the
**autonomy layer**: an unattended curator can mint reflections during
normal operation, and every one of those reflections is auditable by
the *same* verifier an operator would point at an agent-minted
reflection. There is no curator-specific trust boundary.

## What it does step by step

1. **Bootstrap.** Fresh sqlite DB under `.local-runs/cookbook-02-<ts>/`.
2. **Seed.** Six depth-0 observations across two namespaces (3 each),
   simulating two distinct clusters.
3. **Curator dry-run.** Runs `curator --reflect --dry-run --namespace
   <NS_A> --json` and asserts the JSON report contains either the
   no-LLM diagnostic (smoke / CI variant) or a populated
   `dry_run_proposals` array (production variant). Both outcomes count
   as a clean run.
4. **Mint reflections.** Drives `memory_reflect` over MCP twice, once
   per namespace, producing two depth-1 reflections with signed
   `reflects_on` edges to their three sources each.
5. **Inspect.** `ai-memory get <refl-id>` for each, asserting
   `reflection_depth == 1` and `memory_kind == "reflection"`.
6. **Verify.** `ai-memory verify-reflection-chain --format json
   <refl-id>` for each, asserting `chain_depth == 1`, `edges_failed ==
   0`, `edges_verified >= 1`.
7. **Verdict.** Prints a verdict block and exits 0 only when every
   assertion passes.

## Acceptance contract

Exits `0` if and only if:

- Six observations stored cleanly across two namespaces.
- `curator --reflect --dry-run --json` exits 0 and emits a structured
  report containing either `"no LLM client configured"` or
  `"dry_run_proposals"`.
- Two depth-1 reflections mint successfully via MCP.
- For each reflection, `get` reports `reflection_depth=1` and
  `memory_kind=reflection`.
- For each reflection, `verify-reflection-chain --format json` reports
  `chain_depth=1`, `edges_failed=0`, `edges_verified>=1`.

## Cross-references

- Issue [#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666)
  — L2-1 curator `--reflect` mode.
- Issue [#667](https://github.com/alphaonedev/ai-memory-mcp/issues/667)
  — L1-3 `verify-reflection-chain` external verifier.
- [`src/cli/curator.rs`](../../src/cli/curator.rs) — `run_reflect`
  entry point; the dry-run-without-LLM path that this recipe exercises.
- [`src/curator/reflection_pass.rs`](../../src/curator/reflection_pass.rs)
  — the pipeline that runs when an LLM is configured.
- Recipe [`01-bounded-recursive-refinement.md`](01-bounded-recursive-refinement.md)
  — the L1 primitive this recipe wraps.

## Troubleshooting

- **"curator report missing expected fields"** — the curator binary is
  older than v0.7.0 L2-1 (issue #666). Rebuild from
  `feat/v0.7.0-grand-slam` at or after `c359e89`.
- **Curator dry-run *does* mint proposals** — that's expected when the
  operator has an Ollama LLM wired through their config. The recipe
  succeeds in both modes.
- **Verify reports edges_verified=0** — the `memory_reflect` MCP call
  failed silently, leaving an unsigned reflection. Inspect
  `mcp-reflect-N.out.jsonl` under the run directory for the raw MCP
  response.
