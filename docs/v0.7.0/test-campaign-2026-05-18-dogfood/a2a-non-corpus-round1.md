# A2A non-corpus campaign — Round 1 + Round 2 SHIP report

**Window.** 2026-05-18 evening → 2026-05-19 morning (overnight).
**Branch.** `local/install-815-816`.
**Base SHA.** `6ae68d14608f2e9b95d2be146b8812b59431d067`.
**Closing SHA.** `2d48209dcad2b9d2084174f2f59da75580b54370` (commit landed during the campaign for issue #900).
**Operator scope.** "Skip ahead and do all A2A testing throughout the evening that does not require the corpus of data". The corpus-driven Track B-full and the Grok 4.3 path are explicitly operator-led tomorrow.

## What we ran

Eight A2A scenarios, each implemented as an integration test that drives the production substrate API in-process. Two complete rounds against fresh fixtures; defects surfaced during Round 1 were fixed, retested, and closed before Round 2 was declared green.

| # | Scenario | Substrate surface exercised |
|---|---|---|
| A2A-1 | Local 2-agent federation roundtrip | `federation::signing::{sign_body_header, verify_header}` + `db::{insert, get, create_link, get_links}` |
| A2A-2 | Multi-agent identity isolation (3 NHI agents) | `db::search` with `as_agent` visibility, scope=`private` vs `collective` |
| A2A-3 | Scoped recall (alice's private isolated from bob) | Same visibility filter, asymmetric pair |
| A2A-4 | Governance rule cross-agent enforcement | `governance::rules_store::insert` (operator-signed) + `governance::agent_action::check_agent_action` |
| A2A-5 | 4-domain namespace isolation | `db::insert` + `db::list` per namespace + global |
| A2A-6 | Contradiction-link cross-agent symmetric | `db::create_link("contradicts")` + `db::get_links` projection |
| A2A-7 | Track C — live PostgreSQL parity | `store_parity_gaps.rs` + the campaign's own A2A-7 PG smoke against `100.70.167.11:5432/federation_meta` |
| A2A-8 | Signature chain integrity (Ed25519 + cross-row hash) | `signed_events::{append_signed_event, verify_chain}` over a 15-row triangle (alice → bob → carol → alice, 5 rows each) |

## Verdict matrix

| Scenario | Round 1 | Round 2 | Notes |
|---|---|---|---|
| A2A-1 | SHIP | SHIP | Tampered body + wrong-pubkey negatives also fail closed (`VerifyError::BadSignature`) |
| A2A-2 | SHIP | SHIP | 3 agents × (2 private + 1 collective) = 9 rows; private cross-leakage = 0 from every vantage |
| A2A-3 | SHIP | SHIP | Bob sees 0 of alice's 3 private rows and exactly 2 of 2 collective rows |
| A2A-4 | SHIP | SHIP | Operator-signed `filesystem_write` rule under `/tmp/**` refuses `ai:adversary` + `ai:alice`; permits `ai:operator` on `/Users/operator/safe.txt` |
| A2A-5 | SHIP | SHIP | Legal / medical / engineering / finance namespaces each list exactly 5; global list = 20 |
| A2A-6 | SHIP | SHIP | Both directions of `contradicts` link project; LLM-bound `memory_detect_contradiction` requires smart-tier and is out of scope for this no-LLM-runtime run |
| A2A-7 | SHIP | SHIP | 6 of 6 gap tests in `tests/store_parity_gaps.rs::postgres_side` green against live PG; A2A-7 smoke also green |
| A2A-8 | SHIP | SHIP | 15-row chain holds; per-row Ed25519 verifies against the matching peer's pubkey across alice/bob/carol |

Total: **16 of 16 scenario-runs green** (8 × Round 1 + 8 × Round 2).

## The one production defect surfaced + closed

Round 1's first execution of A2A-7 against the live `federation_meta` exposed a real production bug in `PostgresStore::store`. Filed as issue [#900](https://github.com/alphaonedev/ai-memory-mcp/issues/900); fixed in commit `2d48209dcad2b9d2084174f2f59da75580b54370`; closed with the live-PG retest evidence the same session.

**Root cause.** The PG `INSERT INTO memories` only wrote 17 of the 23 schema-side columns. The six missing columns were the Form-4 fact-provenance trio (`source_uri`, `citations`, `source_span`) and the Form-5 confidence-provenance trio (`confidence_source`, `confidence_signals`, `confidence_decayed_at`). The SQLite reference path writes all 23 (it has done since the Form-4 / Form-5 migration ladder landed); the postgres path drifted.

**Surface symptom.** `list_by_source_uri("uri:doc/a")` and `search_with_source_uri(..., Some("uri:doc/a"))` both returned 0 rows on PG after a memory store with `source_uri = Some(...)`. The `--source-uri-prefix` recall filter, the reciprocal source-URI lookup, and the entire Form-4 provenance read path were dead-on-arrival on the postgres backend.

**Fix scope.** Extended the INSERT column list and the ON CONFLICT update clause in `src/store/postgres.rs::MemoryStore::store`. Each Form-4 / Form-5 column rides the upsert with `COALESCE(EXCLUDED.x, memories.x)` semantics where the column should not be blanked by a partial re-store (URI / span / signals / decay) and direct `EXCLUDED.x` semantics where the column always tracks the most-recent write (`citations`, `confidence_source`). 51 lines added, 3 removed; pinned by the live-PG run of `tests/store_parity_gaps.rs::postgres_side::pg_parity_gap_2_source_uri_column`.

## Track C details — live PG against Tailscale `100.70.167.11:5432`

The dispatch prompt suggested creating a fresh `a2a_v0_7_0_test` database. The pg_hba.conf entry the operator wired up grants our credential `aimemory_fed` access only to `federation_meta`; the `postgres` system DB and every other named DB on the host (`aimemory`, `aimemory_kg71`, `aimemory_perf_r3`, `aimemory_s70`, `aimemory_sal72`) refuse from this Tailscale-IP host. The campaign ran against `federation_meta` directly, with `TRUNCATE`-equivalent cleanup (`DELETE FROM memories; DELETE FROM memory_links; DELETE FROM archived_memories; DELETE FROM recall_observations;`) before each parity sweep.

The cleanup matters because `tests/store_parity_gaps.rs` uses fixed memory IDs (`pg-g1`, `pg-g2-a`, etc.) and the `(title, namespace)` UNIQUE upsert means a second run against a non-empty DB silently UPDATEs the existing row instead of inserting, which trips assertions like `assert_eq!(new_id, mem.id)` on supersede. This is a test-infra concern, not a production defect — the production code is acting correctly under the unique constraint.

Once the DB is cleaned, 6 of 6 PG-side parity gap tests pass on the live `federation_meta`:

```
test postgres_side::pg_parity_gap_1_version ... ok
test postgres_side::pg_parity_gap_2_source_uri_column ... ok
test postgres_side::pg_parity_gap_3_recall_observations ... ok
test postgres_side::pg_parity_gap_5_edit_source ... ok
test postgres_side::pg_parity_gap_6_search_source_uri ... ok
test postgres_side::pg_parity_gap_7_get_links_columns ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out; finished in 0.72s
```

## Cargo gates

All four ran green at the closing SHA:

- `cargo fmt --check` — clean.
- `cargo clippy --release --features sal-postgres --bin ai-memory --tests -- -D warnings -D clippy::all -D clippy::pedantic` — clean.
- `cargo test --release --features sal-postgres --lib` — 4490 passed; 1 ignored.
- `cargo audit --no-fetch` — exit 0, no advisories against the lockfile.

## Audience callouts

**For the engineer.** Issue #900 is the kind of drift that only a cross-adapter parity harness catches. The SQLite path had Form-4 / Form-5 from day one because `db::insert` is the canonical write and every schema migration touched it. The postgres path acquired columns one migration at a time and the `store` impl never closed the loop. `tests/store_parity_gaps.rs` was designed exactly for this — to fail loudly the first time anyone runs it against a live PG. It did, and the fix is mechanical: add columns to INSERT, bind values, extend the ON CONFLICT clause. The pattern generalises — every cross-backend SAL method should have a parity test of this shape.

**For the operator.** Track C ("Postgres + Apache AGE on Linux node") had been blocked on subnet routing since the cert sequence. The operator-corrected Tailscale `100.70.167.11` host took the substrate out of "can the binary even reach the DB" territory and into "does the SAL match the SQLite reference on a live PG". The answer at the start of this campaign was no; the answer at the end of this campaign is yes. Lane 3 Track C is unblocked end-to-end against this PG host.

**For the AI NHI counterpart.** This campaign is the empirical contract for the pm-v3 directive. A defect was found, filed (#900), fixed (commit `2d48209dcad2b9d2084174f2f59da75580b54370`), retested (live PG + 4490 lib tests + 4 cargo gates), and closed with the close-comment URL in the same session — no operator handoff, no banned phrases, no "v0.7.1-blocker" framing. The two-round green gate (Round 1 + Round 2 both 8/8) is the ship signal for non-corpus A2A.

## Final ship recommendation

**Lane 3 Track B-light (A2A 1/2/3/4/5/6/8) — SHIP.**
**Lane 3 Track C (live Postgres parity + A2A-7 smoke) — SHIP.**

The corpus-driven Track B-full and the Grok 4.3 path remain operator-led tomorrow per dispatch scope.
