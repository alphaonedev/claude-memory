# Recipe 01 — bounded recursive refinement

**Script:** [`01-bounded-recursive-refinement.sh`](01-bounded-recursive-refinement.sh)

## What this recipe proves

Recursive refinement is a **substrate property, not an application property**.

An agent that drives `memory_reflect` through the MCP server cannot escape
the per-namespace `max_reflection_depth` cap by re-calling the verb on its
own output — the substrate refuses with `REFLECTION_DEPTH_EXCEEDED` at
cap + 1. After the chain is built, the external CLI verb
`ai-memory verify-reflection-chain` walks the `reflects_on` edges
backward to depth 0, re-verifies every Ed25519 signature, and emits a
structured chain-integrity report (`chain_depth`, `edges_verified`,
`edges_failed`). The verifier exits non-zero on any tampering.

This is the L1 link in the v0.7.0 recursive-learning chain — every other
recipe in this cookbook builds on top of the bounded `memory_reflect`
primitive demonstrated here.

## Why it matters

Without a substrate-enforced depth cap, an agent that calls
"reflect-on-my-reflection" in a tight loop can run away — burning LLM
budget, expanding the `reflects_on` graph without bound, or worse,
laundering a bad observation into a polished-looking depth-N
"insight" that no auditor can untangle. The cap is the kill-switch.
The external verifier is the procurement-grade auditable surface that
proves the kill-switch was actually applied to a specific chain.

## What it does step by step

1. **Bootstrap.** Carves a fresh sqlite DB under
   `.local-runs/cookbook-01-<timestamp>/memory.db`. Idempotent: every
   run uses a fresh subdirectory; nothing is written outside it.
2. **Seed.** Stores three depth-0 observations in a per-run namespace
   (`cookbook/recursive-learning-01-<timestamp>`).
3. **Reflect to depth 1, 2, 3.** Drives the MCP server over stdio
   JSON-RPC to call `memory_reflect` on each prior layer. Asserts the
   returned `reflection_depth` field at each level.
4. **Refuse at depth 4.** Attempts a fourth-level reflection. The
   substrate refuses with `REFLECTION_DEPTH_EXCEEDED` (the default cap
   is 3); the script greps the MCP response for the token and fails the
   run if it's missing.
5. **Verify the chain.** Calls `ai-memory verify-reflection-chain
   --format json <depth-3-id>` and asserts
   `chain_depth == 3`, `edges_failed == 0`, `edges_verified >= 1`.
6. **Verdict.** Prints a single-screen verdict block and exits 0 only
   when every assertion passes.

## Expected output (abridged)

```
==> 1/6  bootstrap demo DB
    fresh sqlite DB initialised OK
==> 2/6  store 3 source observations (depth=0)
    stored src-1 → id=… OK
    stored src-2 → id=… OK
    stored src-3 → id=… OK
==> 3/6  reflect over 3 sources → depth=1
    depth-1 reflection minted → id=… (depth=1) OK
==> 4/6  reflect on depth-1 → depth=2
    depth-2 reflection minted → id=… (depth=2) OK
==> 5/6  reflect on depth-2 → depth=3 (at the default cap)
    depth-3 reflection minted → id=… (depth=3) OK
==>     attempt depth=4 (substrate MUST refuse with REFLECTION_DEPTH_EXCEEDED)
    depth-4 refused with REFLECTION_DEPTH_EXCEEDED OK
==> 6/6  verify-reflection-chain over the depth-3 chain
    report → …/verify-reflection-chain.json (chain_depth=3 edges_verified=5 edges_failed=0 n_memories=6)
    chain integrity verified OK
==> verdict
…
    Recipe 01 — bounded recursive refinement reproduced end-to-end. OK
```

## Acceptance contract

The script exits `0` if and only if **all** of the following hold:

- Three depth-0 observations stored cleanly.
- `memory_reflect` succeeds at depth 1, 2, and 3 and the response body
  includes `reflection_depth: <expected>`.
- `memory_reflect` at depth 4 is refused with the literal token
  `REFLECTION_DEPTH_EXCEEDED` in the MCP response.
- `verify-reflection-chain` over the depth-3 reflection reports
  `chain_depth=3`, `edges_failed=0`, and at least one verified edge.

Any other outcome exits non-zero with a clearly-formatted error line on
stderr.

## Cross-references

- [`docs/RECURSIVE_LEARNING.md`](../../docs/RECURSIVE_LEARNING.md) —
  conceptual primer.
- [`scripts/reproduce-recursive-learning.sh`](../../scripts/reproduce-recursive-learning.sh) —
  the original Tasks 1-4 reproduction (this recipe is its cookbook-shaped
  evolution, plus the L1-3 verifier step).
- Issue [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655)
  — substrate cap + `memory_reflect` MCP verb (Tasks 1-4).
- Issue [#667](https://github.com/alphaonedev/ai-memory-mcp/issues/667)
  — L1-3 `verify-reflection-chain` external verifier.
- Issue [#675](https://github.com/alphaonedev/ai-memory-mcp/issues/675)
  — L3-2 cookbook ticket (this recipe).

## Troubleshooting

- **"ai-memory binary not found"** — install the v0.7.0 release or
  build with `cargo build --release --features sal,sal-postgres` and
  point `AI_MEMORY_BIN` at the resulting `target/release/ai-memory`.
- **"depth-N reflection failed"** — the MCP profile must include
  `memory_reflect`. The script uses `--profile full`; if your build is
  feature-gated differently, set `--profile recursive-learning` (the
  minimum profile that ships the verb).
- **"depth-4 was NOT refused"** — the substrate's per-namespace
  `max_reflection_depth` policy has been overridden upward in the
  loaded config. Run with `AI_MEMORY_NO_CONFIG=1` (the script already
  exports this) or check
  `ai-memory --db <db> --json get <std-id>` for a `governance.max_reflection_depth`
  override on the namespace standard memory.
- **"chain integrity check failed"** — either the keypair changed mid-
  run or the sqlite file is corrupted. The script's freshly-carved DB
  rules out both; if you see this on a re-run, examine
  `verify-reflection-chain.json` in the run directory.
