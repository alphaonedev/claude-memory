# Recipe 03 — reflection-to-skill closing loop (KEYSTONE)

**Script:** [`03-reflection-to-skill-promote.sh`](03-reflection-to-skill-promote.sh)

## What this recipe proves

This recipe is the **keystone** of the v0.7.0 grand-slam recursive-learning
chain. It reproduces the L2-6 closing-loop contract (#671) end-to-end:

> A Reflection memory (synthesised via bounded `memory_reflect`) promotes
> into an Apache-2.0 Agent Skill. The promoted skill exports to a
> SKILL.md folder. Re-registering that folder on a separate, fresh
> database produces a **byte-identical SHA-256 digest** — the same one
> the promotion emitted in step 1.

Identical-digest round-trip is what makes a promoted skill
*portable*: the operator can ship the SKILL.md folder to any ai-memory
deployment, re-register it, and prove cryptographically that what they
re-registered is exactly what came out of the curator-driven reflection
pipeline at the source. Without the round-trip property the promoted
skill is a one-way deliverable; *with* it, the skill becomes a
shareable artefact with deterministic, audit-grade provenance.

## Why it matters

This is where recursive learning stops being an internal substrate
property and starts producing **operator-distributable artefacts**. A
reflection that lives in the source DB is interesting; a SKILL.md folder
whose digest matches across DBs is *transferable knowledge*. Every other
v0.7.0 capability (the depth cap, the signed `reflects_on` edges, the
curator's reflection pass, the forensic bundle of recipe 04) is in
service of producing skills that other operators can trust.

## What it does step by step

1. **Bootstrap DB 1.** Fresh sqlite under `.local-runs/cookbook-03-<ts>/memory.db`.
2. **Seed.** Three depth-0 observations in a per-run namespace.
3. **Reflect.** Drive `memory_reflect` over MCP → one depth-1 reflection
   with three signed `reflects_on` edges.
4. **Promote.** Drive `memory_skill_promote_from_reflection` over MCP
   with the reflection id, an agentskills.io-compliant `skill_name`
   (lowercase + hyphens, no caps), and a `skill_description`. The
   handler returns an envelope with `skill_id`, `digest`,
   `derived_from_reflection_id`, `sources_attached`. The recipe asserts
   `sources_attached == 3` and `derived_from_reflection_id == <refl_id>`.
5. **Export.** Drive `memory_skill_export` over MCP with the freshly
   minted `skill_id` and a target folder under the run directory. The
   handler returns the same digest as the promotion. The recipe
   asserts file existence for `SKILL.md` and three
   `resources/references/source_{0,1,2}.md` resources.
6. **Optional skills-ref validate.** If `skills-ref` is on `PATH`, run
   `skills-ref validate <export-folder>` and fail the run on rejection.
   When absent: log a SKIP. Same convention as `tests/skill_test.rs` L1-5.
7. **Re-register on DB 2.** Bootstrap a *second* fresh sqlite
   (`memory-2.db`) — this isolates the re-registration from the source
   DB so the `(namespace, name)` collision doesn't trigger a
   supersession. Drive `memory_skill_register` over MCP with the
   exported folder.
8. **Assert round-trip.** The re-registered digest must equal the
   original promotion digest, byte-for-byte. This is the keystone
   acceptance contract.
9. **Verdict.** Print all three digests side-by-side; exit 0 only when
   they match.

## Expected output (abridged)

```
==> 4/9  promote reflection → skill via memory_skill_promote_from_reflection
    skill_id=…
    digest (promoted)        = 101c62…ba66
    sources_attached         = 3
    derived_from_reflection  = <refl-id>
==> 5/9  export skill → …/exported-skill
    digest (exported)        = 101c62…ba66
==> 7/9  re-register exported folder on a fresh DB (DB2)
    digest (re-registered)   = 101c62…ba66
==> 8/9  assert promote → export → re-register identical digest
    KEYSTONE: round-trip digest identical (byte-for-byte SHA-256 match) OK
```

## Acceptance contract

Exits `0` if and only if:

- Three observations stored, one depth-1 reflection minted with 3
  `reflects_on` edges.
- `memory_skill_promote_from_reflection` returns a skill envelope with
  `sources_attached == 3` and `derived_from_reflection_id == <refl_id>`.
- `memory_skill_export` writes `SKILL.md` and three
  `resources/references/source_{0,1,2}.md`, AND its returned digest
  matches the promotion digest.
- If `skills-ref` is on `PATH`: `skills-ref validate` exits 0.
- `memory_skill_register` on a separate fresh DB returns a digest
  byte-identical to the promotion digest.

## Cross-references

- Issue [#671](https://github.com/alphaonedev/ai-memory-mcp/issues/671)
  — L2-6 `memory_skill_promote_from_reflection`.
- Issue [#544](https://github.com/alphaonedev/ai-memory-mcp/issues/544)
  / L1-5 — Agent Skills substrate (`memory_skill_register`,
  `memory_skill_export`).
- [`src/mcp/tools/skill_promote.rs`](../../src/mcp/tools/skill_promote.rs)
  — the handler's contract docstring spells out the promotion gate,
  digest-construction routing, and round-trip guarantee.
- [`tests/skill_promote_test.rs`](../../tests/skill_promote_test.rs)
  — the in-tree integration test `round_trip_promote_export_reregister_identical_digest`
  pins the same property this recipe reproduces from the operator's
  perspective.
- Recipe [`01-bounded-recursive-refinement.md`](01-bounded-recursive-refinement.md)
  — the bounded reflection primitive the promotion consumes.

## Troubleshooting

- **"depth-1 reflection failed"** — the MCP profile must include
  `memory_reflect`. Use `--profile full` (default for the script).
- **"sources_attached != 3"** — check that all three `store` calls
  returned a non-empty id; the substrate refuses to attach a missing
  source.
- **"derived_from_reflection_id mismatch"** — the handler routed
  through `register_core` with an unexpected lineage. Inspect
  `mcp-promote.out.jsonl` under the run directory.
- **"ROUND-TRIP FAILED"** — this is the failure the L2-6 keystone
  exists to catch. The most likely cause is a divergence between
  `register_core`'s digest construction (frontmatter JSON + body bytes
  + sorted resource digests) and `memory_skill_export`'s on-disk
  rendering. Compare the in-DB row to the exported folder byte-for-byte;
  open an issue with the SKILL.md diff if a regression has been
  introduced.
- **"skills-ref validate REJECTED"** — the third-party validator
  found an agentskills.io-spec violation in the promoted skill. The
  in-tree handler is the canonical source of truth for the frontmatter
  shape; this typically indicates a regression in the handler that
  needs an in-tree test added before re-attempting.

## Operator-distributable artefacts

When `COOKBOOK_KEEP_DB=1` is set, the recipe leaves the exported folder
at `<run-dir>/exported-skill/`. That folder is a complete, portable
Agent Skill — copy it into any ai-memory deployment's skill registry,
run `ai-memory memory_skill_register --folder-path <folder>`, and you
get back the same SHA-256 digest. That digest is the cryptographic
proof of provenance for the entire promotion lineage.
