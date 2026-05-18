# Cookbook — atomisation (WT-1)

v0.7.0 WT-1-G atomisation substrate primitive. Three end-to-end
reproducible recipes covering the substrate decomposition flow,
recall-time visibility flip, and forensic-bundle audit chain.

Each recipe is hermetic — every run mints a fresh sqlite DB under
`.local-runs/cookbook-atomisation-*-<ts>/` (project HARD RULE: no
`/tmp` writes; see `CLAUDE.md`), uses the deterministic stub curator
shipped with `examples/atomise_roundtrip.rs`, and prints a pass/fail
verdict block.

| Recipe | Surface exercised | Acceptance |
|---|---|---|
| [`01-basic-flow.sh`](01-basic-flow.sh) | Substrate engine: store → atomise → archive | Six WT-1 invariants (atom_count ≥ 2, parent.atomised_into bumped, atom_of children, recall skip, include_archived flip, forensic chain included) |
| [`02-cli-atomise-recall-flow.sh`](02-cli-atomise-recall-flow.sh) | Recall-time visibility flip end-to-end | Default recall hides archived parent; `include_archived=true` re-surfaces it |
| [`03-forensic-bundle-walk.sh`](03-forensic-bundle-walk.sh) | Audit chain + forensic bundle | `derives_from` lineage rows match atom count; bundle includes parent + atom envelopes for offline replay |

## Prerequisites

- A clean Rust toolchain (`cargo --version` returns 1.84+).
- The cargo cache is warm (recipes invoke `cargo run --features
  sal,sal-postgres --example atomise_roundtrip`); the first run
  triggers a build pass and may take several minutes.
- No external dependencies. The recipes do NOT require a running
  Ollama daemon or any other LLM backend — the stub curator inside
  `examples/atomise_roundtrip.rs` is deterministic.

## Running

```bash
cookbook/atomisation/01-basic-flow.sh
cookbook/atomisation/02-cli-atomise-recall-flow.sh
cookbook/atomisation/03-forensic-bundle-walk.sh
```

Set `COOKBOOK_KEEP_DB=1` to retain the per-recipe DB + JSON report
for inspection. Set `AI_MEMORY_DEMO_ROOT=<path>` to override the
project-local `.local-runs/` scratch root (the override is
refused for any `/tmp`-class tmpfs path per the project HARD RULE).

## Related

- Long-form atomisation primer: [`docs/atomisation.md`](../../docs/atomisation.md)
- Substrate engine code: [`src/atomisation/`](../../src/atomisation/)
- Live-Ollama integration test:
  `examples/atomise_roundtrip.rs::live_gemma_e2b_smoke`
- Tracking issue: [#736 WT-1-G capabilities-v3 + cookbook + docs](https://github.com/alphaonedev/ai-memory-mcp/issues/736)
