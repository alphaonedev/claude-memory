# v0.7.0 Review Synthesis — Fix Dispatch Backlog

> Synthesized from 6 parallel reviewers (security/correctness/perf/API-UX/docs/coverage)
> Base commit: `0536e96` (feat/v0.7.0-grand-slam HEAD with CHANGELOG + ship-readiness fixes)
> Reviewer baseline: `ef92bd7` / `64528b1` (pre-ship-readiness reconciled trunk)
> Issue: #767
> Date: 2026-05-15

## Reviewer source counts (raw findings)

| Lens | File | CRIT | HIGH | MED | LOW | INFO | Total |
|---|---|---|---|---|---|---|---|
| Security | `review-security/docs/internal/v070-review-security.md` | 0 | 5 | 9 | 6 | 3 | 23 |
| Correctness | `review-correctness/docs/internal/v070-review-correctness.md` | 1 | 3 | 6 | 3 | 0 | 13 |
| Performance | `review-performance/docs/internal/v070-review-performance.md` | 2 | 5 | 6 | 4 | 0 | 17 |
| API+UX | `review-api-ux/docs/internal/v070-review-api-ux.md` | 0 | 5 | 6 | 5 | 4 | 20 |
| Docs | `review-docs/docs/internal/v070-review-docs.md` | 2 | 6 | 8 | 4 | 0 | 20 |
| Coverage | `review-coverage/docs/internal/v070-review-coverage.md` | 1 | 4 | 8 | 5 | 0 | 18 |
| **Total raw** | | **6** | **28** | **43** | **27** | **7** | **111** |

> Prompt header said "103 findings"; actual count from re-reading the six docs is **111** (likely
> 103 was based on a pre-finalization head; net total higher by 8). All 111 are absorbed below or
> classified as INFO / accepted-debt.

## Severity summary (after dedup + cluster)

- **CRITICAL: 4** fixes (dedupe'd from 6 raw CRIT findings — one already resolved, one merged)
- **HIGH: 13** fixes (dedupe'd from 28 raw HIGH findings)
- **MEDIUM: 14** fixes (dedupe'd from 43 raw MED)
- **LOW: 10** fixes (dedupe'd from 27 raw LOW)
- **INFO / Already-fixed / accepted-debt: 14** (dedupe'd from 7 INFO + cross-lens dupes)
- **Cluster count: 12** (A–L)

## Known dedupes applied

| Root issue | Dedupe'd from | Cluster |
|---|---|---|
| UTF-8 panic in `compute_atom_span` | COR-1 CRIT + COV-3 HIGH | A |
| `idx_memories_source_uri` is dead code | PERF-3 HIGH | A |
| `row_to_memory` drops corrupt provenance JSON silently | COR-3 HIGH | A |
| `read_atomised_into` swallows all DB errors | COR-2 HIGH | A |
| Form 1 LLM prompt-injection mass-delete | SEC-1 HIGH + COR-6 MED + PERF-7 HIGH (per-candidate content truncation) | B |
| Form 1 verdict drop / synthesis fallback | COR-5 MED + COR-6 MED + SEC-11 MED | B |
| signed_events BEGIN DEFERRED race | SEC-3 HIGH + SEC-19 LOW (atomisation re-use) + COR-9 MED (read_chain_head NULL collapse) | C |
| L1-6 fail-open + handle_deref IDOR | SEC-2 HIGH + SEC-4 HIGH | D |
| MemoryKind parse_csv typo-collapse + Skills CLI/HTTP parity | COR-4 HIGH + API-2 HIGH (Skills slice) | E |
| Performance hot-paths: per-store db::open + recall N+1 + touch txn fan-out + mem.clone | PERF-1 CRIT + PERF-2 CRIT + PERF-6 HIGH + PERF-10 MED + PERF-14 LOW + PERF-15 LOW | F |
| Form 5 shadow-mode unboundedness + env-var-on-hot-path + calibrate materialisation | PERF-4 HIGH + PERF-9 MED + PERF-12 MED | G |
| MCP tool count drift (71 not 63) | DOC-4 HIGH + API-1 HIGH + API-7 MED + API-20 LOW | H |
| README/release-notes/MIGRATION/cookbook/API_REF/CLI_REF drift | DOC-2/3/5/6/7/8/9/10/11/12/14/15/16/17/18/19 | H |
| Migration filename collisions (0031_*, 0018_*) | COR-13 LOW + DOC-20 LOW | J |
| QW-4 missing / phantom | API-12 HIGH + DOC (implicit in DOC-2) | K |
| CHANGELOG silent for Forms/QW/WT-1 | DOC-1 CRIT → **RESOLVED by `bbeb1e8`** (verified: 59 matches for Form/atomise/persona/offload/calibrate in current CHANGELOG.md) | Already-fixed |
| Postgres integration tests not running in CI | COV-1 CRIT + COV-6 HIGH | I |
| Missing acceptance/handler-envelope tests | COV-2/4/9/10/11/12/13/14 | (folded into A/B/D/G + I) |
| K10 HMAC negative cross-method test | COV-5 HIGH | C (test-pin add-on) |
| Form 7 governance matcher: substring vs regex + args=Vec::new() + lossy command_str | SEC-12 MED + SEC-13 MED + SEC-17 LOW + COR-10 MED | D |

---

## Fix dispatch clusters

Each cluster is a self-contained fix-agent prompt. Clusters are designed to be parallel-safe
unless the **Parallel-safe with** / **Dependencies** rows say otherwise.

### CLUSTER A — Form 4 fact-provenance correctness + atomisation idempotency

- **Scope:**
  1. Fix UTF-8 panic in `compute_atom_span`: replace `start.saturating_add(1)` with a
     codepoint-aware advance (use `body.char_indices()` or `floor_char_boundary`).
  2. Distinguish `QueryReturnedNoRows` from real errors in `read_atomised_into` — propagate
     real errors via `?` so the idempotency guard doesn't double-atomise on lock-timeout.
  3. Replace `unwrap_or_default()` / `.ok().and_then(serde_json::from_str)` silent drops in
     `row_to_memory` for `citations` / `source_span` / `confidence_signals` with the existing
     `metadata`-style `tracing::warn!("corrupt … defaulting to …", row_id)` pattern + a
     counter metric.
  4. Push `source_uri_prefix` filter into recall SQL `WHERE` clause so
     `idx_memories_source_uri` is actually consulted (currently dead code).
  5. Tighten `validate_source_span` to require `end <= body.len()` and char-boundary checks.
  6. Tests: add `compute_atom_span_paraphrase_fallback_returns_none`,
     `compute_atom_span_multibyte_utf8_stays_on_char_boundary`,
     `recall_with_source_uri_prefix_uses_idx_memories_source_uri` (EXPLAIN QUERY PLAN check),
     and a `row_to_memory_corrupt_citations_logs_and_returns_default`.
- **Files (absolute):**
  - `/Users/fate/v07/synth-fix/src/atomisation/mod.rs` (compute_atom_span, read_atomised_into)
  - `/Users/fate/v07/synth-fix/src/storage/mod.rs` (row_to_memory)
  - `/Users/fate/v07/synth-fix/src/validate.rs` (validate_source_span)
  - `/Users/fate/v07/synth-fix/src/cli/recall.rs` and `src/storage/mod.rs` recall SQL site
    (push source_uri_prefix into WHERE)
  - `/Users/fate/v07/synth-fix/tests/form_4_provenance.rs`,
    `/Users/fate/v07/synth-fix/tests/atomisation.rs`
- **Findings absorbed:** COR-1 CRITICAL, COR-2 HIGH, COR-3 HIGH, COR-7 MED, PERF-3 HIGH, COV-3 HIGH
- **Acceptance tests required:** four new tests listed above; CI gate green.
- **LOE:** ~1 session (3–4 hours)
- **Parallel-safe with:** B, C, D, E, G, H, I, J
- **Dependencies:** none

### CLUSTER B — Form 1 synthesis security + verdict-application correctness + prompt-injection guard

- **Scope:**
  1. **SEC-1 fix:** re-run the K9 permission gate on every `verb:"delete"` candidate before
     applying; refuse verdict batches that delete > N (default N=1) without an explicit K10
     approval flow; wrap incoming title/content into a
     `<USER_CONTENT>...</USER_CONTENT>` envelope in the synthesis prompt so the system
     prompt tells the model to treat as opaque.
  2. **COR-5 fix:** decide policy on multi-update verdicts (recommend honour all updates in
     sequence; if not, emit `tracing::warn!` per dropped update and adjust
     `synthesis_decisions.update_dropped`).
  3. **COR-6 fix:** surface synthesis failure in the response envelope
     (`synthesis_failed: true, reason: "..."`); optionally refuse-on-curator-down for
     namespaces that opted into `auto_atomise_mode = Synchronous`.
  4. **PERF-7 fix:** truncate each candidate's `content` to a configurable cap
     (default 1500 chars / ~400 tokens) before inlining into the LLM prompt; add a
     `synthesis_prompt_size` telemetry counter.
  5. **SEC-11 doc:** document explicitly that synthesis is a *quality* gate, not a *security*
     gate; add per-namespace `synthesis_failure_mode = "fall_through" | "block_write"` knob.
  6. Tests: `synthesis_delete_verdict_consults_k9_per_candidate`,
     `synthesis_response_carries_synthesis_failed_on_llm_error`,
     `synthesis_prompt_truncates_candidate_content_at_cap`.
- **Files:**
  - `/Users/fate/v07/synth-fix/src/synthesis/mod.rs`
  - `/Users/fate/v07/synth-fix/src/mcp/tools/store.rs`
  - `/Users/fate/v07/synth-fix/src/governance/` (K9 hook call site)
  - `/Users/fate/v07/synth-fix/tests/form_1_synthesis.rs`
- **Findings absorbed:** SEC-1 HIGH, SEC-11 MED, COR-5 MED, COR-6 MED, PERF-7 HIGH
- **LOE:** ~1 session
- **Parallel-safe with:** A, C, D, E, G, H, I, J
- **Dependencies:** Touches `src/mcp/tools/store.rs` — coordinate with **F** (memory_store
  hot-path refactor) by landing B first or F first, then rebase.

### CLUSTER C — Signed-events chain integrity (BEGIN IMMEDIATE + drainer DLQ + HMAC negative tests)

- **Scope:**
  1. **SEC-3 fix:** replace `conn.unchecked_transaction()` (DEFERRED default) with
     `conn.transaction_with_behavior(TransactionBehavior::Immediate)` in
     `append_signed_event`. Audit `signed_events.rs` for other unchecked_transaction sites.
  2. Drainer in `deferred_audit.rs` requeues on `SQLITE_CONSTRAINT_UNIQUE` specifically;
     add a DLQ for unrecoverable failures (counter-exposed via capabilities).
  3. **COR-9 fix:** `read_chain_head` `WHERE sequence IS NOT NULL` (don't COALESCE NULL
     to 0); emit a clear migration-needed diagnostic when a NULL-sequence row is observed
     post-v34.
  4. **COV-5 fix:** add negative cross-method + cross-pending_id tests:
     `hmac_cross_method_binding_rejected`, `hmac_cross_pending_id_binding_rejected`.
- **Files:**
  - `/Users/fate/v07/synth-fix/src/signed_events.rs`
  - `/Users/fate/v07/synth-fix/src/governance/deferred_audit.rs`
  - `/Users/fate/v07/synth-fix/src/atomisation/mod.rs:656` (SEC-19 site — closed by SEC-3 fix)
  - `/Users/fate/v07/synth-fix/tests/signed_events_chain_v34.rs`
  - `/Users/fate/v07/synth-fix/tests/k10_approval_security.rs`
- **Findings absorbed:** SEC-3 HIGH, SEC-19 LOW, COR-9 MED, COV-5 HIGH
- **LOE:** ~1 session
- **Parallel-safe with:** A, B, D, E, G, H, I, J
- **Dependencies:** none

### CLUSTER D — L1-6 governance fail-open + handle_deref IDOR + matcher correctness

- **Scope:**
  1. **SEC-2 fix:** at daemon startup, `tracing::error!` (not warn) when L1-6 not active AND
     `governance_rules` is non-empty; surface `l1_6_attest: false` in capabilities v3;
     add `[governance] require_operator_pubkey = true` config knob for fail-closed mode.
  2. **SEC-4 fix:** add `agent_id` parameter to `handle_deref` (matching `handle_offload`),
     thread to `ContextOffloader::deref`, refuse with `OffloadError::NotFound`
     (leak-resistant) when caller != stored agent_id AND K9 does not grant cross-agent read.
  3. **SEC-12 + COR-10 fix:** either rename `command_regex` → `command_substring` AND add
     validator that rejects regex metacharacters, OR actually implement regex matching gated
     on a `command_regex_kind: "regex"` discriminator. Recommend the validator+rename for
     v0.7.0; regex impl for v0.7.1.
  4. **SEC-13 fix:** plumb actual argv into `ProcessSpawn` wire-point at
     `hooks/executor.rs:469-478`; extend matcher with optional `args_contain` / `args_regex`.
  5. **SEC-17 fix:** use `as_os_str()` in command_str wire-check; convert to lossy only for
     log surface.
  6. **COV-8 fix:** add named test `tests/k9_kg_invalidate_governance_gate.rs::handle_kg_invalidate_refuses_when_rule_denies`.
- **Files:**
  - `/Users/fate/v07/synth-fix/src/governance/rules_store.rs`
  - `/Users/fate/v07/synth-fix/src/governance/agent_action.rs`
  - `/Users/fate/v07/synth-fix/src/hooks/executor.rs`
  - `/Users/fate/v07/synth-fix/src/mcp/tools/offload.rs`
  - `/Users/fate/v07/synth-fix/src/offload/mod.rs`
  - `/Users/fate/v07/synth-fix/src/capabilities.rs`
- **Findings absorbed:** SEC-2 HIGH, SEC-4 HIGH, SEC-12 MED, SEC-13 MED, SEC-17 LOW, COR-10 MED, COV-8 MED
- **LOE:** ~1 session
- **Parallel-safe with:** A, B, C, E, G, H, I, J
- **Dependencies:** none

### CLUSTER E — Form 6 kind-filter inversion + L1-5 skill HTTP/CLI parity + capability skills CLI

- **Scope:**
  1. **COR-4 fix:** distinguish `parsed-zero` from `omitted` in `MemoryKind::parse_csv`. Return
     `Some(vec![])` for empty-intentional → no matches; `None` for absent → no filter. Update
     `apply_kinds_filter`. Cascading test fixes.
  2. **API-3 fix:** add `#[arg(long = "kinds", alias = "kind")]` on CLI; update doc examples.
  3. **Skills parity slice (subset of API-2):** ship `ai-memory skill {register|list|get|export|promote|compose}`
     CLI subcommands AND `POST /api/v1/skill/{register,list,get,export,promote,compose}` HTTP
     routes. The handlers already exist in `src/mcp/tools/skill_*.rs`; the work is
     boilerplate copy-paste into the CLI dispatcher + Axum router.
  4. Tests: `mcp_handler_recall_kinds_empty_array_returns_zero_results`,
     `cli_recall_kinds_alias_kind_still_works`,
     skill CLI/HTTP integration tests (6 each).
- **Files:**
  - `/Users/fate/v07/synth-fix/src/models/memory.rs` (parse_csv)
  - `/Users/fate/v07/synth-fix/src/mcp/tools/recall.rs` (apply_kinds_filter)
  - `/Users/fate/v07/synth-fix/src/cli/recall.rs`
  - `/Users/fate/v07/synth-fix/src/cli/skill.rs` (NEW)
  - `/Users/fate/v07/synth-fix/src/handlers/http.rs` (NEW skill routes)
  - `/Users/fate/v07/synth-fix/src/lib.rs` (router additions)
  - `/Users/fate/v07/synth-fix/tests/form_6_memorykind_vocab.rs`
  - `/Users/fate/v07/synth-fix/tests/skill_test.rs`
- **Findings absorbed:** COR-4 HIGH, API-3 MED, API-2 HIGH (Skills slice only — other HTTP gaps
  in cluster K disposition).
- **LOE:** ~1.5 sessions (skills CLI+HTTP is the bulk)
- **Parallel-safe with:** A, B, C, D, G, H, I, J
- **Dependencies:** none

### CLUSTER F — Performance hot-paths (memory_store / memory_recall)

- **Scope:**
  1. **PERF-1 fix:** thread `&Connection` from MCP handler into `maybe_enqueue_auto_atomise`,
     `auto_persona`, `auto_export` rather than calling `db::open` per hook. Removes 5+
     SQLite syscalls per store.
  2. **PERF-2 fix:** in recall hybrid loop, extract embedding bytes in the row mapper
     (already in the SELECT); move cosine calc into row-iteration body. Eliminates N+1
     `get_embedding` queries.
  3. **PERF-6 fix:** batch all K touches into a single `BEGIN IMMEDIATE` transaction;
     merge the 3 UPDATEs into one CASE-driven statement.
  4. **PERF-10 fix:** change `maybe_enqueue_auto_atomise` / `run_synchronous_auto_atomise`
     to accept `&Memory + &str id`; remove `..mem.clone()` spread.
  5. **PERF-14 fix:** swap `Vec<Memory>` clone for `&[&Memory]` in `synthesise`.
  6. **PERF-15 fix:** subordinate to PERF-1.
  7. **PERF-5 fix:** treat Form-2 Synchronous mode as operator contract — document latency
     envelope in `auto_atomise_mode` docstring; add per-namespace deadline fall-through to
     Deferred on timeout; consider reducing default `max_retries` to 1.
  8. Tests: regression test asserting recall no longer issues separate `get_embedding`;
     regression test asserting K touches share one writer transaction.
- **Files:**
  - `/Users/fate/v07/synth-fix/src/mcp/tools/store.rs` (handler signature + hook wire-up)
  - `/Users/fate/v07/synth-fix/src/hooks/pre_store/auto_atomise.rs`
  - `/Users/fate/v07/synth-fix/src/hooks/post_reflect/auto_persona.rs`
  - `/Users/fate/v07/synth-fix/src/hooks/post_reflect/auto_export.rs`
  - `/Users/fate/v07/synth-fix/src/storage/mod.rs` (recall hybrid + touch)
  - `/Users/fate/v07/synth-fix/src/synthesis/mod.rs`
  - `/Users/fate/v07/synth-fix/src/atomisation/curator.rs` (max_retries)
- **Findings absorbed:** PERF-1 CRIT, PERF-2 CRIT, PERF-5 HIGH, PERF-6 HIGH, PERF-10 MED, PERF-14 LOW, PERF-15 LOW
- **LOE:** ~2 sessions (handler signature change cascades; well-scoped)
- **Parallel-safe with:** C, G, H, I, J, K, L
- **Dependencies:** **Sequential after B** (Cluster B touches `src/mcp/tools/store.rs` synthesis
  call site — must land first or merge-conflict). After F, all of A/B/D have touched the
  same area, so coordinate via the Group-1/Group-2 schedule below.

### CLUSTER G — Form 5 shadow-mode unboundedness + sampling + calibration streaming

- **Scope:**
  1. **PERF-4 fix:** add `shadow_retention_days` config (default 30) + sweeper in existing
     background GC scheduler; add compound `(namespace, source, observed_at)` index;
     stream the calibration aggregation (in-SQL GROUP BY or chunked scroll cursor).
  2. **PERF-9 fix:** read `AI_MEMORY_CONFIDENCE_SHADOW` + `_SAMPLE_RATE` env vars **once**
     at daemon boot into a `OnceLock<ShadowConfig>`; gate `observe()` on cached value.
  3. **PERF-12 fix:** rewrite calibration sweep as `SELECT namespace, source, COUNT(*),
     AVG(...), …` aggregate; use streaming median (Welford-style) over sorted cursor when
     percentile_cont unavailable.
  4. **COV-2 fix:** add `mcp_handler_calibrate_confidence_returns_baselines_envelope` test.
  5. **COV-14 fix:** add `recall_touch_with_decay_env_set_updates_decayed_at` test.
- **Files:**
  - `/Users/fate/v07/synth-fix/migrations/sqlite/0033_v07_form5_confidence_calibration.sql`
    (or new follow-up migration for the compound index + retention column)
  - `/Users/fate/v07/synth-fix/src/confidence/shadow.rs`
  - `/Users/fate/v07/synth-fix/src/confidence/calibrate.rs`
  - `/Users/fate/v07/synth-fix/src/background/` (new sweeper)
  - `/Users/fate/v07/synth-fix/tests/form_5_confidence_calibration.rs`
- **Findings absorbed:** PERF-4 HIGH, PERF-9 MED, PERF-12 MED, COV-2 HIGH, COV-14 LOW
- **LOE:** ~1.5 sessions (GC sweeper + retention column = bulk)
- **Parallel-safe with:** A, B, C, D, E, H, I, J, K, L
- **Dependencies:** none

### CLUSTER H — Docs + cookbook + CLI-help accuracy sweep (DOCS-ONLY)

- **Scope:**
  1. **DOC-4 / API-1 / API-7 / API-20 (tool count drift):** single source of truth.
     `build.rs` substitution OR CI grep gate against `Profile::full().expected_tool_count()`.
     Update README badge `7_default • 71_full`, narrative "7 always-loaded ... 71 total",
     release-notes "60 → 71", `daemon_runtime.rs:181` `--profile full` help string from
     "43 tools" to "71 tools at v0.7.0".
  2. **DOC-2 (release notes):** append Post-grand-slam Form 1-6 + 7th-form + QW wave section
     to `docs/v0.7.0/release-notes.md` mirroring `docs/internal/v070-feature-inventory.md`.
  3. **DOC-3 (README "5 substrates"):** rewrite "What's new in v0.7" anchored on canonical
     feature inventory; link `docs/atomisation.md`, `docs/persona.md`,
     `docs/memory-kind-vocab.md`, `docs/confidence-calibration.md`, `docs/provenance.md`.
  4. **DOC-5:** flip evidence-frozen badge from `v0.6.4` to `v0.7.0` (or add a v0.7.0
     companion badge).
  5. **DOC-6/7/8/10/11:** rewrite `docs/MIGRATION_v0.7.md` — delete `TODO — track X lands`
     blocks; rename `memory_approval_pending`/`memory_approval_decide` →
     `memory_pending_list`/`memory_pending_approve`/`memory_pending_reject`; fix "20 lifecycle
     events" → "25"; either drop `doctor --kg-backend` claim or ship the flag (recommend
     drop the claim, document `ai-memory doctor --remote` JSON grep); add per-form sections.
  6. **DOC-9:** in `docs/memory-kind-vocab.md`, replace `ai-memory doctor --capabilities=v3`
     with `ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq .memory_kind_vocab`.
  7. **DOC-12:** in `cookbook/multistep-ingest/01-two-phase.sh`, replace hardcoded
     `/Users/fate/v07/v07-fixes/` paths with `"${REPO_ROOT}/.local-runs"` /
     `"${REPO_ROOT}/target"`.
  8. **DOC-13:** add post-audit prologue to `docs/internal/batman-framework-audit.md`
     declaring it reflects state at `53b4d39` and Forms 1-6 + 7th shipped in response (PRs
     #761-#766).
  9. **DOC-14/15:** sweep new HTTP routes from `src/handlers/http.rs` into
     `docs/API_REFERENCE.md`; add §"v0.7.0 new commands" block to `docs/CLI_REFERENCE.md`.
  10. **DOC-16:** ship MVP docs for Hook pipeline, Federation hardening, K8 quotas, K10 SSE
      approvals, Sidechain transcripts, Signed-events V-4 chain. 200-500 lines each in
      `docs/atomisation.md` style.
  11. **DOC-17:** create `RELEASE_NOTES_v0.7.0.md` at project root (synopsis + pointer to
      `docs/v0.7.0/release-notes.md`) OR document convention in CONTRIBUTING.md.
  12. **DOC-18:** verify Gemma model identifier (`gemma3:4b` vs "Gemma 4 E2B") between
      `docs/atomisation.md` and `entrypoint.plan-c.sh`; reconcile.
  13. **DOC-19:** add reconcile banner to `docs/policy-engine.md`.
  14. **API-5 (error-code convention ADR):** define ADR — recommend lowercase.dotted as
      canonical; add alias map; update tests.
  15. **API-6:** add `/api/v1/load_family` alias for `/api/v1/memory_load_family` with
      Deprecation header; mention in MIGRATION_v0.7.md.
  16. **API-8/11:** rename CLI envelope `pending_id` → `id` (or add alias); replace
      `--from-shadow` bool with `--source <SOURCE>` enum.
  17. **API-10:** remove HTTP-admin-endpoints claim from `src/mcp/registry.rs:1325` docstring.
  18. **API-4:** add `"source": {"enum": ["shadow"], "default": "shadow"}` to MCP
      `memory_calibrate_confidence` inputSchema.
- **Files:** (many; docs and registry strings)
  - `/Users/fate/v07/synth-fix/README.md`
  - `/Users/fate/v07/synth-fix/docs/v0.7.0/release-notes.md`
  - `/Users/fate/v07/synth-fix/docs/MIGRATION_v0.7.md`
  - `/Users/fate/v07/synth-fix/docs/memory-kind-vocab.md`
  - `/Users/fate/v07/synth-fix/docs/atomisation.md`
  - `/Users/fate/v07/synth-fix/docs/policy-engine.md`
  - `/Users/fate/v07/synth-fix/docs/internal/batman-framework-audit.md`
  - `/Users/fate/v07/synth-fix/docs/API_REFERENCE.md`
  - `/Users/fate/v07/synth-fix/docs/CLI_REFERENCE.md`
  - `/Users/fate/v07/synth-fix/cookbook/multistep-ingest/01-two-phase.sh`
  - `/Users/fate/v07/synth-fix/src/daemon_runtime.rs:181` (help string)
  - `/Users/fate/v07/synth-fix/src/mcp/registry.rs:1325` (docstring)
  - `/Users/fate/v07/synth-fix/src/cli/governance.rs` (envelope key rename)
  - `/Users/fate/v07/synth-fix/src/cli/commands/calibrate_confidence.rs` (--source enum)
  - **NEW:** `docs/hooks.md`, `docs/federation.md`, `docs/quotas.md`, `docs/approvals.md`,
    `docs/transcripts.md`, `docs/signed-events.md`
  - **NEW:** `RELEASE_NOTES_v0.7.0.md` (root)
- **Findings absorbed:** DOC-2 CRIT (DOC-1 RESOLVED separately), DOC-3/4/5/6/7/8/9/10/11/12/13/14/15/16/17/18/19 (all), API-1 HIGH, API-2 HIGH (docs slice — full slice in K), API-4 MED, API-5 MED, API-6 MED, API-7 MED, API-8 LOW, API-10 INFO, API-11 LOW, API-20 LOW
- **LOE:** ~3 sessions (DOC-16 alone is 12-20 hours of net-new doc writing; split into a
  sub-cluster H-2 if needed)
- **Parallel-safe with:** all code clusters (docs-only — minimal overlap with `src/`; only
  three Rust touchpoints: `daemon_runtime.rs`, `mcp/registry.rs` docstring,
  `cli/commands/calibrate_confidence.rs`)
- **Dependencies:** none

### CLUSTER I — CI test-runner expansion (postgres integration tests not running)

- **Scope:**
  1. **COV-1 fix:** change `.github/workflows/ci.yml` lines 211, 252 from
     `cargo test --features sal-postgres --lib` and `cargo test --features sal --lib` to
     `cargo test --features sal-postgres --workspace` (or drop `--lib`). Verify the
     pgvector/pgvector:pg16 service container + `AI_MEMORY_TEST_POSTGRES_URL` env var are
     wired through.
  2. **COV-6 fix:** add `tests/postgres_schema_parity.rs::pre_form6_postgres_db_upgrades_memory_kind_column_idempotently`.
  3. **COV-7 fix:** add `tests/schema_ladder_v14_to_v39.rs::legacy_v064_db_forwards_migrates_through_all_arms`
     with a checked-in v0.6.4 seed DB (~50 KB) generated via
     `git checkout v0.6.4 && cargo run -- init && git checkout -`.
  4. **COV-9 fix:** `tests/offload/ttl_sweep_loop.rs::spawn_loop_drains_expired_within_interval_window`.
  5. **COV-10 fix:** `mcp_handler_dependents_envelope_round_trip`.
  6. **COV-11 fix:** `tests/persona/generate_acceptance.rs::generate_distils_cluster_into_persona_with_citations`
     with a `MockLlmDispatch`.
  7. **COV-12 fix:** `pre_v070_peer_payload_deserialises_with_default_provenance`.
  8. **COV-13 fix:** `mcp_handler_deref_envelope_round_trip` + `mcp_handler_deref_refuses_tampered_blob_with_structured_error`.
  9. **COV-4 fix:** `auto_persona_hook_fires_every_n_memories` end-to-end test.
- **Files:**
  - `/Users/fate/v07/synth-fix/.github/workflows/ci.yml`
  - `/Users/fate/v07/synth-fix/tests/` (numerous new files)
  - Seed DB: `/Users/fate/v07/synth-fix/tests/fixtures/v064_seed.db` (or equivalent)
- **Findings absorbed:** COV-1 CRIT, COV-4 HIGH, COV-6 HIGH, COV-7 MED, COV-9 MED, COV-10 MED, COV-11 MED, COV-12 MED, COV-13 MED
- **LOE:** ~2 sessions (CI gate flip = 0.5h; the 8 test adds = remainder)
- **Parallel-safe with:** all (test-only)
- **Dependencies:** **Sequential after A** (COV-3 belongs in A; once A lands, COV-3 closes
  organically; if A is delayed, COV-3 stays open here). The other adds are independent.

### CLUSTER J — Migration filename collisions cleanup

- **Scope:**
  1. **COR-13 / DOC-20 fix:** rename `migrations/sqlite/0031_v07_namespace_auto_atomise.sql`
     → `migrations/sqlite/0031_v07_namespace_auto_atomise.NOTES.md` (or move under
     `docs/internal/migration-notes/`). Same for `migrations/postgres/0018_v07_persona.sql`
     vs `0018_v07_namespace_auto_atomise.sql`.
  2. Verify `grep -rn "0031_v07_namespace_auto_atomise" src/` returns zero matches (then
     safe to rename — currently it returns zero per DOC-20).
  3. Add `tests/migration_filename_unique.rs::no_duplicate_migration_prefix_collisions` ship-gate.
- **Files:**
  - `/Users/fate/v07/synth-fix/migrations/sqlite/0031_v07_namespace_auto_atomise.sql`
  - `/Users/fate/v07/synth-fix/migrations/sqlite/0031_v07_persona.sql` (verify slot)
  - `/Users/fate/v07/synth-fix/migrations/postgres/0018_v07_namespace_auto_atomise.sql`
  - `/Users/fate/v07/synth-fix/migrations/postgres/0018_v07_persona.sql`
  - **NEW:** `/Users/fate/v07/synth-fix/tests/migration_filename_unique.rs`
- **Findings absorbed:** COR-13 LOW, DOC-20 LOW
- **LOE:** 30 minutes
- **Parallel-safe with:** all
- **Dependencies:** none

### CLUSTER K — QW-4 honest disposition (operator decision)

- **Scope:**
  - **Decision point** — EITHER:
    (a) Ship QW-4 (Tencent's "fourth quick win"; competitive positioning beyond docs).
        Scope, design, implement, test in v0.7.0 — would push tag-cut.
    (b) Re-attribute QW-4 to Form 7 governance refusals (`MCP_MUTATION_DISABLED_ERROR` is a
        plausible QW-4 anchor) and update CHANGELOG + release-notes accordingly.
    (c) Defensibly defer to v0.7.1 — remove QW-4 from "shipped" lists in CHANGELOG /
        release-notes / docs/positioning.md; document as "scoped for v0.7.1".
  - **Recommendation:** (c) defer — fastest path to tag-cut with no procurement lie.
  - HTTP-coverage gap (API-2 minus the Skills slice handled in E): same operator decision.
    13 missing routes; recommend (b) "honest documentation" in MIGRATION_v0.7.md noting
    the MCP-only surface, with HTTP routes scoped for v0.7.1.
- **Files:** (depending on decision)
  - `/Users/fate/v07/synth-fix/CHANGELOG.md`
  - `/Users/fate/v07/synth-fix/docs/v0.7.0/release-notes.md`
  - `/Users/fate/v07/synth-fix/docs/positioning.md`
  - `/Users/fate/v07/synth-fix/docs/internal/v070-feature-inventory.md` (KU #9 closure note)
- **Findings absorbed:** API-12 HIGH, API-2 HIGH (residual after E), DOC implicit
- **LOE:** Option (a) 1-2 sessions; (b) 30 minutes; (c) 30 minutes
- **Parallel-safe with:** all
- **Dependencies:** **OPERATOR DECISION REQUIRED**

### CLUSTER L — INFO / accepted-debt / operator-document-only (no fix-agent dispatch)

These are real findings but operator-defensible or explicitly scoped out of v0.7.0. Flag for
documentation rather than code change.

- **SEC-5 (case-sensitive `host:` filter)** — LOW. Defense-in-depth; lowercase the compare
  in two sites. Could roll into D as a 30-min add-on (recommend).
- **SEC-6 (single-key HTTP auth)** — MEDIUM but documented as design (out of scope for v0.7.0
  per the federation hardening doc). Document the limitation in security posture page.
- **SEC-7 (federation x-peer-id is operator-bound not cert-bound)** — MEDIUM. v0.8.0 closeout
  tracked; v0.7.0 mitigation = startup-time WARN on bypass env vars (1 hour; recommend roll
  into D).
- **SEC-8 (validate_id accepts `..`/`/`/`\`)** — MEDIUM. Tighten validator + add `\` to
  filesystem-emit sanitisers (recommend roll into D as add-on).
- **SEC-9 (SSRF validate_url_dns fails open on DNS hiccup)** — MEDIUM, documented design.
  Optional `validate_at_dispatch_too` opt-in for hardening; recommend defer.
- **SEC-10 (quota_status K9-rule permissive enumeration)** — MEDIUM, docs-only. Roll into H.
- **SEC-14 (`validate_namespace` ".." only at segment level)** — LOW. Defense-in-depth.
- **SEC-15 (auto-export detached-thread silent failure)** — LOW. Counter add (recommend
  defer).
- **SEC-16 (persona entity_id minimal validation)** — LOW. Roll into D.
- **SEC-18 (env-prefix redactor case sensitivity)** — LOW. Add `pass` / `pwd` to keyword list.
- **SEC-20 (K10 SSE 1024 channel capacity, no per-agent rate-limit)** — INFO, v0.8.0 scope.
- **SEC-21 (publish-sdks.yml tag-trigger)** — INFO, procurement-grade acceptable.
- **SEC-22 (migration 0025 backfill)** — INFO, safe.
- **SEC-23 (Form 5 shadow honors contract)** — INFO, positive.
- **COR-11 (last_err unreachable placeholder)** — LOW, cosmetic.
- **COR-12 (env-var test races)** — LOW. `serial_test` crate or process mutex. Roll into G.
- **COR-8 (`archive_source` BEGIN IMMEDIATE in outer tx)** — MEDIUM. Use
  `unchecked_transaction` + nested-tx detection. Recommend roll into A.
- **PERF-8 (`auto_persona` LIKE %X% scan)** — MEDIUM. Requires schema column extension +
  backfill; recommend defer (v0.7.1 climb-back).
- **PERF-11 (Form 3 content duplication across stages)** — MEDIUM. Recommend defer.
- **PERF-13 (deferred_audit unbounded channel)** — MEDIUM. Recommend roll into C
  (signed_events cluster).
- **PERF-16 (`format!` in Form 1 candidate loop)** — LOW, 5-iter bound. Defer.
- **PERF-17 (auto_persona resolve_entity_id JSON parse)** — LOW. Subordinate to PERF-8.
- **COV-15/16/17/18** — LOW, opportunistic test adds. Recommend defer.
- **API-9 / API-13/14/15/16/17/18/19** — INFO. Net-zero confirmations.

---

## Already-fixed (verified at `0536e96`)

- **DOC-1 CHANGELOG silent for Forms/QW/WT-1** → **RESOLVED by commit `bbeb1e8`**
  (`docs(CHANGELOG): comprehensive v0.7.0 entry update — WT-1 + QW + Batman 6+7 + security
  reconciliation`). Verified via
  `grep -c "Form|atomis|persona|offload|calibrate" CHANGELOG.md` → 59 matches.
- **Postgres init retry loop (PERF "areas requiring investigation" #4)** → addressed by
  `0536e96` (`fix(v0.7.0 ship-readiness): postgres concurrent-init retry + g3 timeout +
  stderr-redaction test alignment`). Reviewer-Performance could not locate this in the
  baseline; it landed AFTER the reviewer's anchor.
- **g3 timeout flake (Flake census #1)** → addressed by `0536e96` (same commit).
- **stderr-redaction test alignment** → addressed by `0536e96`.

---

## Parallel-safe dispatch order

**Group 1 (parallel, low/no shared-write-surface overlap):**
- CLUSTER A (Form 4 code paths — `atomisation/`, `storage/`, `validate.rs`)
- CLUSTER C (signed_events.rs + deferred_audit.rs)
- CLUSTER D (governance/ + hooks/executor.rs + mcp/tools/offload.rs)
- CLUSTER E (MemoryKind enum + Skills CLI/HTTP — new files mostly)
- CLUSTER G (confidence/shadow.rs + calibrate.rs + new background sweeper + new migration)
- CLUSTER H (docs sweep — touches `src/` in only 3 small spots)
- CLUSTER I (CI workflow + new test files)
- CLUSTER J (migration filename rename + new test)

**Group 2 (after Group 1; touch overlapping write-path files):**
- CLUSTER B (Form 1 synthesis — `src/mcp/tools/store.rs` synthesis call site +
  `src/synthesis/mod.rs`)
- CLUSTER F (memory_store + memory_recall hot-path refactor —
  `src/mcp/tools/store.rs` handler signature + `src/storage/mod.rs` recall path)

**Coordination notes:**
- B and F both touch `src/mcp/tools/store.rs`. Land B first (smaller, security-load-bearing),
  then F (signature refactor that can rebase cleanly).
- A and F both touch `src/storage/mod.rs` but in different functions
  (`row_to_memory` vs `recall_hybrid`/`touch`); should not conflict.
- D and B both consult K9 governance — D lands the wire-points; B consumes them. Land D first
  if you want B's K9-on-delete to use the cleaned-up matcher; otherwise either order works.

**Group 3 (operator decision):**
- CLUSTER K (QW-4 disposition + HTTP-coverage policy decision)

**Already-fixed verification:**
- Confirm `bbeb1e8` actually covers DOC-1 (done — 59 matches).
- Confirm `0536e96` covers the postgres init retry / g3 timeout / stderr-test alignment
  (per commit message — done).

---

## Operator decision points (cannot autonomously resolve)

1. **CLUSTER K disposition** — QW-4: ship (a), re-attribute (b), or defer (c)?
   Recommendation: **(c) defer**, accompanied by HTTP-coverage gap honesty-doc in
   MIGRATION_v0.7.md.
2. **CLUSTER H sub-scoping** — DOC-16 alone is 12-20 hours. Split off as cluster H-2
   "Hook pipeline + Federation + K8 quotas + K10 SSE + Transcripts + Signed-events
   dedicated docs" and defer to v0.7.0.1 doc-only point release? Or block tag-cut on it?
3. **CLUSTER E Skills HTTP routes** — confirm CLI Skills are wanted alongside HTTP
   (operator may prefer MCP-only by design); if so, downscope E to MemoryKind+CLI alias only.
4. **CLUSTER F PERF-5 default** — reduce `max_retries` from 3 to 1 on Synchronous path? This
   is a behavior change visible to operators relying on aggressive retry.
5. **API-5 error-code convention ADR** — pick UPPER_SNAKE vs lowercase.dotted as canonical
   (recommend lowercase.dotted).
6. **LOE > 2-session clusters:**
   - **H** (docs sweep, ~3 sessions; split as H-1 stale-fix and H-2 net-new MVP docs).
   - **F** if PERF-5 default change requires a deprecation cycle.
7. **API-6 breaking-change alias decision** — keep `/api/v1/memory_load_family` working with
   Deprecation header (recommend) vs hard-rename (breaks v0.7.0-rc users).

---

## End-of-synthesis checklist

- [x] All 6 reviewer deliverables read in full at their pinned commits.
- [x] Verified DOC-1 (CHANGELOG silent) RESOLVED by `bbeb1e8` (59 matches for Form/atomise
      keywords in current CHANGELOG.md at `0536e96`).
- [x] Counted raw findings: 23 + 13 + 17 + 20 + 20 + 18 = **111** (prompt said 103;
      delta noted in §"Reviewer source counts").
- [x] Dedup applied per "Known dedupes" table.
- [x] 12 clusters defined with file paths, findings absorbed, LOE, parallel-safety, deps.
- [x] Group 1 / Group 2 dispatch schedule provided.
- [x] Operator decision points enumerated (7).
- [x] Already-fixed items distinguished from open work.

— Cold mountain.
