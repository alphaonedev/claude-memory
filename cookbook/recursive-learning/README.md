# cookbook/recursive-learning

Operator-reproducible evidence for the v0.7.0 substrate-native recursive-learning
primitive: bounded `memory_reflect`, curator-driven reflection clustering, and
the closing loop of *reflection → skill → re-registered identical-digest skill*
plus procurement-grade forensic bundles.

These recipes are the runnable companion to
[`docs/RECURSIVE_LEARNING.md`](../../docs/RECURSIVE_LEARNING.md) and to the
v0.7.0 Grand-Slam Layer-1 + Layer-2 task chain (issues
[#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655),
[#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666),
[#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670),
[#671](https://github.com/alphaonedev/ai-memory-mcp/issues/671)).

| # | Recipe | What it demonstrates | Underlying issue |
|---|--------|----------------------|------------------|
| 01 | [`01-bounded-recursive-refinement.sh`](01-bounded-recursive-refinement.sh) ([md](01-bounded-recursive-refinement.md)) | Manual `memory_reflect` at depth 1/2/3, refusal at depth=4, external `verify-reflection-chain` walk-back | #655 / L1-3 |
| 02 | [`02-curator-driven-reflection.sh`](02-curator-driven-reflection.sh) ([md](02-curator-driven-reflection.md)) | `ai-memory curator --reflect --dry-run` over a seeded namespace, inspect + verify each minted reflection | #666 / L2-1 |
| 03 | [`03-reflection-to-skill-promote.sh`](03-reflection-to-skill-promote.sh) ([md](03-reflection-to-skill-promote.md)) | `memory_skill_promote_from_reflection` → `memory_skill_export` → optional `skills-ref validate` → re-register → **identical SHA-256 digest** | #671 / L2-6 |
| 04 | [`04-forensic-bundle.sh`](04-forensic-bundle.sh) ([md](04-forensic-bundle.md)) | `export-forensic-bundle` over a depth-2 chain → `verify-forensic-bundle` passes → tamper one byte → verify refuses | #670 / L2-5 |
| 05 *(optional)* | [`05-autoresearch-composition.sh`](05-autoresearch-composition.sh) ([md](05-autoresearch-composition.md)) | Karpathy-style autoresearch loop: synthetic experiment observations → clustering reflection → skill promote → forensic bundle (end-to-end composition) | composition |

## Prerequisites

- `ai-memory` binary on `PATH`, or `AI_MEMORY_BIN=<path>` exported (the
  v0.7.0 build with `sal` and `sal-postgres` features; sqlite is the
  runtime backend).
- A POSIX shell (`bash 4+`), `jq`, `tar`, `sha256sum` (macOS: `shasum -a 256`).
- ~50 MiB free disk per script run (each carves a timestamped
  subdirectory under `.local-runs/cookbook-NN-<ts>/`).

Optional:

- `skills-ref` Agent Skills validator (a third-party CLI). When present,
  script 03 also runs `skills-ref validate <exported-folder>` on the
  promoted skill and fails the run if the validator rejects. When absent,
  the script logs a SKIP and proceeds. Mirrors the same convention
  `tests/skill_test.rs` follows for L1-5.

## Hard rules

- **No /tmp.** Every script refuses to run if `AI_MEMORY_DEMO_ROOT`
  resolves under `/tmp`, `/var/tmp`, or `/private/tmp`. The project-wide
  HARD RULE in [`CLAUDE.md`](../../CLAUDE.md) overrides any default
  scratch location.
- **Hermetic.** Each script seeds a fresh sqlite DB under its own
  timestamped run directory. Idempotent on re-run (different
  `RUN_DIR`).
- **Self-contained.** Every script can run on a fresh checkout with no
  prior cookbook state.
- **Each run < 10 minutes on a fresh ai-memory.** No script invokes the
  release build itself — the operator is expected to have the binary
  already built. The accompanying gate-pass evidence under
  `audits/v0.7.0-grand-slam/l3-cookbook/` (post-PR) records observed
  wall-clock times on the certification host.

## Running

```bash
# Default scratch root: <repo>/.local-runs/cookbook-NN-<ts>/
./01-bounded-recursive-refinement.sh
./02-curator-driven-reflection.sh
./03-reflection-to-skill-promote.sh
./04-forensic-bundle.sh

# Override scratch root (must NOT be under /tmp /var/tmp /private/tmp)
AI_MEMORY_DEMO_ROOT="$HOME/ai-memory-demo" ./01-bounded-recursive-refinement.sh

# Override binary location
AI_MEMORY_BIN=/opt/ai-memory/bin/ai-memory ./01-bounded-recursive-refinement.sh

# Retain demo DB after a green run (default: clean up)
COOKBOOK_KEEP_DB=1 ./01-bounded-recursive-refinement.sh
```

Each script writes a `run.log` next to the demo DB, exits `0` on
success, exits `>0` on any acceptance failure, and never writes outside
its `RUN_DIR`.

## What this cookbook proves

The v0.7.0 grand-slam claim is that *recursive learning is a substrate
property, not an application property*: an agent that calls
`memory_reflect` cannot escape the depth cap; the curator's
reflection-pass produces signed `reflects_on` edges that an external
verifier can walk; reflections promote into Apache-2.0 Agent Skills
whose digest round-trips byte-for-byte; and any of those chains exports
into a forensic bundle that re-verifies offline. These five scripts are
the operator-reproducible evidence for each link in that chain.

## See also

- [`docs/RECURSIVE_LEARNING.md`](../../docs/RECURSIVE_LEARNING.md) —
  the conceptual primer.
- [`docs/v0.7.0/release-notes.md`](../../docs/v0.7.0/release-notes.md)
  §"Substrate-native recursive refinement".
- [`scripts/reproduce-recursive-learning.sh`](../../scripts/reproduce-recursive-learning.sh) —
  the original depth-cap demo (L1 only).
- [`tests/skill_promote_test.rs`](../../tests/skill_promote_test.rs)
  — the in-tree acceptance suite that pins the round-trip digest match.
- [`audits/v0.7.0-grand-slam/`](../../audits/v0.7.0-grand-slam/) —
  per-layer verdict memories and post-PR gate-pass evidence.
