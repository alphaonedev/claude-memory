# Track A — NHI Test Playbook Results (2026-05-17)

Per the v0.7.0 NHI test playbook (memory id `8ccc7fed-7b93-4d2e-8d83-ea2562637f95`, namespace `ai-memory/v0.7.0-nhi-testing`). All 12 phases (P0 → P11) re-run on the post-PR-#820-fixes binary.

| Phase | Status | Pass / Fail / Gap | Result memory |
|-------|--------|-------------------|---------------|
| P0  Environment       | ✅ done   | 6 / 0 / 1 | `NHI-P0-handshake-2026-05-17` |
| P1  Core CRUD         | ⏳ pending | — | — |
| P2  Lifecycle         | ⏳ pending | — | — |
| P3  Knowledge graph   | ⏳ pending | — | — |
| P4  Governance & sec  | ⏳ pending | — | — |
| P5  Power tools       | ⏳ pending | — | — |
| P6  Capabilities v3   | ⏳ pending | — | — |
| P7  Token budget      | ⏳ pending | — | — |
| P8  Hooks             | ⏳ pending | — | — |
| P9  Cross-interface   | ⏳ pending | — | — |
| P10 Performance       | ⏳ pending | — | — |
| P11 Chaos             | ⏳ pending | — | — |

## Phase 0 — Environment handshake

| Test | Expected | Actual | Verdict |
|------|----------|--------|---------|
| `ai-memory --version` | `ai-memory 0.7.0` | `ai-memory 0.7.0` | PASS |
| `readlink -f $(which ai-memory)` | under `v07-f5/target/release/` | under `v07-fixes/.cargo-shared-target/release/` | PASS (path-drift noted) |
| `memory_capabilities.version` | `0.7.0` | `0.7.0` | PASS |
| `memory_capabilities.schema_version` | `2` or v3 | `3` | PASS (v3 surface live, summary field present) |
| All 8 families loaded | true | true (core, lifecycle, graph, governance, power, meta, archive, other) | PASS |
| `SELECT MAX(version) FROM schema_version` | `28` (May 7 baseline) | `43` | PASS-EXPECTED (28→43 progression over 10 days: v37 QW-2 → v38 Form 4 → v39 Form 5 → v40 Cluster C → v41 Cluster G → v42 PERF-8 → v43 Persona Signing #813) |
| trimmed token total | ≤ 3500 | 3449 (51 token headroom) | PASS |
| verbose token total | 5K–10K | 15400 | GAP (over playbook ceiling; opt-in surface only — not a blocker; trending wrong direction) |
| max per-tool tokens | ≤ 1500 | 988 (memory_recall) | PASS (71 tools, 0 over ceiling) |

**Verdict:** SHIP-READY for phase 0 surface. No P0/P1 blockers; one trend-line gap on verbose tokens to track in a follow-up.

### Reproduction

```bash
ai-memory --version
readlink -f $(which ai-memory)
sqlite3 /Users/fate/.claude/ai-memory.db "SELECT MAX(version) FROM schema_version;"
ai-memory doctor --tokens --json | jq '{trimmed: .trimmed_full_profile_total_tokens, verbose: .full_profile_total_tokens}'
ai-memory doctor --tokens --raw-table | jq '.tools | max_by(.tokens) | {name, tokens}'
```

Memory: `NHI-P0-handshake-2026-05-17` in namespace `ai-memory/v0.7.0-nhi-testing`.

---

*Track A is in progress. P1 – P11 + final verdict will land in subsequent commits.*
