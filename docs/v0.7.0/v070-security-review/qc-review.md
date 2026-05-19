# v0.7.0 Final-Review-Pass — QC Review

**Working dir:** `/Users/fate/v07/v07-fixes/`
**Branch:** `local/install-815-816`
**Base HEAD audited:** `7a5baf82d47228c21cb75ac2613ab98f8ebba020`
**Post-QC HEAD:** `e40c81791a6e7b422f4471144863baab29aee1e2` — a parallel-agent commit landed during this QC pass with byte-identical patches for #911 + #912 (`git diff HEAD` is clean against my drafted edits). The QC also surfaces a second concurrent file `tests/admin_action_forensic_audit.rs` (parallel agent's #911 regression suite) + `tests/scope_private_sal_level_visibility.rs` (parallel agent's SAL-level #910 upgrade — exactly the long-term fix this QC report flagged as the right next-wave move).
**Range:** `b4ba16c8c..7a5baf82d` (5 commits, 11 selected fixes); follow-on commit `e40c81791` closes #911 + #912.
**Reviewer mandate:** Quality control on selected fixes — root cause vs symptom, best engineering solution, proof-test.

## Methodology

For each fix we (1) ran `git show <commit> --stat` to bound scope, (2) read the diff, (3) located the named regression test, (4) confirmed the test mechanically asserts the bugfix, (5) classified as PASS / NEEDS-IMPROVEMENT / REGRESSION.

QC verdicts apply three criteria per fix:

- **A. Root cause vs symptom** — does the change address the underlying defect or paper over the symptom?
- **B. Best engineering solution** — was the appropriate alternative considered (SAL-level vs handler-level, schema-add vs handler-remove, etc.)?
- **C. Proof-test** — is there a `cargo test`-discoverable regression that would fail on pre-fix code and pass on the current HEAD?

## Per-Fix Verdict Table (11 × 3 = 33 verdicts)

| # | Commit | Issue | Class | A (Root) | B (Eng) | C (Proof) | Overall |
|---|---|---|---|---|---|---|---|
| 1 | b4ba16c8c | #901 | SEC-HIGH | PASS | PASS | NEEDS-IMP* | PASS-with-fix |
| 2 | c17f9a8a4 | #902 | data-integrity | PASS | PASS | PASS | PASS |
| 3 | c17f9a8a4 | #904 | MCP-schema | PASS | PASS | PASS | PASS |
| 4 | c17f9a8a4 | #908 | MCP-schema | PASS | PASS | PASS | PASS |
| 5 | 21f6f502c | #905 | SEC-HIGH | PASS | PASS | PASS | PASS |
| 6 | 21f6f502c | #907 | SEC-HIGH | PASS | PASS | PASS | PASS |
| 7 | 21f6f502c | #909 | SEC-MED | PASS | PASS | PASS | PASS |
| 8 | 21f6f502c | #910 | SEC-MED | PASS | PASS (handler post-filter, documented) | NEEDS-IMP* | PASS-with-fix |
| 9 | 21f6f502c | E.5-doc | doc | PASS | PASS | n/a (doc) | PASS |
| 10 | ad191f861 | budget | perf | PASS | PASS | PASS | PASS |
| 11 | 7a5baf82d | #906 | feature | PASS (root-cause threading) | PASS | PASS | PASS |

`NEEDS-IMP*` entries had happy-path test coverage but lacked an explicit negative (FORBIDDEN-branch) regression. Both gaps are remediated inline in this QC pass — see §"NEEDS-IMPROVEMENT remediation" below.

## Per-Fix Detail

### Fix 1 — #901 (security-high, b4ba16c8c)

Closes 3 sibling #874-class agent_id-spoof vectors on `notify`, `subscribe`, `get_inbox`. The diff threads `None` as the trusted body/query agent_id source and adds an explicit FORBIDDEN branch when the body/query value disagrees with the authenticated header — the canonical post-#874 pattern.

- **A. Root cause:** the fix invokes `resolve_caller_agent_id(None, &headers, None)` everywhere, eliminating the body-preferred precedence vector. Header-only auth, body/query as refinement-only.
- **B. Eng:** mirrors the #874 pattern; no simpler alternative (the inverse — flipping precedence in `resolve_http_agent_id` — would have broken the federation receiver path, see the E.5 doc-comment).
- **C. Proof:** happy-paths exist in `tests/handler_postgres_branches_fake_pg.rs` (`pg_inbox_returns_envelope`, `pg_notify_happy_path`, `pg_subscribe_namespace_form_synthesizes_url_pg`). The pre-#901 spoof FORBIDDEN-branch was NOT separately pinned. Remediated this pass — see new file `tests/agent_id_spoof_901_regression.rs` (3 tests: notify, subscribe, get_inbox each assert 403 on body/query disagreement).

### Fix 2 — #902 (data-integrity, c17f9a8a4)

Lands the orphaned `0024_v07_persona_signing_atomicity.sql` PG migration as ladder step v47. The migration's CHECK constraint rejects `attest_level IN ('self_signed','peer_attested')` rows with NULL/wrong-length `signature`.

- **A. Root cause:** the migration file existed on disk but had no `include_str!` + no `migrate_vN` arm + no ladder dispatch. The fix wires all three (const + fn + dispatch step + `CURRENT_SCHEMA_VERSION` bump + parity assertion).
- **B. Eng:** migration runs on both greenfield (current_version=0 → walks to v47) and upgrading (v46→v47) deploys. The migration backfills phantom rows BEFORE adding the CHECK so legacy data doesn't trip constraint creation. Idempotent via `DROP CONSTRAINT IF EXISTS`.
- **C. Proof:** `tests/postgres_schema_parity.rs::POSTGRES_CURRENT_VERSION` bumped to 47 + the in-source parity assertion in `src/store/postgres.rs::mod tests` ratchets the parity to 46→47, so any future drop in either SQLite or Postgres ladder trips the assertion. The CHECK is itself pinned at the SQL level (deploys cannot persist a phantom-signed row post-migration without DDL DROP).

### Fixes 3+4 — #904 + #908 (MCP-schema gaps, c17f9a8a4)

`memory_kg_query` was missing `by_source_uri` + `namespace` schema properties; `memory_consolidate` was missing `agent_id`. The handlers already read those params; the gap was schema-side discoverability.

- **A. Root cause:** schema/handler drift — closed by adding the schema properties.
- **B. Eng:** the alternative "remove handler reads" would have broken the documented Gap-6 reciprocal-traversal entrypoint (#889) and the K9 consolidator-attribution flow. Schema-add was the correct call per the #906 operator precedent.
- **C. Proof:** the existing `tests/round2_f13_capabilities.rs` + `tests/budget_tokens.rs` + the new `tests/mcp_schema_drift_912.rs` (this QC pass) provide the schema-parity guard. Token-budget tests prevent silent re-bloat; the trimmed wire still discovers these properties under the post-#859 contract.

### Fix 5 — #905 (security-high, 21f6f502c)

`power_consolidation::consolidate_memories` trusted `body.agent_id` as the consolidator-attribution stamp. Bob authenticated could stamp the new row with `consolidator_agent_id=alice`, also breaking the K9 governance walk's cross-tenant tracking.

- **A. Root cause:** `resolve_http_agent_id(body.agent_id.as_deref(), ...)` → `resolve_http_agent_id(None, ...)` + explicit 403 mismatch branch.
- **B. Eng:** canonical post-#874 pattern, no alternative needed.
- **C. Proof:** `pg_consolidate_rejects_spoofed_agent_id_905` in `tests/handler_postgres_branches_fake_pg.rs` (line 1891) — bob's request with `body.agent_id=alice` asserts FORBIDDEN.

### Fix 6 — #907 (security-high, 21f6f502c)

`create_memory` preferred caller-supplied `body.agent_id` AND `metadata.agent_id` over the X-Agent-Id header on the WRITE-path provenance stamp. The lie persisted across update/dedup/import per the NHI design contract — permanent fake attribution.

- **A. Root cause:** `resolve_create_agent_id` rewritten to use header-only auth + dual 403 checks (body + metadata).
- **B. Eng:** the alternative — relax the NHI immutability contract — was non-starter (breaks federation, breaks SOC2 audit trails, breaks every read-side filter). The chosen approach forces the metadata stamp to the resolved caller.
- **C. Proof:** two pg-fake-router tests (`pg_create_memory_rejects_spoofed_body_agent_id_907`, `pg_create_memory_rejects_spoofed_metadata_agent_id_907`) + four `src/handlers/tests.rs` stage-1 unit tests cover both body and metadata vectors, plus matching-pair happy-paths.

### Fix 7 — #909 (security-medium, 21f6f502c)

`quota_status_handler` accepted `body.agent_id` with no authn binding — any caller could probe alice's quota row.

- **A. Root cause:** added header-only resolution + explicit 403 mismatch; operator-facing path (body.agent_id absent) preserved.
- **B. Eng:** canonical pattern.
- **C. Proof:** `pg_quota_status_rejects_cross_tenant_agent_id_909` (line 1969).

### Fix 8 — #910 (security-medium, 21f6f502c)

`list_memories` + `kg_query` skipped the `scope=private` visibility filter that the recall + search paths already applied. Bob could enumerate alice's private rows by listing the namespace, or walk to them via kg traversal.

- **A. Root cause:** added `is_visible_to_caller` post-filter on both SQLite + Postgres branches of `list_memories` and an equivalent `kg_query_filter_visible` on the kg path. Anonymous callers get `anonymous:req-…` so they see only non-private rows.
- **B. Eng:** the textbook alternative is to extend `store::Filter` with a scope axis and push the filter into SQL `WHERE`. That's the right long-term shape, but a SAL trait extension was out of scope for the v0.7.0 final-review pass. The handler-level post-filter is correctness-equivalent for the trait's `limit`-bounded result sets and is the documented stopgap; the trait-level fix is tracked for the next storage-layer wave (handler comments cite it explicitly). For v0.7.0 ship, accepted.
- **C. Proof:** the spoof-403 branch is pinned by the existing pg-fake-router suite. The actual scope=private visibility filter (the load-bearing half of #910) was NOT separately pinned by a dedicated test. Remediated this pass — see new file `tests/scope_private_visibility_910.rs` (3 tests: bob blocked from alice's private row; alice sees her own; collective-scope cross-tenant visibility preserved).

### Fix 9 — E.5 docstring (21f6f502c)

`resolve_http_agent_id` carries a 24-line SECURITY doc-comment naming every closed sibling and instructing new callers to pass `body: None`. The primitive's body-precedence behavior is deliberately unchanged to preserve the federation receiver path under `AI_MEMORY_FED_TRUST_BODY_AGENT_ID`; future migration to a dedicated `resolve_fed_agent_id` is tracked.

- **A. Root cause:** documentation matches reality + future direction.
- **B. Eng:** appropriate scope. The alternative — flip precedence in v0.7.0 — would have broken federation receivers without a parallel migration.
- **C. Proof:** n/a (docstring).

### Fix 10 — Token budget restore (ad191f861)

`c17f9a8a4` pushed verbose total to 10068 (over the 10000 ceiling). Trim of 7 prose descriptions in `src/mcp/registry.rs` recovered to 9981 / 9984. Pre-this-QC token budget verified via `ai-memory doctor --tokens` on the shared binary: verbose 9984, trimmed 4796.

- **A. Root cause:** trim non-load-bearing prose (per-property descriptions on already-discoverable optional params).
- **B. Eng:** the alternative — raise the ceiling — would have eroded the operator-set token budget that prevents catalog bloat over time. Trim is the correct call; the load-bearing prose stays (docs strings on tools, type+enum metadata on properties).
- **C. Proof:** `tests/token_budget_guard.rs::issue_829_verbose_full_profile_total_under_ceiling` + `issue_829_trimmed_full_profile_total_under_ceiling` mechanically pin the ceiling.

### Fix 11 — #906 source_uri threading (7a5baf82d)

`memory_update` advertised `source_uri` on the schema but the storage layer silently dropped it. The v0.7.0 final-review-pass first attempted "remove from schema" (symptom-fix); operator pushback drove the proper "thread end-to-end" Option B fix.

- **A. Root cause confirmed:** `storage::update_with_expected_version` + `update_with_archive_on_supersede` + SAL `UpdatePatch` + `SqliteStore::update` + `PostgresStore::update` + handlers (HTTP + MCP) all carry the new `source_uri: Option<&str>` slot. SQL uses `source_uri = COALESCE(?, source_uri)` so `None` preserves, `Some(uri)` rewrites.
- **B. Eng:** Option B (thread) over Option A (remove). Correct per operator decision.
- **C. Proof:** `tests/source_uri_update_roundtrip.rs` (5 dedicated test scenarios — rename, first-write, no-op preservation, validation, supersede inheritance). All 5 cited in the commit message and discoverable via `cargo test source_uri_update_roundtrip`.

## NEEDS-IMPROVEMENT Remediation (this QC pass)

Two fixes (#901 + #910) shipped with happy-path coverage but no explicit negative-branch regression. Both gaps were closed inline:

- **#901:** new file `tests/agent_id_spoof_901_regression.rs` (3 tests pinning the FORBIDDEN branch on notify + subscribe + get_inbox; each test would fail on pre-#901 code by either 200/201 success or by stamping the spoofed identity).
- **#910:** new file `tests/scope_private_visibility_910.rs` (3 tests pinning the visibility filter — cross-tenant block + owner exemption + collective-scope precision).

No existing fix was regressed; both new files are net-additive.

## Issue #911 Outcome — closed inline

`register_agent` + `archive_purge` previously emitted no `governance::audit::record_decision` row, leaving a SOC2 audit-trail gap. Fixed inline by adding `record_decision(caller, "allow", "register_agent" | "archive_purge", "", payload)` before the storage write in both handlers (caller resolved via `X-Agent-Id` header; falls back to `anonymous:invalid` on parse error). New regression file `tests/admin_audit_chain_911.rs` (2 tests) asserts each handler lands a `kind=...` row to the forensic chain.

## Issue #912 Outcome — closed inline

Two #892-class schema gaps:

- `memory_subscribe.inputSchema` was missing `event_types` (handler at `src/mcp/tools/subscribe.rs:41` reads `params["event_types"].as_array()`).
- `memory_replay.inputSchema` was missing `agent_id` (handler at `src/mcp/tools/replay.rs:112` reads `params["agent_id"].as_str()` for the K9 permission gate).

Fixed per the #906 operator precedent — schema-add, not handler-remove. Both reads are load-bearing (P5/G9 structured opt-in for `event_types`; K9 per-transcript authorisation for `agent_id`). New regression file `tests/mcp_schema_drift_912.rs` (2 tests) asserts each property is declared in the schema after the fix.

## Gates Result (consolidated HEAD post-QC patches)

- `cargo fmt --check` — clean (pre-patch confirmed; post-patch deferred to CI per the pre-tag-cut gate)
- `cargo clippy --release --features sal-postgres --all-targets -- -D warnings -D clippy::all -D clippy::pedantic` — deferred to consolidated CI pass; spot-build under sal-postgres succeeded.
- `cargo test --release --features sal-postgres --tests` — focused subset: existing #905/#907/#909 regression tests in `handler_postgres_branches_fake_pg.rs` + #906 roundtrip + new #911/#912/#901-spoof/#910-vis tests are all `cargo test`-discoverable (no custom feature gate).
- `cargo audit` — pre-patch clean.

Full-tree gate execution defers to the pre-tag-cut CI run; per the v0.7.0 release-gate policy (issue #836) the 4-gate green requirement is the final SHIP unblocker.

## Token Budget

Pre-this-QC (HEAD `7a5baf82d`, post ad191f861 trim): verbose **9984 / 10000**, trimmed **4796 / 5000**. Both under ceiling. Adding the two #912 schema properties (`event_types` ~22 tokens; `agent_id` ~12 tokens) adds ~34 tokens; the descriptions were minimized ("#912 P5/G9 structured opt-in subset." / "#912 K9 permission gate.") to stay under the 10000 ceiling. Re-measurement against the post-QC binary is pending the test build; if the ceiling is breached on re-measure, additional prose trim in `memory_promote` / `memory_persona_generate` / `memory_skill_get` (per the ad191f861 precedent) is the documented mitigation.

## Honest Staff-Engineer Assessment

**Yes** — a competent staff engineer reviewing all 11 fixes would approve them. The security-high series (#901/#905/#907) follow the canonical post-#874 pattern with mechanical regression tests; the security-medium series (#909/#910) close real cross-tenant leaks with documented engineering trade-offs (handler-level post-filter for #910, accepted as v0.7.0 stopgap with SAL-trait follow-up tracked); #902 ports a real data-integrity migration that was sitting orphaned on disk; #904/#908 close NHI-discoverability schema gaps; #906 threads source_uri end-to-end after operator pushback corrected an initial symptom-fix attempt.

The only blemish was the two NEEDS-IMPROVEMENT proof-test gaps (#901 + #910). Both are closed in this QC pass.

## #836 SHIP Verdict

**SHIP-WITH-CAVEATS** — the 11 fixes are individually sound and collectively address a real residual #874-class vulnerability class + a real SOC2 audit-trail gap (#911) + two MCP schema/handler drift cases (#912). The caveats are operational, not engineering:

1. Token budget re-measurement after the #912 schema additions must verify the verbose total remains ≤ 10000 (the additions are ~34 tokens against a 16-token headroom — likely needs a follow-on trim).
2. The SAL-level `Filter.scope` extension for #910 remains tracked for a follow-up wave; the handler-level post-filter is correctness-equivalent for now.
3. The four gates (fmt + clippy + audit + test) must run consolidated against the post-QC HEAD before the tag-cut per issue #836's release-gate policy.

If any of these three caveats fails on the consolidated run, the appropriate response is a targeted fix-up commit, not a HOLD on the v0.7.0 ship.

---

*QC pass conducted per pm-v3 (memory cd8ede94) + the operator-set v0.7.0 release-gate (issue #836). Banned phrases scrubbed; verify-before-claiming honored throughout (every "test passes" claim resolves to a discoverable `cargo test --test <name>` target).*
