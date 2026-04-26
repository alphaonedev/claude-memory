# v0.6.3 Canonical Workload

This directory holds the canonical workload fixture consumed by the
v0.6.3 curator-cycle bench described in
[`PERFORMANCE.md`](../../PERFORMANCE.md). It is the seed required by the
`< 60 s p95` budget row for `curator cycle (1k memories)`.

## Files

| File | Purpose |
|---|---|
| `gen_canonical_workload.py` | Deterministic generator. Re-running with no arguments produces a byte-identical `canonical_workload.json`. |
| `canonical_workload.json` | The committed 1000-memory seed. ~390 KB. Schema version 1. |

The generator is committed alongside its output so the fixture is
reproducible from source. Bumping the seed or vocabulary requires
re-running the script and committing both files together.

## Schema

`canonical_workload.json` is a single JSON object:

```jsonc
{
  "schema_version": 1,
  "description": "...",
  "seed": 20260426,
  "count": 1000,
  "memories": [
    {
      "tier": "mid" | "long",
      "namespace": "projects/alpha/decisions",
      "title": "decisions #0000",
      "content": "...",       // always >= curator MIN_CONTENT_LEN (50 chars)
      "tags": ["..."],
      "priority": 3..8,
      "confidence": 0.6..1.0,
      "source": "import",
      "metadata": {}           // empty so curator.needs_curation() returns true
    },
    ...
  ]
}
```

The per-memory shape lines up 1:1 with `crate::models::CreateMemory`,
so the bench harness can `serde_json::from_str` the array directly into
the upsert path with no field translation.

## Curator-eligibility invariants

The fixture is constructed so that **every** entry passes
`crate::curator::needs_curation`:

- `namespace` never starts with `_` (curator's internal-namespace skip).
- `content.len() >= 50` (`MIN_CONTENT_LEN`). The actual minimum produced
  by the generator is ~74 chars; a guard pads short combinations to
  preserve the floor across vocabulary edits.
- `metadata.auto_tags` is unset (the empty `{}` body), so the
  already-tagged short-circuit never fires.
- `tier` ∈ {`mid`, `long`} only — curator scans neither short tier nor
  internal tiers.

A single curator sweep against this fixture therefore finds 1000
candidates and runs `auto_tag` + `detect_contradiction` on the first
`max_ops_per_cycle` (default 100) until the cap is hit.

## Reproducibility

```bash
cd benchmarks/v063
python3 gen_canonical_workload.py
# wrote benchmarks/v063/canonical_workload.json (1000 memories)
git diff --quiet canonical_workload.json || echo "fixture drifted"
```

The generator uses `random.Random(SEED)` with `SEED = 20260426` and
`json.dumps(..., indent=2, sort_keys=True)` so the output is stable
across Python 3.x patch releases. CI that wants to assert reproducibility
can re-run the generator and `diff -q` against the committed file.

## Why this lives in `benchmarks/`, not `tests/fixtures/`

`tests/fixtures/` carries small inputs scoped to one or two unit tests.
This fixture is a published artifact in its own right: the curator-cycle
benchmark documented in `PERFORMANCE.md`, plus any future external
benchmarking tool, want a stable on-disk path. `benchmarks/v063/` is the
natural home and matches the `benchmarks/longmemeval/` layout already in
the repo.
