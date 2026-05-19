# Track A NHI re-run — for SME engineers + architects (2026-05-18)

This is the deep-dive page. For a one-screen summary go to [audience-c-level.md](audience-c-level.md). For the raw test grid go to [track-a-nhi-results.md](track-a-nhi-results.md). For the plain-English version go to [audience-non-technical.md](audience-non-technical.md).

---

## Reproducibility

**Pinned binary at write time:** git SHA `c3e344c7a` on branch `local/install-815-816`. Worktree: `/Users/fate/v07/v07-fixes/`. The campaign's earlier retest evidence was captured at SHA `e99fb0e` (post-fix batch on 2026-05-18 morning); the post-Gap-7 closeout moved HEAD forward through the provenance hardening commits (`6ad87c824` Gaps 1+2+5+6, `3cd8c116d` Gap 3 / schema v47, `23379e26f` Gap 4, `c3e344c7a` Gap 7).

**Schema version at HEAD:** v47 (constant `CURRENT_SCHEMA_VERSION` in `src/storage/migrations.rs`; postgres parity ladder ends at migration 0020).

**MCP tool count at `--profile full`:** 71 advertised = 70 callable + 1 always-on (`memory_capabilities`). Canonical assertion: `Profile::full().expected_tool_count()` in `src/profile.rs`. The +1 disambiguation is closed under issue [#862](https://github.com/alphaonedev/ai-memory-mcp/issues/862).

**Models (autonomous tier):** `nomic-ai/nomic-embed-text-v1.5` (768-dim), `cross-encoder/ms-marco-MiniLM-L-6-v2`, `gemma4:e4b` via Ollama.

**Databases:** operator's live MCP DB at `/Users/fate/.claude/ai-memory.db` for sqlite path; Plan C postgres+AGE fleet on 192.168.50.1 (AGE 1.5.0, vector 0.8.2 in all 6 ai-memory DBs).

---

## Test methodology

The NHI playbook is the canonical AI Non-Human Identity exercise — 12 phases (P0 through P11) executed by a Claude session acting as an MCP client of the ai-memory daemon. The playbook lives in memory `8ccc7fed-7b93-4d2e-8d83-ea2562637f95`, namespace `ai-memory/v0.7.0-nhi-testing`. Each phase has an expected and an actual; each test surfaced any drift to a GitHub issue per the prime directive testing-loop addendum.

| Phase | Name | Tests | Pass | Fail | Memory id |
|---|---|---|---|---|---|
| P0 | Environment | 10 | 10 | 0 | `5f7fc7d7` |
| P1 | Core CRUD | 6 | 6 | 0 | `72bab0fb` |
| P2 | Lifecycle | 12 | 12 | 0 | `41e909a5` |
| P3 | Knowledge graph | 12 | 12 | 0 | `7b3ccb9e` |
| P4 | Governance + sec | 11 | 11 | 0 | `31056e03` |
| P5 | Power tools | 6 | 6 | 0 | `01fd8c65` |
| P6 | Capabilities v3 | 6 | 6 | 0 | `6ff118c4` |
| P7 | Token budget | 6 | 6 | 0 | `ac9bb391` |
| P8 | Hooks | 5 | 5 | 0 | `e446d12b` |
| P9 | Cross-interface | 4 | 4 | 0 | `ae951362` |
| P10 | Performance | 4 | 4 | 0 | `b5f26792` |
| P11 | Chaos | 6 | 6 | 0 | `d02185ba` |
| **Total** | | **88** | **88** | **0** | verdict `a3c00030` |

(The README headline figure 85/0 reflects the pre-#862 / pre-#864 closure count where 3 tests were counted under "PASS with clarification in flight." Post-closure all 88 collapse to PASS-clean; the README/track-a-nhi-results.md will be reconciled in the upcoming dogfood pass.)

---

## Token budget data

Per the C2 capability-envelope ceiling (issue [#829](https://github.com/alphaonedev/ai-memory-mcp/issues/829)) and the post-[#859](https://github.com/alphaonedev/ai-memory-mcp/issues/859) restoration of optional-property discovery in `tools/list`:

| Metric | Pre-#829 | Post-#829 | Ceiling | Status |
|---|---|---|---|---|
| `full_profile_total_tokens` (verbose) | 15,570 | **9,827** | 10,000 | PASS (173 headroom) |
| `trimmed_full_profile_total_tokens` | 3,456 | **4,613** | 5,000 | PASS (387 headroom; ceiling raised 3,500 → 5,000 to support discovery) |
| `active_total_tokens` (core profile) | n/a | 3,021 | n/a | within budget |
| Per-tool max | n/a | 561 (`memory_recall`) | 1,500 | PASS |

CI guards pinning these forward: `tests/token_budget_guard.rs`, `tests/mcp_tools_list_schema_discovery.rs`, `tests/c2_tool_docs_field.rs`. The trimmed ceiling raise is a deliberate trade: NHI discoverability of optional parameters vs. wire-form minimum payload — discoverability wins because the alternative is every NHI re-fetching tool schemas via `tools/list` calls, which is more expensive in aggregate than the 5,000-token resident form.

---

## Schema migration ladder (v0.7.0 closeout)

| Migration | Issue | What changed |
|---|---|---|
| v43 → v44 | [#228](https://github.com/alphaonedev/ai-memory-mcp/issues/228) | Encrypt envelope schema for sqlcipher-aware row layout |
| v44 → v45 | [#884](https://github.com/alphaonedev/ai-memory-mcp/issues/884) | Memory `version` column (provenance Gap 5) |
| v45 → v46 | [#885](https://github.com/alphaonedev/ai-memory-mcp/issues/885) | Memory `source_uri` column (provenance Gap 6) |
| v46 → v47 | [#886](https://github.com/alphaonedev/ai-memory-mcp/issues/886) | `recall_observations` table for Tier 3 provenance (Gap 3 — recall-consumption observation) |

Tier 3 unlock (Gap 7, commit `c3e344c7a`, issue [#890](https://github.com/alphaonedev/ai-memory-mcp/issues/890)) decorates the `memory_recall` response surface with the full provenance bundle so the AI consumer sees what's verifiable without making a second call.

---

## Coverage data

Per the lane-2 floor-raise closures ([#838](https://github.com/alphaonedev/ai-memory-mcp/issues/838), [#839](https://github.com/alphaonedev/ai-memory-mcp/issues/839), [#840](https://github.com/alphaonedev/ai-memory-mcp/issues/840)):

| Module | Coverage | Floor | Notes |
|---|---|---|---|
| `src/storage/store.rs` | 96.40% | 90% | Was 78% pre-decomposition |
| `src/curator/mod.rs` | 96.19% | 90% | New module post-handlers split |
| `src/daemon_runtime.rs` | 87.15% | 85% | Top-level CLI dispatch + serve bootstrap |

CI gates these via `tarpaulin` thresholds; regression below floor breaks build.

---

## Architecture observations

The Wave-1 + Wave-2 + Wave-3 refactor mandate decomposed every monolithic file above the new `clippy::too_many_lines = 250` ceiling. Outcome (LOC measured at HEAD):

| File / function | Before | After | Issue |
|---|---|---|---|
| `src/handlers.rs` (monolith) | 14,840 LOC | `src/handlers/mod.rs` 69 LOC + per-domain modules each ≤ 1,200 LOC | [#650](https://github.com/alphaonedev/ai-memory-mcp/issues/650) (partial), [#866](https://github.com/alphaonedev/ai-memory-mcp/issues/866) |
| `create_memory` (handler) | 790 LOC | 158 LOC + extracted helpers | [#867](https://github.com/alphaonedev/ai-memory-mcp/issues/867) |
| `recall_hybrid` (handler) | 508 LOC | 115 LOC + extracted scorer | [#868](https://github.com/alphaonedev/ai-memory-mcp/issues/868) |
| `mcp` dispatch | 637 LOC | 274 LOC + per-tool handlers | [#871](https://github.com/alphaonedev/ai-memory-mcp/issues/871) |
| `src/storage/store.rs` | 3,009 LOC | mod 565 LOC + 6 per-domain modules | [#881](https://github.com/alphaonedev/ai-memory-mcp/issues/881) |

The decomposition contract: every top-level function ≤ 250 LOC (clippy enforced per [#873](https://github.com/alphaonedev/ai-memory-mcp/issues/873)), every module ≤ ~1,200 LOC (style guide, not lint), every domain split has its own integration test surface so refactors are reversible.

---

## Security review verdict

**Status: YELLOW with three cross-tenant subscription gaps fixed.** The R3-S1 subscription hardening series:

- [#870](https://github.com/alphaonedev/ai-memory-mcp/issues/870) — Cross-tenant subscription leak via shared HMAC secret
- [#872](https://github.com/alphaonedev/ai-memory-mcp/issues/872) — Replay window mis-scoped per-subscriber instead of per-tenant
- [#874](https://github.com/alphaonedev/ai-memory-mcp/issues/874) — DLQ replay enumeration leaked sibling-tenant subscription metadata

HMAC-required subscription dispatch is enforced ([`src/subscriptions.rs`](../../../src/subscriptions.rs) post R3-S1.HMAC). Unsigned dispatch is structurally disabled — the code path is removed, not gated. Substrate rules L1–L6 are Ed25519-signed by `AI_MEMORY_OPERATOR_PUBKEY`; `memory_rule_list` (P4 test 3) returned 4 operator-signed rules with valid signatures.

The YELLOW rather than GREEN reflects three remaining items in the test infrastructure (not the runtime): HTTP-layer probes for HMAC replay window, SSE tenant isolation, TOCTOU race, and zstd bomb — these are covered by the existing repo test suite (postgres `serve_*` tests confirmed green per iter #18) but not by an in-NHI-playbook probe surface. Closing the YELLOW requires those probes to be lifted into the NHI playbook itself.

---

## Code review verdict

**Status: YELLOW with four god-function splits done + clippy ceiling codified.**

The four function-decomposition issues above ([#866](https://github.com/alphaonedev/ai-memory-mcp/issues/866), [#867](https://github.com/alphaonedev/ai-memory-mcp/issues/867), [#868](https://github.com/alphaonedev/ai-memory-mcp/issues/868), [#871](https://github.com/alphaonedev/ai-memory-mcp/issues/871)) closed in the same campaign. The `clippy::too_many_lines = 250` lint level is codified in `Cargo.toml` per [#873](https://github.com/alphaonedev/ai-memory-mcp/issues/873) — any function above 250 LOC is now a build break.

The YELLOW reflects: there are still ~7 functions in the 175–249 range that are healthy but worth a second-pass review as part of the Wave-3 closure. They are below ceiling; the YELLOW is about discipline, not blocker risk.

---

## Forensic audit log

The L2-5 forensic bundle ([#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697), `src/forensic/`) writes Ed25519-signed governance decisions to an append-only audit chain (`signed_events.rs`, V-4 cross-row hash chain). Replayable via `ai-memory verify --since <duration>`. Every L1–L6 substrate-rule signing decision is in the chain.

---

## Federation signing

Issue [#791](https://github.com/alphaonedev/ai-memory-mcp/issues/791) made the `X-Memory-Sig` header mandatory by default on `/sync/push`. The env var `AI_MEMORY_FED_REQUIRE_SIG=1` is the secure-default value; `0` is the v0.6.x-compat opt-out during peer Ed25519-key enrolment. Federation peer attestation is still gated on the allowlist (`AI_MEMORY_FED_PEER_ATTESTATION`); the change here is making the signing surface required, not optional.

---

## Performance

The `PERFORMANCE.md` budget table is enforced by `cargo bench --bench recall` at a 10% regression tolerance via `.github/workflows/bench.yml`. Key budgets:

- `memory_recall` hot path: p95 < 50ms (autonomous tier, reranker on, embedder on)
- `kg_query` depth=5: p95 < 250ms (sqlite recursive-CTE; the AGE-Cypher path is faster but tested separately)
- Reranker: neural cross-encoder (`cross-encoder/ms-marco-MiniLM-L-6-v2`)
- Embedder: nomic-embed-text-v1.5 (768-dim)

P10 measured: 872 → 883 memories during session, no eviction storms, `ai-memory doctor` overall=INFO post-test, sustained load ~25 stores + 30 recalls + 7 links over ~7 minutes with zero errors.

---

## Plan C verification

The Plan C (Docker postgres+AGE fleet) verification on 192.168.50.x is GREEN at the data-plane level:

- 4-daemon fleet UP on 192.168.50.100
- Postgres + Apache AGE on 192.168.50.1 (AGE 1.5.0 + pgvector 0.8.2 in all 6 ai-memory databases)
- Embedding-dimension auto-migrate fix landed via [#877](https://github.com/alphaonedev/ai-memory-mcp/issues/877) — this was a real product bug (the AGE store path was not migrating embedding dims on first-open with a different model than the sqlite path expected). The fix is in `src/store/age/migrate.rs`.

The blocker for the cross-node Track C/D extension is the subnet route between .100 and 192.168.1.50 — operator-network-decision, not engineering.

---

## Provenance maturity — six levels (article reference)

The reference article (see RFC at `docs/v0.7.0/rfc-nhi-viewpoint.md`) defines six provenance levels. v0.7.0 status against each:

| Level | Name | v0.7.0 status |
|---|---|---|
| 1 | Identity | Carried — `agent_id` on every row, regex-validated, immutable post-write |
| 2 | Source | Carried — `source` field + `source_uri` post-Gap-6 (#885) |
| 3 | Causal | Carried — `derived_from` + `derives_from` link variants in `MemoryLinkRelation` enum |
| 4 | Capture confidence | Carried — `confidence_source`, `confidence_signals`, `confidence_decayed_at` columns |
| 5 | Versioned | Carried — `version` column post-Gap-5 (#884); incremented on update |
| 6 | Reciprocal | Carried — `observed_by` column on links; `recall_observations` table post-Gap-3 (#886) for Tier 3 |

Tier 2 by default, Tier 3 unlocked by Gap 7 (`memory_recall` response decoration) + signed-event chain enforcement (operator policy toggle).

---

## Open items + dispositions

Per the v0.7.0 release gate (issue [#836](https://github.com/alphaonedev/ai-memory-mcp/issues/836)), open items at write time are exactly four — and all four are operator-gated, not engineering-blocked:

| Item | Type | Gate |
|---|---|---|
| #836 | Release gate verification | Operator approval (8-tier verification) |
| Ship-readiness final review | Process | Operator approval |
| #833 (DO CPU hive) | Provisioning | Operator $-approval |
| #834 (AWS GPU burst) | Provisioning | Operator $-approval |

**Zero engineering-blocked issues.** Every issue surfaced in this campaign was fixed in v0.7.0 (per the testing-loop addendum), no deferral to v0.7.1+.

---

## Discipline artifacts

The directives governing this work, all in canonical memory:

- **Prime directive pm-v3 (verify-before-claiming + no-operator-handoffs):** memory `cd8ede94-3376-4837-b570-9d975290ae08`, namespace `global/policies`. Supersedes pm-v2 (`28860423`) and the testing-loop addendum (`f1dca8fa`).
- **Orchestrator safeguards (7-check protocol C1–C7):** memory `a1cc142d-053a-49ab-83bd-1a99992fa93e`, namespace `_v070_orchestrator_safeguards`. Set as namespace standard.
- **Violations log:** memory `3b5378e4-c709-40be-900d-8b09cdb05833`, namespace `_v070_orchestrator_safeguards/violations`.
- **Lane index:** memory `f970d6f6-7bde-4d6b-9a53-500734961e04`, namespace `_v070_strategic_tracking`.

The orchestrator safeguards' C7 (discrepancy detection) is what kept this campaign honest — every agent return is verified against observable state (git log, gh issue list, cargo test output, LOC counts) before the task can mark complete. A discrepancy triggers re-dispatch + violation logging, not silent acceptance.

---

## What's still TBD after this writeup

Honest disclosure of what this audience-sme-engineer.md page does NOT yet cover:

1. **Provenance Gap 1–7 individual writeups.** Will land after Agent B (provenance docs scope) returns. Track at `docs/v0.7.0/provenance-gaps/` once created.
2. **Coverage agent's final number for `src/store/age/`.** The AGE-path coverage is in flight; the floor target is 85% but the current measure is incomplete.
3. **Dogfood pass on the rebuilt post-Gap-7 binary.** The `scripts/dogfood-rebuild.sh` run on the current HEAD `c3e344c7a` is queued; the 24h dogfood window per the release gate tier 8 is not yet started.
4. **A2A campaign writeups.** Pending corpus arrival from the IronClaw 4-domain run with Grok 4.3.

These are real gaps, not "out of scope." They are queued.

---

*Drafted by Claude (Opus 4.7, 1M context) on 2026-05-18, against binary SHA `c3e344c7a`. Every claim on this page traces to a commit SHA, file path, memory id, GH issue URL, or canonical CLAUDE.md section. If a number on this page disagrees with what you measure on the binary, the binary wins — file an issue.*
