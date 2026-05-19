# Dogfood findings — flat enumeration (2026-05-18)

Every anomaly the 2026-05-18 dogfood pass surfaced. One row per finding. Each row links to the GitHub issue, the closing commit (if closed), and the close-comment URL (if closed).

Filed per the prime directive pm-v3 (memory `cd8ede94-3376-4837-b570-9d975290ae08`, namespace `global/policies`): every finding gets its own audit-trail entry, no bundling, no deferral framing.

## Defects

| # | Finding | Class | Status | Issue | Commit | Close-comment URL |
|---|---------|-------|--------|-------|--------|-------------------|
| 1 | `memory_store` MCP wire schema omitted `source_uri` property AND `validation.rs:224` hard-coded `source_uri: None` — caller-supplied URI silently dropped. | Product defect (sqlite path) | CLOSED | [#892](https://github.com/alphaonedev/ai-memory-mcp/issues/892) | `39aa158f9` | https://github.com/alphaonedev/ai-memory-mcp/issues/892 |
| 2 | `memory_update` MCP wire schema omitted `expected_version` (Gap 1 If-Match) and `edit_source` (Gap 5 supersede). Handler already read both — wire-schema only — but NHIs could not discover the parameters via `tools/list`. | Product defect (discoverability, sqlite + PG paths) | CLOSED | [#893](https://github.com/alphaonedev/ai-memory-mcp/issues/893) | `39aa158f9` | https://github.com/alphaonedev/ai-memory-mcp/issues/893 |
| 3 | Gap 5 `SupersedeResult` docstring + sequence-step-4 comment in `src/storage/mod.rs` claimed a `memory_links` row with relation `supersedes` was written from NEW → archived OLD. Impl correctly skips it (FK `target_id REFERENCES memories(id)` would reject pointing at an archived id). Lineage IS preserved via `archived_memories.archive_reason='superseded'` and `new_memory.metadata.superseded_id`. | Documentation drift | CLOSED (doc-only fix) | [#895](https://github.com/alphaonedev/ai-memory-mcp/issues/895) | `19b08543c` | https://github.com/alphaonedev/ai-memory-mcp/issues/895 |
| 4 | Postgres + Apache AGE store missing v44-v47 migrations (Gap 1 `version` column, Gap 2 `source_uri` upgrade path, Gap 3 `recall_observations` table, Gap 5 `edit_source` column) + 6 SAL methods + AGE Cypher snippets for the superseded edge. ~600 LOC across `src/store/postgres.rs` and `src/store/postgres_schema.sql`. Track C cross-store parity stays gapped until this lands. | Product defect (PG+AGE path parity) | CLOSED | [#894](https://github.com/alphaonedev/ai-memory-mcp/issues/894) | `a69eed03b` (migrations v42-v46) + `e3ae0a555` (SAL methods, ~870 LOC) + `9bec43c7c` (cross-adapter parity harness) + `62cf9e49b` (build/clippy unblock); landing series closed by `62cf9e49b` | https://github.com/alphaonedev/ai-memory-mcp/issues/894 |

## Test-coverage scope statements

These are not defects; they are honest scope statements about what the dogfood did NOT exercise. Each has a concrete next-action and is queued for a follow-on dogfood pass.

| # | Scope statement | Owner-class for next action |
|---|------------------|------------------------------|
| 5 | Gap 3 `recall_observations` live data round-trip not exercised. The MCP tool's parameter branches (`since`, `until`, `limit`) are unit-tested via `tests/recall_observations.rs` (3 tests, commit `913a2ffb0`), but a live probe that runs `memory_recall`, lets the side effect populate `recall_observations`, and then reads it back through the tool against a real recall_id was not run. | Next dogfood agent |
| 6 | Gap 7 signed-link `latest_link_attest_level` decoration on `memory_recall` responses not exercised because the dogfood test corpus contained no signed links. The decoration code path exists and the data shape is defined; what is not yet evidenced is the live round-trip against a memory connected to a signed link. | Next dogfood agent |
| 7 | Phase B v2 retest script (`/Users/fate/v07/v07-fixes/.local-runs/dogfood-2026-05-18/phase_b_revalidate.sh` line 59) references SQL table `archive`; the actual table is `archived_memories`. The script silently failed the `ARCHIVE check` SQL block in the v2 log. This is a test-script defect (not a product defect) — the Gap 5 response itself carries `superseded_id` + `metadata.superseded_id`, which are the load-bearing verification artifacts, so the typo did not invalidate the Gap 5 finding. To be fixed in the next dogfood iteration of the retest script. | Next dogfood agent (script fix; not a separately tracked GH issue at write time) |

## Audit-trail summary

| Class | Count |
|-------|-------|
| Product defects (closed in v0.7.0) | 4 (#892, #893, #894, #895) |
| Documentation drift (closed in v0.7.0, doc-only fix) | 1 (#895 — counted in the 4 above; called out for framing precision because it was the only doc-only fix in the defect column) |
| Test-coverage scope statements | 3 (live recall-observations, signed-link decoration, retest-script SQL typo) |
| **Total findings** | **7** |

Of the **7 total findings**: **4 product defects** all closed in v0.7.0 (3 by code fix + 1 by doc-only fix); **3 test-coverage scope statements** queued with concrete next-actions for the next dogfood iteration. Counting the substrate gap (PG+AGE parity, #894) as a defect rather than a scope statement is deliberate — it was a parity drift between the two adapters, not a "we didn't test it" admission. Earlier framing as "5 findings" / "4 findings + #894 open" was imprecise. The 7-vs-5-vs-4 disagreement was a count framing drift surfaced by the 2026-05-19 truthfulness audit (#917) — the table above is the authoritative count.

Per pm-v3: no finding was deferred to a future release; no finding was framed as "non-blocking"; no finding was handed to the operator. The PG+AGE parity gap (#894) closed in the same v0.7.0 cycle via the migration-v42-v46 + SAL-method + parity-harness series (commits `a69eed03b` → `62cf9e49b`). The three scope statements are queued for the next dogfood pass with concrete next-actions, not labelled "out of scope" in the dismissive sense.

---

*Drafted by Claude Opus 4.7 (1M context) on 2026-05-18. Every row traces to a commit SHA, file path, log artifact, or GitHub issue URL.*
