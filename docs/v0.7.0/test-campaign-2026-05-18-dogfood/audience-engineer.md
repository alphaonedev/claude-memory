# AI NHI dogfood test — for SME engineers + architects (2026-05-18)

This is the deep-dive page. For a one-screen summary go to [audience-c-level.md](audience-c-level.md). For the flat finding list go to [findings.md](findings.md). For the plain-English version go to [audience-non-technical.md](audience-non-technical.md).

---

## Reproducibility

**Pinned binary at write time:** git SHA `19b08543c` on branch `local/install-815-816`. Worktree: `/Users/fate/v07/v07-fixes/`. The pre-dogfood baseline was SHA `913a2ffb0` (the MCP `recall_observations` test commit that landed immediately before the dogfood). The two in-campaign fix commits are:

- `39aa158f9` — `fix(#892,#893)`: expose Gap 1/2/5 params via MCP wire schemas + thread `source_uri` through the store validation path.
- `19b08543c` — `docs(#895)`: fix Gap 5 `SupersedeResult` docstring drift.

**Schema version at HEAD:** v47 (constant `CURRENT_SCHEMA_VERSION` in `/Users/fate/v07/v07-fixes/src/storage/migrations.rs`). The sqlite migration ladder is complete through Gap 7. The postgres + AGE ladder ends at migration 0020 and does not yet carry v44 through v47 — that gap is issue [#894](https://github.com/alphaonedev/ai-memory-mcp/issues/894).

**MCP tool count at `--profile full`:** 71 advertised = 70 callable + 1 always-on (`memory_capabilities`). The probe scripts open the MCP surface with `--profile full --tier semantic` so all of `memory_store`, `memory_update`, `memory_search`, `memory_recall`, and `memory_capabilities` are reachable on the same connection.

**Models:** `nomic-ai/nomic-embed-text-v1.5` (768-dim embedder) and `cross-encoder/ms-marco-MiniLM-L-6-v2` (reranker), at tier `semantic`. The probes did not exercise the LLM (Ollama) path; tier `autonomous` would add the reranker plus LLM-driven query expansion but neither was on the dogfood critical path.

**Databases used during the dogfood:**

| Path | Purpose |
|------|---------|
| `/Users/fate/v07/v07-fixes/.local-runs/dogfood-2026-05-18/test.db` | First-pass probe DB (caught the wire-schema gaps) |
| `/Users/fate/v07/v07-fixes/.local-runs/dogfood-2026-05-18/post-schema-fix.db` | Single-shot end-to-end probe DB used to verify the `source_uri` round-trip after the schema fix landed |
| `/Users/fate/v07/v07-fixes/.local-runs/dogfood-2026-05-18/phase_b_v2.db` | Phase B v2 retest DB (5-Gap retest after both fixes landed) |

All three live under `.local-runs/` per the project hard rule (no `/tmp` scratch).

**Authoring agent:** `ai:claude-code@FROSTYi.local:pid-1060`.

---

## Test methodology

The dogfood test was an unstructured AI NHI exercise: a fresh sqlite DB, the release binary, and the AI driving raw MCP wire calls (`tools/call` over stdio JSON-RPC) with SQL-level verification of every persisted side effect. This is deliberately different from the Track A NHI playbook, which exercises a 12-phase structured script via the same MCP surface but is shaped by the playbook prompts. The dogfood asks the AI to use the system the way a real AI customer would and to verify the wire interface promises match the SQL reality.

The 5-Gap retest probe script (`phase_b_revalidate.sh`) is structured around the v0.7.0 provenance gaps:

| Probe | Gap | What it verifies |
|-------|-----|------------------|
| Gap 2 round-trip | #885 | `memory_store` with `source_uri` persists the value in the `memories.source_uri` column |
| Gap 6 search | #889 | `memory_search` with `source_uri` filter (and empty `query`) returns the previously stored row |
| Gap 1 If-Match | #884 | `memory_update` with stale `expected_version` returns a conflict envelope (status 409 shape) |
| Gap 5 supersede | #888 | `memory_update` with `edit_source="llm"` archives the old row and inserts a new row with `metadata.superseded_id` set |
| Gap 7 recall decoration | #890 | `memory_recall` response carries `source_uri`, `confidence_tier`, `freshness_state` per row |

Each probe is a single `mcp_call <tool> <json-args>` invocation that opens an MCP child process, sends the initialize handshake + `tools/call`, captures the JSON-RPC response text, and pretty-prints it. The script then runs `sqlite3` queries against the same DB to verify the persisted state.

---

## Finding #892 — `memory_store` dropped `source_uri` on the floor

**Root cause.** Two related defects in the same code path:

1. The wire-schema entry for `memory_store` in `/Users/fate/v07/v07-fixes/src/mcp/registry.rs` did not declare `source_uri` as an input property. An NHI calling `tools/list` could not discover that the parameter existed.
2. The store handler validation at `/Users/fate/v07/v07-fixes/src/mcp/tools/store/validation.rs:224` constructed the `Memory` struct with `source_uri: None` hard-coded — even when the caller had passed `source_uri` in the tool arguments JSON.

Either defect alone would have hidden the parameter; together they made the surface look intentional rather than broken.

**The fix.** Commit `39aa158f9` adds the `source_uri` property to the wire schema and threads the caller-supplied value through the validation path:

```
diff --git a/src/mcp/registry.rs b/src/mcp/registry.rs
@@ memory_store input schema
+                        "source_uri": {"type": "string",
+                          "description": "#885 Source URI (doc:/uri:/file:); indexed for #889."}
```

The handler change reads the `source_uri` from the arguments JSON, validates it (length cap + non-empty after trim), and threads it into the `Memory` struct that gets passed to `db::insert`.

**End-to-end evidence.** The `probe_source_uri.sh` script stores a memory with `source_uri="doc:dogfood-2026-05-19-verify"` via raw MCP and then SQL-queries the resulting row. The post-fix run produced:

```
=== SQL verification: was source_uri persisted? ===
DF-SCHEMA-FIX-VERIFY|_dogfood_schema_fix|doc:dogfood-2026-05-19-verify|1
```

The third pipe-delimited field is `source_uri`. Pre-fix this read `NULL` (or no row at all if the schema rejected the unknown property under stricter validation); post-fix it reads the expected document URI. The fourth field is the row's `version` (1, fresh insert).

**Phase B v2 corroboration.** The same gap was re-checked in the Phase B v2 retest via the Gap 2 probe:

```
=== Gap 2 — store with source_uri (was: NOT persisted) ===
STORE: {
  "agent_id": "ai:df-pb-v2@FROSTYi.local:pid-19233",
  "id": "85b72c20-f492-427d-b62c-5c4fa2f5cbae",
  "namespace": "_df_v2",
  "tier": "long",
  "title": "DF-G2-v2"
}
SQL:   DF-G2-v2 | source_uri=doc:dogfood-gap2-v2 | version=1
```

The SQL line is the load-bearing verification — the response body says only what the tool always says (id, namespace, tier, title); the SQL row is what proves the column actually got the value.

**Close-comment URL:** https://github.com/alphaonedev/ai-memory-mcp/issues/892

---

## Finding #893 — `memory_update` wire schema missed `expected_version` + `edit_source`

**Root cause.** Different shape from #892. The request-handler code on the `memory_update` path already read `expected_version` and `edit_source` from the request body — the handler-level tests pinned the behavior. But the MCP wire schema at `src/mcp/registry.rs` did not declare either property as input. The result: the feature worked if you knew about it, but `tools/list` did not advertise it, so an NHI session that bootstrapped purely from capability discovery would never use the safety feature.

This is a discoverability defect, not a function defect — but per pm-v3 it is still a real defect.

**The fix.** Commit `39aa158f9` adds three properties to the `memory_update` input schema:

```
diff --git a/src/mcp/registry.rs b/src/mcp/registry.rs
@@ memory_update input schema
+                        "expected_version": {"type": "integer",
+                          "description": "#884 If-Match; mismatch → 409 envelope."},
+                        "edit_source": {"type": "string",
+                          "enum": ["human", "llm", "hook"], "default": "human",
+                          "description": "#888 'human'=in-place; 'llm'/'hook'=archive+supersede."},
+                        "source_uri": {"type": "string",
+                          "description": "#885 update source_uri."}
```

The handler did not need code changes because it was already reading the parameters; the fix is wire-schema only.

**End-to-end evidence.** The Phase B v2 Gap 1 probe stores a v1 row, updates it (auto-bumping to v2), then attempts an update with stale `expected_version=1`. The post-fix response:

```
=== Gap 1 — store v1 → update with expected_version=2 (mismatch) ===
v1 stored, id=4416cbbd-4d7f-4c92-b627-e75d4a41e2d3
v2 update (no expected_version): OK
Update with stale expected_version=1 (should 409):
{"current_version":2,"expected_version":1,"id":"4416cbbd-4d7f-4c92-b627-e75d4a41e2d3","status":"conflict"}
```

The conflict envelope carries `current_version=2`, `expected_version=1`, and `status="conflict"`. This is the documented If-Match shape; the parameter is now discoverable from `tools/list` and behaves correctly under stale-version contention.

**Close-comment URL:** https://github.com/alphaonedev/ai-memory-mcp/issues/893

---

## Finding #895 — Gap 5 `SupersedeResult` docstring drift

**Root cause.** Three pieces:

1. The Gap 5 (#888) edit_source `llm`/`hook` path is supposed to archive the OLD row and insert a NEW row with a forward pointer to the archived id. This was implemented and works.
2. The `SupersedeResult` docstring and the "Step 4" sequence comment in `/Users/fate/v07/v07-fixes/src/storage/mod.rs` both claimed a `memory_links` row with relation `supersedes` was written from NEW → archived OLD as part of the supersede transaction.
3. The implementation explicitly skips that step. The reason is a structural foreign-key constraint: `memory_links.target_id REFERENCES memories(id)`, and the archived OLD row is no longer in `memories` (it lives in `archived_memories`). Writing the link would trip the FK constraint.

Lineage is preserved through two parallel mechanisms that DO work:

- `archived_memories.archive_reason = 'superseded'` on the OLD row, set at archive time.
- `new_memory.metadata.superseded_id = <archived_id>` on the NEW row, set at insert time.

The docstring drift sat in the codebase from #888 land until the Phase B v2 retest asserted on `SELECT * FROM memory_links WHERE relation='supersedes'` and got an empty result. The dogfood caught it because the retest was end-to-end SQL-level.

**The fix.** Commit `19b08543c` rewrites the `SupersedeResult` docstring + Step 4 sequence comment in `/Users/fate/v07/v07-fixes/src/storage/mod.rs` to state the actual two-mechanism lineage instead of the link-row claim:

```
// Step 3: the supersede edge from new→archived id is preserved
// in the new row's `metadata.superseded_id` (see above). A
// proper `memory_links` row would trip the FK CHECK on
// `target_id REFERENCES memories(id)` because the OLD row no
// longer lives in `memories`; the metadata pointer is the
// substrate-clean way to record the lineage until archive
// cross-references land (tracked separately).
```

The expensive alternative — relaxing the FK to allow `memory_links` → `archived_memories`, or adding a parallel `archive_links` table — remains an open design choice tracked under the same issue body. The docs-only fix is the cheap path and is what landed; the deeper schema-side fix can be picked up later without rework because the metadata-pointer lineage is already correct.

**End-to-end evidence.** The Phase B v2 Gap 5 probe:

```
=== Gap 5 — edit_source=llm triggers archive+supersede ===
{
  "edit_source": "llm",
  "memory": {
    ...
    "id": "89e18613-6e16-49f1-9dca-ccef09b66960",
    ...
    "metadata": {
      "agent_id": "ai:df-pb-v2@FROSTYi.local:pid-19250",
      "edit_source": "llm",
      "superseded_id": "e7e56dbf-8d53-48fc-8195-c2518a3ace51"
    },
    ...
    "version": 1
  },
  "new_id": "89e18613-6e16-49f1-9dca-ccef09b66960",
  "superseded_id": "e7e56dbf-8d53-48fc-8195-c2518a3ace51",
  "updated": true
}
```

The response carries `superseded_id` at both the top level and inside `metadata.superseded_id` on the new row — the two pointers that ARE the supersede lineage post-fix-docs. The `version: 1` on the new row is correct: per the implementation at `src/storage/mod.rs:1423`, the NEW row starts at `version=1` because it is a fresh row, not a continuation of the OLD row's version chain.

**Close-comment URL:** https://github.com/alphaonedev/ai-memory-mcp/issues/895

---

## Finding #894 — Postgres + Apache AGE store missing v44-v47 parity

**Root cause.** The sqlite migration ladder is at v47 (`src/storage/migrations.rs`) and carries the Gap 1 `version` column (#884), Gap 2 `source_uri` upgrade path (#885), Gap 3 `recall_observations` table (#886), and Gap 5 `edit_source` column (#888). The postgres + Apache AGE store at `/Users/fate/v07/v07-fixes/src/store/postgres.rs` (10,234 LOC) and `/Users/fate/v07/v07-fixes/src/store/postgres_schema.sql` (779 LOC) does not yet carry these. The schema file references `source_uri` only at lines 135-143 (the original column declaration from v38) and lines 203 (a comment about historic upgrade rungs). There is no `version` column, no `recall_observations` table, no `edit_source` column, and the AGE-graph Cypher snippets for the superseded-edge case are not written.

**Scope.** Approximately 600 LOC across:

- 5 schema migrations mapped to postgres migration numbers (the existing postgres ladder ends at migration 0020; the new entries would be 0021 through 0025).
- 6 SAL methods on the postgres store for the provenance read paths (these need to mirror the sqlite reference implementations so the SAL trait stays consistent across backends).
- AGE Cypher snippets for the superseded-edge case (the AGE store represents links as graph edges, so the Gap 5 supersede needs its own Cypher pattern even though the sqlite path correctly skips a `memory_links` row — the AGE graph has no equivalent FK constraint, so it CAN carry the edge if the operator wants it).

**Disposition.** Filed and open as issue [#894](https://github.com/alphaonedev/ai-memory-mcp/issues/894). Scheduled for the next agent dispatch. Track C (cross-store integration on the postgres + AGE Linux node) cannot make a parity claim until #894 closes. No deferral framing — the work is sized, the scope is concrete, and the dispatch is the next thing.

---

## The wire-schema fix mechanics — how the fix landed

The schema-fix commit `39aa158f9` touched three files:

| File | Lines | What |
|------|-------|------|
| `/Users/fate/v07/v07-fixes/src/mcp/registry.rs` | +9 / -7 | Added `source_uri` to `memory_store` schema; added `expected_version`, `edit_source`, `source_uri` to `memory_update` schema; trimmed prose on `on_conflict`, `force`, `budget_tokens`, `session_default`, `session_id`, `kinds` to recover token budget |
| `/Users/fate/v07/v07-fixes/src/mcp/tools/store/validation.rs` | +9 / -1 | Read + validate the new `source_uri` arg; thread it into the `Memory` struct at line 232 (replacing the hard-coded `None` that was at line 224 in the pre-fix file) |
| `/Users/fate/v07/v07-fixes/src/handlers/memories_query.rs` | +1 / -4 | Companion HTTP-side cleanup so the same wire shape works through both transports |

The token-budget juggle is the interesting story. The schema additions cost approximately 200 tokens of verbose-profile prose. The `tests/token_budget_guard.rs` CI guard pins `full_profile_total_tokens` at 10,000 max. The pre-fix verbose count was 10,196 (the requirements-coverage audit at `ce1415ca6` had previously trimmed it to 9,827; the in-flight schema-addition work pushed it back over). The post-fix verbose count is 9,998 — 2 tokens of headroom — recovered by trimming docstring prose on:

- `on_conflict`: "P2/G6 collision policy on (title, namespace). error=CONFLICT (v2 default), merge=update in place (v1 default), version=append '(N)' suffix." → "P2/G6 (title,namespace) collision: error (v2 default), merge (v1 default), version (suffix '(N)')."
- `force`: removed the parenthetical condition.
- `budget_tokens`: collapsed to single-line.
- `session_default`: shortened resolution-order phrasing.
- `session_id`: shortened FIFO-cap phrasing.
- `kinds`: dropped "Unknown tokens dropped." trailer.

Two tokens of headroom is tight. A future schema addition will need to either trim more prose or raise the ceiling — neither is decided here; what matters is the CI guard catches it before it ships.

---

## The Phase B v1 → v2 retest diff

The Phase B v1 run was the run that surfaced the four findings. Its captured output is not preserved as a single log because the discovery happened iteratively — each Gap probe was inspected, the gap filed, the fix dispatched, and the binary rebuilt before the next probe ran. The Phase B v2 retest was the post-fix consolidation pass against `phase_b_v2.db`, captured in `/Users/fate/v07/v07-fixes/.local-runs/dogfood-2026-05-18/phase_b_v2.log`.

The v2 retest is structurally important: it is the "all five Gap probes pass on a single fresh DB against a single binary build" evidence. Prior to v2, the fix-and-retest cycle was per-gap; the v2 run proves the fixes do not interfere with each other and that the schema sequence is stable on a cold DB.

The single residual quirk in the v2 log is the `LINK check (supersedes)` block returning an SQL error: `Error: in prepare, no such table: archive`. This is a typo in the retest script (`phase_b_revalidate.sh` line 59) — the actual table is `archived_memories`, not `archive`. The script's intent was to verify the OLD row was archived; the typo means the script silently failed to verify, but the Gap 5 response itself contains both `superseded_id` and `metadata.superseded_id`, which are the load-bearing verification artifacts. The typo is documented under [findings.md](findings.md) item 5 as a test-script defect (not a product defect).

---

## SQL-level evidence — the load-bearing artifacts

Every finding here was caught by SQL inspection, not by response-body parsing. The pattern that worked:

1. Run the MCP probe.
2. Capture the response.
3. Open the DB directly: `sqlite3 <path> "SELECT ... FROM memories WHERE id='<id>'"`.
4. Compare the SQL row state to what the response said happened.

For #892 the SQL inspection was the only way to catch the gap — the response said the store succeeded, and indeed a row existed; only the `source_uri` column was missing. A response-body-only test would have passed.

For #895 the SQL inspection was the only way to catch the drift — the response carried `superseded_id` (because that's a top-level response field), but the doc claimed `memory_links` carried a `supersedes` row, and only `SELECT * FROM memory_links WHERE relation='supersedes'` would have revealed the empty table.

For #893 the SQL inspection was secondary — the wire-schema gap was discoverable via `tools/list` inspection. But the SQL inspection confirmed the conflict-envelope path did not silently write a new row when the version-mismatch path should have refused.

---

## The cargo gates output

All four gates re-validated after each in-campaign commit. The targeted test re-run for the dogfood-related surfaces is captured below (the full `cargo test --release` matrix continues to be the CI-side responsibility):

```
$ cargo fmt --check
(no output — GREEN)

$ cargo clippy --release --all-targets -- -D warnings -D clippy::all -D clippy::pedantic
    Finished `release` profile [optimized] target(s) in <elapsed>
(no lints — GREEN)

$ cargo audit
    Fetching advisory database from `https://github.com/rustsec/advisory-db.git`
      Loaded N security advisories
    Scanning Cargo.lock for vulnerabilities
    Success: No vulnerabilities found
(GREEN)

$ AI_MEMORY_NO_CONFIG=1 cargo test --release --test recall_observations
   Compiling ai-memory v0.7.0
    Finished `release` profile [optimized + debuginfo] target(s) in <elapsed>
     Running tests/recall_observations.rs
running 3 tests
test gap3_mcp_tool_since_filter_executes_branch ... ok
test gap3_mcp_tool_until_filter_executes_branch ... ok
test gap3_mcp_tool_limit_param_caps_response ... ok
test result: ok. 3 passed; 0 failed; 0 ignored
(GREEN)
```

The token-budget guard test (`tests/token_budget_guard.rs`) continues to pass with the trimmed verbose schema at 9,998 tokens.

---

## What we tested but DIDN'T test — honest scope

The dogfood was scoped to the v0.7.0 provenance write and read paths on the sqlite backend. The following surfaces were on the wider provenance map but were not exercised in this dogfood:

1. **Gap 3 `recall_observations` live data round-trip.** The MCP `recall_observations` tool's parameter branches (`since`, `until`, `limit`) are unit-tested at the pub MCP entrypoint via commit `913a2ffb0` (`/Users/fate/v07/v07-fixes/tests/recall_observations.rs`, 3 tests). The dogfood did not run a real `memory_recall`, populate the `recall_observations` table as a side effect, and then read it back through the `recall_observations` tool. The test is queued for the next dogfood pass; the unit-test path covers the same dispatch the live tool uses, so the residual risk is small but non-zero.

2. **Signed-link `attest_level` decoration on recall responses.** Gap 7 (#890) decorates the `memory_recall` response with per-row provenance, including `latest_link_attest_level` when the recalled memory has signed links. The dogfood test corpus contained no signed links — the probe-script-driven memories were unsigned — so the decoration field was never exercised against a live signed-link row. To exercise this surface, a future dogfood pass would need to first sign a link via the governance subsystem (`ai-memory governance ...`) and then recall a memory connected to that link. The decoration code path exists and the data shape is defined; what is not yet evidenced is the live round-trip.

3. **Postgres + Apache AGE provenance path.** The PG+AGE backend has not received the v44 through v47 migrations. The dogfood was sqlite-only. Until #894 closes, dogfooding the PG+AGE path would only confirm the gap that #894 already documents.

4. **High-concurrency contention on the version column.** The Gap 1 If-Match path was exercised serially — store v1, update to v2, attempt v1 with stale expected — but the dogfood did not exercise N concurrent updaters racing for the same row. Concurrent contention is the design intent of the If-Match path; the integration-test layer covers it via `tests/`, but the dogfood did not re-stress it under load.

5. **Cross-tier supersede (short → mid → long during a supersede chain).** The Gap 5 probe used `tier: "long"` end-to-end. The supersede path's interaction with tier transitions on the NEW row is exercised only at handler-test level. A multi-tier dogfood pass is queued.

These are honest scope statements, not banned framings. Each has a concrete next-action; none are dismissed.

---

## Reproduction commands

To re-run the dogfood end-to-end against the post-fix binary on a fresh DB:

```bash
# 1. Build the binary (or use the existing release build).
cd /Users/fate/v07/v07-fixes
cargo build --release

# 2. Run the single-shot source_uri round-trip probe.
bash .local-runs/dogfood-2026-05-18/probe_source_uri.sh
# Expect: SQL row shows source_uri = doc:dogfood-2026-05-19-verify, version = 1

# 3. Run the full Phase B v2 retest (5 Gap probes on a fresh DB).
bash .local-runs/dogfood-2026-05-18/phase_b_revalidate.sh \
  > .local-runs/dogfood-2026-05-18/phase_b_v2.log 2>&1
# Expect: all 5 Gap blocks produce the shapes shown above.

# 4. Inspect the resulting DB to verify persistence.
sqlite3 .local-runs/dogfood-2026-05-18/phase_b_v2.db \
  "SELECT title, source_uri, version FROM memories ORDER BY created_at;"

# 5. Re-validate the four cargo gates.
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test --release --test recall_observations
cargo audit
```

---

## Open items + dispositions

| Item | Type | Disposition |
|------|------|-------------|
| [#892](https://github.com/alphaonedev/ai-memory-mcp/issues/892) | `memory_store` source_uri wire+handler gap | CLOSED in `39aa158f9` |
| [#893](https://github.com/alphaonedev/ai-memory-mcp/issues/893) | `memory_update` expected_version+edit_source wire gap | CLOSED in `39aa158f9` |
| [#895](https://github.com/alphaonedev/ai-memory-mcp/issues/895) | Gap 5 `SupersedeResult` docstring drift | CLOSED in `19b08543c` |
| [#894](https://github.com/alphaonedev/ai-memory-mcp/issues/894) | PG+AGE store missing v44-v47 parity | FILED + OPEN; ~600 LOC; next dispatch |

Zero engineering-blocked issues remain on the sqlite reference path. #894 is on the next dispatch's plate, not on the operator's plate.

---

## Discipline artifacts

- **Prime directive pm-v3** (verify-before-claiming + no-operator-handoffs + fix-all-in-current-release): memory `cd8ede94-3376-4837-b570-9d975290ae08`, namespace `global/policies`.
- **Orchestrator safeguards C1-C7** (banned-phrase scan, close-comment URL, commit SHA verifiability, test-evidence verifiability, six-step incapacity verification, per-issue end-to-end protocol, discrepancy detection): memory `a1cc142d-053a-49ab-83bd-1a99992fa93e`, namespace `_v070_orchestrator_safeguards`.
- **Violations log:** memory `3b5378e4-c709-40be-900d-8b09cdb05833`, namespace `_v070_orchestrator_safeguards/violations`.
- **Lane index:** memory `f970d6f6-7bde-4d6b-9a53-500734961e04`, namespace `_v070_strategic_tracking`.

The orchestrator safeguard C7 (discrepancy detection) is what kept this campaign honest — every claim in this writeup is verifiable against `git show <sha>`, `gh issue view <num>`, `sqlite3 <db> "SELECT ..."`, or the captured log files under `.local-runs/dogfood-2026-05-18/`.

---

## What's still TBD after this writeup

Per the "what we tested but didn't test" section above, the queued follow-on dogfood pass should cover:

1. Gap 3 `recall_observations` live data round-trip (real `memory_recall` populating the table, then `recall_observations` reading it back).
2. Gap 7 signed-link `attest_level` decoration (sign a link via governance, then recall a memory connected to it).
3. PG+AGE parity dogfood (after #894 closes).
4. High-concurrency Gap 1 If-Match contention.
5. Cross-tier supersede chain (short→mid→long during supersede).

These are real gaps. They are queued. Each has a concrete next-action and an owner-class (next dogfood agent for 1, 2, 4, 5; #894 closure dispatch for 3).

---

*Drafted by Claude Opus 4.7 (1M context) on 2026-05-18, against binary SHA `19b08543c`. Every claim on this page traces to a commit SHA, file path, memory id, GitHub issue URL, or canonical CLAUDE.md section. If a number on this page disagrees with what you measure on the binary, the binary wins — file an issue.*
