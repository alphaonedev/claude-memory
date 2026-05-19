# Track A — NHI Test Playbook Results (2026-05-17)

Per the v0.7.0 NHI test playbook (memory id `8ccc7fed-7b93-4d2e-8d83-ea2562637f95`, namespace `ai-memory/v0.7.0-nhi-testing`). All 12 phases (P0 → P11) re-run on the post-PR-#820-fixes binary.

| Phase | Status | Pass / Fail / Gap | Result memory id |
|-------|--------|-------------------|------------------|
| P0  Environment       | ✅ done   | 6 / 0 / 1 | `83b2e21c` `NHI-P0-handshake-2026-05-17` |
| P1  Core CRUD         | ✅ done   | 5 / 0 / 1 | `4f5d67db` `NHI-P1-core-crud-2026-05-17` |
| P2  Lifecycle         | ✅ done   | 7 / 0 / 2 | `e9d35a8d` `NHI-P2-lifecycle-2026-05-17` |
| P3  Knowledge graph   | ⏳ pending | — | — |
| P4  Governance & sec  | ⏳ pending | — | — |
| P5  Power tools       | ⏳ pending | — | — |
| P6  Capabilities v3   | ⏳ pending | — | — |
| P7  Token budget      | ⏳ pending | — | — |
| P8  Hooks             | ⏳ pending | — | — |
| P9  Cross-interface   | ⏳ pending | — | — |
| P10 Performance       | ⏳ pending | — | — |
| P11 Chaos             | ⏳ pending | — | — |
| Verdict               | ⏳ pending | — | — |

**Rolling totals: 18 pass / 0 fail / 4 gap** across 3 phases done.

## Phase 0 — Environment handshake

| Test | Expected | Actual | Verdict |
|------|----------|--------|---------|
| `ai-memory --version` | `ai-memory 0.7.0` | `ai-memory 0.7.0` | PASS |
| `readlink -f $(which ai-memory)` | under `v07-f5/target/release/` | under `v07-fixes/.cargo-shared-target/release/` | PASS (path-drift noted) |
| `memory_capabilities.version` | `0.7.0` | `0.7.0` | PASS |
| `memory_capabilities.schema_version` | `2` or v3 | `3` | PASS (v3 surface live, summary field present) |
| All 8 families loaded | true | true (core, lifecycle, graph, governance, power, meta, archive, other) | PASS |
| `SELECT MAX(version) FROM schema_version` | `28` (May 7 baseline) | `43` | PASS-EXPECTED (28→43 progression over 10 days) |
| trimmed token total | ≤ 3500 | 3449 (51 token headroom) | PASS |
| verbose token total | 5K–10K | 15400 | GAP (over playbook ceiling; opt-in surface; trending wrong direction) |
| max per-tool tokens | ≤ 1500 | 988 (memory_recall) | PASS (71 tools, 0 over ceiling) |

## Phase 1 — Core CRUD

Fixture id: `365fef12-2bfe-491a-ad1a-e7c0fd91d8a7` (`NHI-P1 smoke 2026-05-17`).

| Test | Result | Evidence |
|------|--------|----------|
| memory_store round-trip | PASS | id 365fef12; agent_id `ai:claude-code@FROSTYi.local:pid-23799` stamped; tier=mid default |
| memory_recall (semantic + FTS5 blend) | PASS | rank #2 score 0.751 (rank #1 is May 7 same-titled fixture at 0.752) |
| memory_search FTS5 keyword AND | PASS | "phase 1 marker" surfaces fixture among 7 namespace hits |
| memory_list visibility | PASS | 20 memories in namespace; new fixture visible |
| memory_get round-trip (15 fields) | PASS | All fields present + recent timestamps; agent_id matches session |
| agent_id immutability via memory_update | GAP (MCP-surface coverage) | MCP `memory_update` schema only accepts `id`+`namespace` — no way to attempt mutation. Substrate-layer preservation verified by other test paths; MCP-coverage gap filed as **#826**. |

## Phase 2 — Lifecycle

Short-tier fixture: `642dc316` (subsequently promoted then deleted).

| Test | Result | Evidence |
|------|--------|----------|
| CLI store --tier short | PASS | id 642dc316, tier=short, expires_at=create+6h |
| 5× rapid recall + access_count | PASS | Touched the *previous* P1 fixture (365fef12) which auto-promoted mid→long at access #5 (final access_count=6, tier=long, **expires_at absent**, confirming "long has no expiry"). |
| Auto-promote at 5-access threshold | PASS | 365fef12 mid→long after 5 recalls — exactly the CLAUDE.md contract |
| Sliding-window short-TTL on access | **GAP — docs drift** | 642dc316 expires_at went from 18:50 (create+6h) → 13:50 (last_access+1h) — TTL was **shortened**, not extended. CLAUDE.md says "extend TTL"; actual behavior is "set to now+ttl regardless". Captured for the final docs-alignment mission (#19). |
| Priority bump on touch | PASS | a690e247 (May 7 fixture) priority 5→6 via access path |
| memory_promote explicit short→long | PASS | Single call jumped tier directly; CLAUDE.md narrative implies short→mid→long progression — verb evidently accepts target jumps. Doc-implicit. |
| memory_delete by id | PASS | Returns `deleted:true` |
| memory_get after delete | PASS | Returns sanitized `memory not found` (no path/PII leak) |
| memory_gc trigger | PASS | Returns `collected:0, dry_run:false` |
| memory_forget by query | SKIPPED | Destructive bulk-delete deliberately skipped on shared live DB. Same primitive (delete) verified above. |

### Doc-drift findings captured for FINAL MISSION (#19)
- TTL "extend" wording in CLAUDE.md §Recall Pipeline → should clarify "sliding-window-RESET-to-now+ttl" behavior.
- `memory_promote` short→long direct jump vs implied short→mid→long progression.

---

*Track A pending P3–P11 + verdict. Continuation handoff at `.local-runs/handoff-prompt-next-session-2026-05-17-pm.md`.*
