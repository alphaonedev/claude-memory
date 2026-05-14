# Recipe 05 — autoresearch composition (optional)

**Script:** [`05-autoresearch-composition.sh`](05-autoresearch-composition.sh)

## What this recipe proves

Recipes 01-04 demonstrate four primitives in isolation. This recipe
shows they compose into a single end-to-end **autoresearch loop**:

```
6 synthetic experiments      (depth 0)
        │
        ├── cluster A reflection (depth 1)
        └── cluster B reflection (depth 1)
                  │
                  └─→ meta-reflection (depth 2)
                            │
                            ├─→ SKILL.md promotion (Apache-2.0)
                            └─→ forensic bundle (signed, offline-verifiable)
```

Every artefact is cryptographically traceable to its underlying
experiment observations: the meta-reflection's `reflects_on` edges
point to the two cluster reflections; each cluster reflection's edges
point to its three experiments; the promoted skill carries
`derived_from_reflection_id` in its metadata; and the forensic bundle
ships the entire graph plus signed-event envelopes for offline audit.

## Attribution

The "autoresearch" framing — an agent that runs many small synthetic
experiments and synthesises reusable insight from the results — is
Andrej Karpathy's; the recipe's shape mirrors that frame because it's
the natural compositional shape over the ai-memory primitives, not
because the substrate is Karpathy-specific. The four primitives the
recipe composes (bounded reflection, signed `reflects_on` edges, skill
promotion, forensic bundle) are ai-memory's contributions and ship in
v0.7.0; see the per-issue references below.

This recipe uses **synthetic data** so it runs hermetically without an
LLM dependency. Real autoresearch agents would use this exact substrate
contract — the substrate doesn't care whether the experiment data is
synthetic or real.

## Why it matters

A useful agent today is one that can run cheap experiments at scale
and *retain the lessons*. The substrate primitive that makes retention
audit-grade — rather than vibes — is the closing loop demonstrated
here: synthesised insight (the meta-reflection) crystallises into a
signed, portable, re-registerable Apache-2.0 skill, and the whole
provenance graph is exportable into a single tarball any auditor can
re-verify offline. That's what separates "the agent felt confident" from
"here is the cryptographic evidence for what the agent learned and how
it learned it".

## What it does step by step

1. **Bootstrap.** Fresh sqlite under `.local-runs/cookbook-05-<ts>/memory.db`.
2. **Seed.** Six synthetic experiment observations across two
   hyperparameter clusters (lr=0.001 vs lr=0.0003, three batch sizes each).
3. **Cluster reflect.** Two depth-1 reflections, one summarising each
   cluster, via `memory_reflect` over MCP.
4. **Meta-reflect.** One depth-2 reflection consolidating both
   cluster reflections — the "lesson learned" across the experiment
   family.
5. **Promote.** `memory_skill_promote_from_reflection` over MCP with
   the depth-2 meta-reflection as source; the resulting SKILL.md
   carries `derived_from_reflection_id` and
   `original_reflection_depth=2` in its metadata.
6. **Bundle.** `export-forensic-bundle --memory-id <meta-refl-id>
   --include-reflections` produces a deterministic tarball of every
   memory + edge + signed-event envelope reachable from the
   meta-reflection.
7. **Verify.** `verify-forensic-bundle` against the bundle exits 0 and
   prints "verification OK".
8. **Verdict.** Prints the entire graph in one block; exits 0 only
   when every stage succeeds.

## Acceptance contract

Exits `0` if and only if:

- Six observations stored.
- Two depth-1 cluster reflections minted, both with `reflection_depth=1`.
- One depth-2 meta-reflection minted with `reflection_depth=2`.
- The promotion succeeds and returns a non-empty `skill_id` and
  `digest`, with `original_reflection_depth=2`.
- The forensic bundle is written to disk and is non-empty.
- `verify-forensic-bundle` returns exit 0 AND emits "verification OK".

## Cross-references

- Recipe [`01-bounded-recursive-refinement.md`](01-bounded-recursive-refinement.md)
  — the bounded reflection primitive (Step 3 + 4).
- Recipe [`02-curator-driven-reflection.md`](02-curator-driven-reflection.md)
  — the curator's automation surface over the same primitive (this
  recipe runs the manual variant; the curator pipeline produces the
  same shape of edges).
- Recipe [`03-reflection-to-skill-promote.md`](03-reflection-to-skill-promote.md)
  — the skill promotion + round-trip digest contract (Step 5).
- Recipe [`04-forensic-bundle.md`](04-forensic-bundle.md) — the
  forensic bundle export + verify (Step 6 + 7).
- Issue [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655)
  — substrate-native recursive refinement.
- Issue [#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666)
  — curator-driven reflection pass (the productionisation of Step 3-4).
- Issue [#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670)
  — forensic bundle export + verify.
- Issue [#671](https://github.com/alphaonedev/ai-memory-mcp/issues/671)
  — reflection → skill promotion (Step 5's keystone).

## Troubleshooting

Any failure mode from recipes 01-04 surfaces here too — see those
recipes' troubleshooting sections first. Composition-specific failures
are rare and indicate a regression in how the primitives chain
together; capture the run directory (`COOKBOOK_KEEP_DB=1`) and the MCP
response files under `mcp-*.out.jsonl` when opening an issue.

## Where to take this next

For real autoresearch:

1. Replace the six static observation strings with real experiment
   outputs (training logs, eval metrics, fuzzer findings, etc.). The
   substrate is content-agnostic.
2. Drive the cluster + meta reflections through the curator
   (`ai-memory curator --reflect --namespace <ns>`) with an Ollama
   LLM wired in — the curator picks clusters by co-recall similarity
   rather than the hand-grouped clusters this recipe uses.
3. Promote the meta-reflection only when its `confidence` exceeds an
   operator-chosen threshold (the substrate carries a `confidence` field
   on every memory; the policy lives in your agent, not in the
   substrate).
4. Distribute the forensic bundle alongside the SKILL.md — auditors
   can re-verify the chain offline without your DB.
