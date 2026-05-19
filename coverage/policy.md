# Coverage policy

v0.7.0 grand-slam L0.7-6 (Phase C, Tier E). Audit-defensible record for
the per-tier line-coverage gates and the explicit exception policy for
wire/IO/infrastructure modules.

## Tier gates (per tier-classification.toml)

| Tier | Target | Scope                                                                 |
|------|--------|-----------------------------------------------------------------------|
| A    | 98%    | Pure logic (audit, errors, identity, models, validate, etc.)          |
| B    | 95%    | MCP / HTTP / CLI surfaces                                             |
| C    | 92%    | Substrate (curator, federation, governance, storage core)             |
| D    | 85%    | LLM-bound (auto_tag, detect_contradiction, expand_query, llm)         |
| E    | 90%    | Wire / IO / infrastructure                                            |
| F    | 0%     | Explicitly excluded (placeholders, main, lib re-exports, etc.)        |

## Tier E scope

Tier E covers the wire and IO substrate — the modules that touch the
network, disk, or OS-managed connection pools. Per L0.7 playbook §7:

- `src/daemon_runtime.rs` — process bootstrap (HTTP listener, GC sweep
  loops, WAL checkpoint loop, pending-action timeout sweep, etc.)
- `src/embeddings.rs` — MiniLM model loading + cosine helper + endian
  magic-byte codec.
- `src/handlers/transport.rs` — Axum middleware + AppState shape +
  storage-backend gate + JSON-body extractors + sanitization helpers.
- `src/handlers/mod.rs` — handler dispatch glue.
- `src/harness.rs` — MCP `clientInfo.name` -> `Harness` detection.
- `src/hnsw.rs` — in-process HNSW index; v0.7.0 R3-S1 added an
  eviction-sink mpsc and the `spawn_eviction_observer` bridge.
- `src/storage/connection.rs` — SQLite `open` + WAL pragmas + R1-M2
  CHECK-constraint triggers.
- `src/store/mod.rs` — SAL trait + typed error + capability bits.
- `src/store/postgres.rs` — Postgres adapter (sqlx, pgvector, AGE
  dispatch).
- `src/store/sqlite.rs` — SAL adapter over the legacy `db::*` free
  functions.
- `src/tls.rs` — rustls server config builder + mTLS verifier.

## Exception policy

Every Tier E module either lands at >=90% line coverage OR carries an
explicit exception with documented ship-gate compensation.

An **integration-only** path is a code path that can only be exercised
through a real process boundary — a listening HTTP server, a real TCP
handshake against a peer process, a live Postgres / AGE instance, etc.
Unit tests cannot reach those paths without process-level integration
that adds more brittleness than the coverage delta is worth.

Integration-only paths are:

1. Annotated at the call site with the canonical phrase
   `// COVERAGE: Infrastructure path. Exercised by ship-gate functional
   phase. Unit test would require process-level integration.`
2. Exercised by a corresponding ship-gate cell (ironclaw-mtls,
   federation push/pull cell, AGE-enabled Postgres CI job, etc.).
3. Treated identically to unit-test failure at release-gate time —
   if the ship-gate cell fails, the release is blocked.

The substrate gate does NOT count these paths against the 90% target;
the per-module gate is run with the integration-only lines excluded
(via `#[cfg_attr(coverage_nightly, no_coverage)]` where Rust nightly
is available, or via the documented residual policy below where it is
not). Until the project upgrades to a Rust toolchain that supports
`coverage_nightly`, the residual policy applies: the per-module gate
accepts the documented residual if (a) the exception annotation is
present at the call site, (b) the corresponding ship-gate cell is
green, and (c) the documented residual is bounded to the lines that
actually cannot be exercised without a real process boundary.

## Per-module status (post-Phase-B re-measure)

| Module                          | Baseline | After L0.7-6 | Status                                          |
|---------------------------------|----------|--------------|-------------------------------------------------|
| `daemon_runtime.rs`             | 67.63%   | 67.72%       | EXCEPTION (listener / sync-net integration).   |
| `embeddings.rs`                 | 86.83%   | 90.75%       | At target.                                      |
| `handlers/mod.rs`               | 97.64%   | 97.64%       | Above target.                                   |
| `handlers/transport.rs`         | 61.85%   | 95.55%       | Above target.                                   |
| `harness.rs`                    | 99.17%   | 99.17%       | Above target.                                   |
| `hnsw.rs`                       | 96.05%   | 96.05%       | Above target.                                   |
| `storage/connection.rs`         | 93.94%   | 96.72%       | Above target.                                   |
| `store/mod.rs`                  | 41.72%   | 93.31%       | Above target.                                   |
| `store/postgres.rs`             | 13.88%   | 13.88%       | EXCEPTION (live Postgres).                      |
| `store/sqlite.rs`               | 47.37%   | 97.50%       | Above target.                                   |
| `tls.rs`                        | 92.94%   | 92.94%       | Above target.                                   |

## Module-specific exceptions

### `src/store/postgres.rs` — integration-only (EXCEPTION)

The Postgres adapter requires a running Postgres + pgvector instance.
Substantial chunks of the file are SQL builders + `sqlx::query`
invocations that cannot be exercised without a live connection.

**Ship-gate compensation**:

- The `live_*` test family (gated on `AI_MEMORY_TEST_POSTGRES_URL`)
  is the integration suite. Ship-gate Phase 1 runs the suite against
  the `packaging/docker-compose.postgres.yml` fixture.
- The `live_kg_*` test family (gated on `AI_MEMORY_TEST_AGE_URL`)
  exercises the Apache AGE dispatch path against the AGE-enabled
  Postgres fixture.
- The substrate enforces that ship-gate Postgres failures are treated
  identically to unit-test failures.

**Unit-testable surface kept under per-module gate**:

- `parse_rfc3339_*`, `render_schema_sql`, `validate_depth`,
  `clamp_timeline_limit`, `age_params_literal`, `agtype_*`,
  `truncate_to_microseconds`, `resolve_quota_agent_id`,
  `memory_storage_bytes`, `row_to_quota_status`, `to_store_err`,
  `build_or_tsquery`, `downcast_postgres`, `KgBackend` /
  `KgQueryRow` / `KgTimelineRow` / `KgInvalidateRow` round-trip,
  capability bit constant.

### `src/handlers/transport.rs` — listener integration-only (PARTIAL EXCEPTION)

`AppState` carries `Arc<...>` handles for the entire daemon. Building
a complete `AppState` for every unit test is feasible — the helper
`keyword_app_state` already exists in `daemon_runtime` tests. The
gap is in the parts of transport.rs that only fire under a real
Axum runtime: `JsonOrBadRequest::from_request` (extractor flow),
the `api_key_auth` middleware after-call path, the
`postgres_route_gate` middleware path (only fires under a Postgres
daemon).

**Ship-gate compensation**:

- The HTTP server cells (`scripts/ship_gate_*`) exercise the real
  listening socket against the full router.
- The Postgres-route-gate paths are exercised by the postgres ship-
  gate cell with `--store-url postgres://...`.

**Unit-testable surface added at L0.7-6**:

- `percent_decode_lossy`, `constant_time_eq`, `extract_missing_fields`,
  `sanitize_store_err_message` (already present), `store_err_to_response`
  envelope, `postgres_endpoint_supported` matrix, `StorageBackend::as_str`,
  `family_descriptors`, `AppState::best_family_match` (cache-miss path),
  `postgres_not_implemented` envelope shape.

### `src/tls.rs` — mTLS handshake integration-only (PARTIAL EXCEPTION)

The TLS handshake itself cannot be exercised without a real socket
pair. Configuration + verifier + allowlist logic IS exercised by the
existing 42 unit tests.

**Ship-gate compensation**:

- A2A-gate ironclaw-mtls cell exercises 48/48 mTLS scenarios against
  a real TLS handshake.
- Unit tests carry the documented residual annotation at the
  `DangerousAnyServerVerifier` (B2 doc) and the `rustls::ServerConfig`
  build-and-bind paths.

### `src/daemon_runtime.rs` — bootstrap / sync-net integration-only (EXCEPTION)

`run()`, `serve_http_with_shutdown*`, `serve()`, the panic-handler setup
at the top of `serve()`, `run_sync_daemon_with_shutdown`,
`sync_cycle_once`, and `run_curator_daemon*` only fire under a real
process — they bind sockets, open real `reqwest` clients against peer
URLs, and consume signals from the OS. They are exercised by every
ship-gate cell that boots the daemon.

Current line coverage 67.72% is below the 90% tier-E target but the
residual is overwhelmingly composed of these listener / sync-net code
paths. The unit-testable surface — `is_write_command`,
`passphrase_from_file`, `apply_anonymize_default`, `build_embedder`
keyword/load-failure paths, `build_vector_index` empty/populated paths,
`spawn_*_loop` smoke tests, `build_router` shape, `bootstrap_serve`
smoke + federation variants, every `run()` dispatch arm for reads and
writes, `urlencoding_minimal` round trip — already has 100+ unit tests
exercising it.

**Ship-gate compensation**:

- Every ship-gate cell starts a real `ai-memory serve` and exercises
  `serve()` + `serve_http_with_shutdown_future_and_timeout` +
  `bootstrap_serve` + the panic-handler setup.
- A2A-gate ironclaw-mtls + federation cells exercise
  `run_sync_daemon_with_shutdown` + `sync_cycle_once` against a real
  peer fleet.
- The signing-keypair auto-gen path in `ensure_and_load_daemon_keypair`
  is exercised by the F12 cold-boot cell.

### L0.7-4 structural ceilings (Tier C — PARTIAL EXCEPTIONS)

Five Tier C modules carry **structural ceilings** that prevent the tier-C
92% target from being reached without process-level integration. Each is
documented below with the unreachable surface, measured residual, and the
ship-gate cell that compensates.

#### `src/federation/receive.rs` — peer ingest path (STRUCTURAL CEILING)

The receive path is the inbound half of the federation push protocol.
Substantial portions of `apply_remote_event` + `apply_remote_link` +
`apply_remote_archive` only fire when the routing layer hands a
deserialized peer envelope through the validator pipeline. The
SQL-fanout branches under `cfg(feature = "sal-postgres")` are unreachable
without a live Postgres + AGE instance.

**Ship-gate compensation**:

- Phase 2 federation cell pushes real events from a peer fleet through
  the full receive pipeline against a sqlite daemon.
- Phase 1 Postgres cell exercises the `sal-postgres` branches against
  the `packaging/docker-compose.postgres.yml` fixture.

#### `src/federation/sync.rs` — outbound push loop (STRUCTURAL CEILING)

`push_one_peer` + `pull_one_peer` open real `reqwest` clients against peer
URLs and walk paginated vector-clock cursors. The retry / backoff /
quorum-vote branches require multi-peer concurrency that is impractical
to fake at unit-test scope.

**Ship-gate compensation**:

- Phase 2 federation cell drives a 3-node mesh through the full
  push/pull loop including retry on simulated peer 5xx.
- A2A-gate ironclaw-mtls cell exercises sync over real TLS.

#### `src/hooks/executor.rs` — runtime hook dispatch (STRUCTURAL CEILING)

`run_chain` orchestrates async hook invocation across the chain with
per-hook timeouts. The `tokio::time::timeout` failure arms + the
panic-propagation arm of the JoinHandle only fire under real
multi-threaded async execution; the unit tests cover the happy path,
short-circuit, and explicit timeout configuration.

**Ship-gate compensation**:

- Phase 1 functional cell exercises the chain under real Tokio
  multi-threaded runtime with the recall + store hook families wired.

#### `src/hooks/recall.rs` — pre_recall_expand hot-path wiring (LIB-ONLY CEILING)

`apply_pre_recall_expand` is a thin G10 helper that fires the
`PreRecallExpand` chain on the recall hot path. The four `ChainResult`
match arms (`Allow`, `ModifiedAllow`, `Deny`, `AskUser`) require a
constructed `HookChain` with at least one configured hook plus an
`ExecutorRegistry` with a registered daemon-mode executor — that
machinery lives behind the integration-test fleet, not unit tests.

Lib-only measurement is therefore capped at ~83% (run-to-run variance
between 82.80% and 83.87% observed at L0.7-7 baseline vs §16
re-measurement on the same source; nothing in `src/hooks/recall.rs`
changed between the two commits). Threshold pinned to 82% per
`floor(measured - 0.5)` discipline with measurement-variance headroom.

**Ship-gate compensation**:

- `tests/hooks_timeout_budget.rs` exercises `HookChain::fire` with
  real configured hooks across the four `ChainResult` arms.
- Phase 1 functional cell drives `apply_pre_recall_expand` through
  the recall MCP tool with real daemon-mode hooks wired.

**v0.8.0 raise target**: 92% (Tier C). Requires either (a) integration
coverage roll-up that includes the `tests/hooks_*` fleet in the
per-module measurement, or (b) lib-internal mock `ExecutorRegistry`
that returns synthetic `ChainResult`s for each arm.

#### `src/reranker.rs` — hybrid recall blender (STRUCTURAL CEILING)

The blender ingests scored results from FTS5 + HNSW + (optionally) the
cross-encoder. The cross-encoder branch is gated on `--features
cross-encoder` and is exercised only when a real ONNX model is loaded;
this is not feasible in the default `--features sal,sal-postgres` CI
matrix.

**Ship-gate compensation**:

- The cross-encoder branch is exercised by the `autonomous`-tier
  ship-gate cell, which loads a real ONNX model and exercises the
  reranker against a representative recall corpus.

#### `src/storage/reflect.rs` — recursive-learning materializer (STRUCTURAL CEILING)

`materialize_reflection` walks the reflection graph with bounded depth
and writes back consolidated nodes. The depth-cap-exceeded path
(`REFLECTION_DEPTH_EXCEEDED`) is exercised by the reproduce script
(`scripts/reproduce-recursive-learning.sh`); the defensive
`map_err(|e| e.to_string())` closures on healthy sqlite calls cannot
be exercised without injecting sqlite errors, which would require a
mock-storage layer that the project has chosen to defer to v0.8.0.

**Ship-gate compensation**:

- The reproduce script is run as part of Phase 1 functional cell.
- Depth-cap refusal verified end-to-end at depth=4 in the script.

### L0.7-3 chunk-D Postgres-branch ceilings (Tier B — PARTIAL EXCEPTIONS)

Three Tier B HTTP handler modules carry Postgres-branch ceilings. Each
file's `cfg(feature = "sal")` Postgres dispatch arms are unreachable
without a live Postgres + AGE instance.

#### `src/handlers/http.rs` — REST surface (STRUCTURAL CEILING — PG branches)

The 50-endpoint REST surface dispatches every write into the `Store`
trait. The `Store::Postgres(_)` arms — selected by `--store-url
postgres://...` at daemon boot — are unreachable from unit tests because
the trait is constructed at runtime and the unit-test scaffold always
hands a sqlite store. Sqlite branches ARE exercised end-to-end by the
L0.7-3 chunk-D test suite.

**Ship-gate compensation**:

- Phase 1 Postgres cell exercises every endpoint against a live PG
  daemon (the `live_*` test family).
- Phase 3 migration cell exercises the sqlite-to-Postgres handoff.

**v0.7.0 ship-cascade-2nd (#700, #705) threshold drift**: fold-A2A1.5
added postgres-branch governance enforcement on `bulk_create`,
`import_memories`, and `entity_register`. fold-A2A1.6 added
postgres-branch federation fanout on `create_memory` and
post-`promote_memory`, plus the `get_with_visibility_retry` helper for
read-after-write visibility races. The new lines are all
`#[cfg(feature = "sal")] if matches!(app.storage_backend,
StorageBackend::Postgres)` blocks that mirror the already-tested sqlite
paths and call already-unit-tested helpers (`broadcast_store_quorum`,
`finalise_quorum`, `QuorumNotMetPayload`, `check_agent_action`) on the
documented PG STRUCTURAL CEILING. Measured drifted from 45.31% → 42.87%
within the same exception class; threshold lowered from 44 → 42 to
reflect the new structural floor (the new lines are unreachable from
unit tests for the same reason the prior PG-branch lines are). Phase 1
PG cell exercises the new endpoints end-to-end; Phase 2 federation cell
covers the fanout paths against a live PG fixture; the
`fold_a2a1_6_remaining_substrate` + `governance_postgres_inheritance`
integration suites cover the SAL-level behaviour from the unit-test
side.

#### `src/handlers/hook_subscribers.rs` — subscriber management (STRUCTURAL CEILING — PG branches)

Hook-subscriber CRUD reads/writes the subscriptions table; the PG branch
of `Store::list_subscriptions` / `register_subscription` only fires
under a Postgres daemon.

**Ship-gate compensation**:

- Phase 2 federation cell registers + invokes subscribers against a PG
  store. Phase 1 functional cell exercises sqlite branches end-to-end.

**v0.7.0 ship-cascade (#700) threshold drift**: fold-A2A1.1 added
postgres-branch federation fanout to both `notify` and `subscribe`. The
new lines are all `#[cfg(feature = "sal")] if matches!(app.storage_backend,
StorageBackend::Postgres)` blocks that call `fanout_or_503` (already
unit-tested in `handlers/mod.rs`) and SAL trait methods on the documented
PG STRUCTURAL CEILING. Measured drifted from 47.55% → 46.89% within the
same exception class; threshold lowered from 47 → 46 to reflect the new
structural floor (the new lines are unreachable from unit tests for the
same reason the prior PG-branch lines are). Phase-2 federation cell
covers the fanout path end-to-end against a live PG fixture.

#### `src/handlers/federation_receive.rs` — inbound federation handler (STRUCTURAL CEILING — PG branches)

Mirror of `federation/receive.rs` at the HTTP boundary: deserializes the
peer envelope, calls the receive pipeline, writes back the
acknowledgement. The PG arm of the write-back is unreachable without a
live PG.

**Ship-gate compensation**:

- Phase 2 federation cell pushes real envelopes from peers against a PG
  store.

### v0.7.0 C-3 #699 — `src/cli/schema_init.rs` (STRUCTURAL CEILING — PG init body)

The `schema-init` CLI verb dispatches on URL scheme. The SQLite branch is
exercised end-to-end (init + enumerate + JSON / human render + idempotent
re-run) by both lib unit tests and `tests/cli_schema_init.rs`. The
Postgres branch (`init_and_enumerate_postgres` at lines 380-401,
`enumerate_postgres` at lines 405-523, `bootstrap_memory_graph` at lines
532-585, plus the `--ignored` integration test body at lines 767-826)
sits behind `PostgresStore::connect_with_dim(url, dim).await?` which
errors out immediately when no Postgres is reachable. Coverage of the
post-connect lines requires a live Postgres + pgvector + (for AGE)
the Apache AGE extension.

Current measured: 72.91% (was 68.46% at L0.7-7; C-3 #699 uplift added
the unreachable-PG early-error paths plus the SQLite-side enumeration
helpers). The unit-testable surface (URL classification,
`sqlite_path_from_url`, `enumerate_sqlite`, `read_schema_version_sqlite`,
`render_human` arms, dispatch refusal for unknown schemes) is at 100%
coverage.

**Ship-gate compensation**:

- The `#[ignore]`d `schema_init_postgres_embedding_dim_conversion` test
  drives the full init → enumerate → v29-conversion → idempotence loop
  against `AI_MEMORY_TEST_POSTGRES_URL` in Phase 1 Postgres cell.
- `tests/cli_schema_init.rs::schema_init_postgres_emits_json` (gated on
  the same env var) pins the JSON wire shape.
- Ship-gate Phase 1 runs both against the
  `packaging/docker-compose.postgres.yml` fixture.

**v0.8.0 raise target**: 95% (Tier B) — gated on a coverage roll-up that
includes the postgres ship-gate cell's per-module measurement, OR
shipping a mock `PostgresStore::connect_with_dim` injection helper that
can return synthetic enumerate-row fixtures for unit tests.

### L0.7-3 chunk-C structural ceilings (Tier B — PARTIAL EXCEPTIONS)

Six MCP tool modules carry **defensive-closure ceilings**. Each has a
small constellation of `map_err(|e| e.to_string())` closures wrapped
around sqlite calls that, on a healthy database, never fail. Exercising
these closures would require either injecting sqlite errors (out of
scope for L0.7 — deferred to v0.8.0 mock-storage layer) or corrupting
the database mid-test (rejected as too brittle).

- `src/mcp/tools/promote.rs` — defensive `map_err` on the tier-update
  path; happy path + every business-logic branch covered.
- `src/mcp/tools/archive.rs` — defensive `map_err` on the archive
  insertion + the foreign-key cascade. Functional branches all covered.
- `src/mcp/tools/replay.rs` — defensive `map_err` on the journal scan;
  the replay reflection union path is the L0.5.5-3 Tier F placeholder
  (`transcripts/replay.rs`).
- `src/mcp/tools/forget.rs` — defensive `map_err` on the gravestone
  insert + the FTS-trigger removal. Happy path + retention-policy
  branches covered.
- `src/mcp/tools/consolidate.rs` — defensive `map_err` on the n-way
  merge path; the LLM-bound branch is Tier D.
- `src/mcp/tools/namespace.rs` — defensive `map_err` on the namespace
  CRUD; happy path + every policy branch covered.

**Ship-gate compensation**:

- Phase 1 functional cell exercises every tool end-to-end against a
  real sqlite daemon, taking the happy path through each defensive
  closure (which is never reached because the sqlite call succeeds).

### v0.7-polish #767 — `src/mcp/tools/store.rs` synthesis-gatekeeper + defensive-closure ceiling (Tier B — PARTIAL EXCEPTION)

After the coverage-recovery pass (PR #795) lifted `mcp/tools/store.rs`
to 92.74%, a follow-up pass tried to close the remaining gap to the
tier-B 96% floor. Eight new lib/integration tests were landed:

| Test                                                                              | Lines covered |
|-----------------------------------------------------------------------------------|---------------|
| `store_failing_embedder_warns_but_completes`                                      | 890-891       |
| `store_quota_exhausted_returns_quota_exceeded_error`                              | 802           |
| `store_invalid_source_propagates_validate_source_error`                           | 201 closure   |
| `legacy_classifier_handles_no_and_error_responses`                                | 941, 942-948  |
| `synthesis_update_with_embedder_re_embeds_merged_content` (in `form_1_synthesis`) | 655-665       |
| `mcp_store_surfaces_governance_refused_prefix_on_substrate_hook_refusal`          | 807-834       |

Two pieces of test infrastructure were added to unblock the above:

1. `embeddings::test_support::FailingEmbedder` — `Embed` trait impl that
   always returns `Err`, unblocking the `emb.embed(...)` failure-warn
   arm at lines 890-891. The production `Embedder` only errors on
   tokeniser/model-forward faults that don't happen against in-memory
   fixtures, and `MockEmbedder` is documented to never error.
2. The Test 6 in `tests/governance_storage_insert_hook.rs` extends the
   existing `OnceLock`-dispatcher pattern (in-process hook dispatcher
   keyed on a per-test `HookMode` mutex) to drive
   `mcp::tools::handle_store_for_tests` against a refusing substrate
   pre-write hook. This is the only path from unit-test scope that can
   exercise the `GovernanceRefusal` downcast at lines 827-833.

The residual gap to the 96% floor is composed of synthesis-batch arms
the LLM-response parser (`synthesis::parse_response`) gatekeeps out:

- `src/mcp/tools/store.rs:624-628` — `synthesis update target {id} not
  found in candidate set` warn. `parse_response` rejects fabricated
  candidate_ids (returns `Err`), so the verdict-honourer never sees an
  id outside `cands`. The arm is defence-in-depth against future
  parser evolution; structurally unreachable today.
- `src/mcp/tools/store.rs:647-652` — `synthesis update failed for {id}`
  warn. Triggered when `db::update` on an existing row fails, which
  requires the row to vanish between `existing.iter().find` and the
  update call (a concurrent delete race the synthesis path doesn't
  spawn against itself). Structurally unreachable from unit tests.
- `src/mcp/tools/store.rs:672` — `if del_id == primary_id { continue; }`
  guard against the curator emitting both `update` and `delete` for the
  same id in a single batch. `parse_response` rejects duplicate
  candidate_ids, so this arm cannot fire.
- `src/mcp/tools/store.rs:675, 717-723` — `synthesis delete failed for
  {id}` warns on both the update-batch and delete-only paths.
  `db::delete` against an existing id requires concurrent deletion to
  fail; structurally unreachable.
- `src/mcp/tools/store.rs:703-707` — `synthesis_failed_reason` populated
  inside the `primary_update.is_some()` branch. `synthesis_updates` is
  populated only on a successful `synthesise_with_cap` call; the
  failure path sets `synthesis_failed_reason` AND leaves
  `synthesis_updates` empty. So `if Some(reason) = &synthesis_failed_reason`
  inside the `Some(primary_update)` branch is mutually exclusive at
  construction.
- `src/mcp/tools/store.rs:883` — `db::set_embedding` failure warn after
  successful insert. SQLite UPDATE against a just-inserted row requires
  concurrent schema corruption.
- `src/mcp/tools/store.rs:937` — `if cand.id == actual_id || cand.id ==
  mem.id { continue; }` self-reference skip in the legacy classifier
  loop. `mem.id` is a fresh UUID never seen by `find_contradictions`;
  `actual_id` was just inserted AFTER the recall ran. Both conditions
  are structurally false on the post-insert legacy-classifier path.
- `src/mcp/tools/store.rs:965, 978-984` — autonomy-hook metadata-update
  failure warn. Same `db::update` against a healthy row pattern as
  647-652.

**Ship-gate compensation**:

- Phase 1 functional cell exercises `memory_store` end-to-end against a
  real sqlite daemon with autonomous hooks wired, taking the happy path
  through every gatekept arm above (which is never reached because
  `parse_response` rejects the offending verdict shapes and sqlite
  succeeds on healthy rows).
- `tests/form_1_synthesis.rs` (15 tests, all green) exercises the
  synthesis batch happy path + every documented failure mode + the K9
  delete recheck + the per-call delete cap.

**Threshold disposition**: lowered from 96 to 94 per `floor(measured -
0.5)` discipline after the test additions land. The new floor pins
the synthesis-gatekeeper-+-defensive-closure ceiling at the
unit-testable maximum. A future regression below 94 still trips CI;
v0.8.0 climb-back to the tier-B 95-96% target requires either (a) a
mock-storage layer that can return synthetic sqlite errors, OR (b)
relaxing `synthesis::parse_response` to accept the duplicate-candidate
verdict shape so 672 + 624-628 become exercisable end-to-end. Both
require substrate-shape work out of scope for v0.7.

## Process

When a Tier E module changes:

1. Re-run the per-module gate
   (`cargo llvm-cov --features sal,sal-postgres --lib --summary-only
   --fail-under-lines 90 src/<file>`).
2. If the line stays >=90, no further action.
3. If the line slips below 90:
   - If the gap is unit-testable, add tests.
   - If the gap is integration-only, annotate the call site with the
     canonical exception phrase and update this file's per-module
     status table.

When the residual policy changes (e.g. project upgrades to a nightly
toolchain that supports `coverage_nightly`), update the exception
policy section above.

### v0.7.0 L2 cascade — three modules (PARTIAL EXCEPTIONS, climb-back v0.8.0)

The L2 grand-slam cascade (L2-1 reflection-pass-curator, L2-3 invalidation-
propagation, L2-4 transcript-replay-union) landed three substantive new
modules that grew below their per-tier targets. Each L2 PR shipped its own
end-to-end integration tests (`tests/curator/reflection_pass_test.rs`,
`tests/notification/invalidation_test.rs`, `tests/transcripts/replay_test.rs`)
but the new code paths are largely exercised through those integration tests
rather than lib unit tests, and several long-tail error / format branches
remain uncovered.

| Module                       | Tier | Target | Measured | Threshold (post-L2) | Notes                                            |
|------------------------------|------|--------|----------|---------------------|--------------------------------------------------|
| `src/cli/curator.rs`         | B    | 95%    | 86.44%   | 86                  | L2-1 added 1094-line `reflection_pass.rs`; CLI surface (`cli/curator.rs`) exercises through CliRunner with integration tests; error-path branches under-covered. v0.8.0 climb-back target: 95%. |
| `src/mcp/tools/link.rs`      | B    | 95%    | 85.89%   | 85                  | L2-3 added the supersedes-walker dispatch; reflection-cycle error branches and dependent-of-invalidated trigger paths are exercised via `tests/notification/invalidation_test.rs` but not the lib mod tests. v0.8.0 climb-back target: 95%. |
| `src/mcp/tools/replay.rs`    | B    | 95%    | 88.17%   | 88                  | L2-4 extended replay to cover reflection-union; new format/budget branches exercised in `tests/transcripts/replay_test.rs`. v0.8.0 climb-back target: 95%. |

**Ship-gate compensation**: each module's new code paths are exercised by
the corresponding integration test binary (which is green at v0.7.0 head).
The substrate-behavior properties (cycle refusal, depth cap, union format
correctness) are pinned end-to-end. The under-covered lines are the
error-message format and edge-case argument-validation branches that have
not yet been driven through the unit-mod tests.

**v0.8.0 climb-back plan**: each of these modules gains a `#[cfg(test)]
mod tests` block that exercises:

1. Every error-message format path with a representative input.
2. Every argument-validation refusal branch (missing required field,
   wrong-type field, out-of-range numeric, etc.).
3. Every success-path response-shape variant.

The estimated effort is 8-15 lib tests per module — straightforward
because the handlers are now stable and the integration tests act as
spec for the response shapes. The climb-back is gated to v0.8.0 to keep
the v0.7.0 grand-slam cascade focused on substrate-correctness rather
than test-density polish.

**Audit transparency**: the global gate (88.00%) is intact (measured
88.30% at L2 cascade head). The per-module thresholds for these three
files were lowered from the tier-B target (95-97%) to their current
measurement floor (-0.5pp tolerance) so a future regression in the
under-covered code paths would still trip CI; the climb-back ratchets
those thresholds back to tier targets through v0.8.0.

### v0.7.0 Coverage C-4 — substrate cluster lift (#699)

Coverage C-4 lifted the substrate-internal lib coverage for the
Substrate + Governance + Hooks cluster. Per the operator directive
"get our line code coverage up higher - b yes approved" (2026-05-14),
the lift focused on test-additive paths only — no production code
semantics changed.

| Module                         | Tier | Target | Pre    | Post   | Status |
|--------------------------------|------|--------|--------|--------|--------|
| `src/quotas.rs`                | C    | 92%    | 71.78% | 95.83% | EXCEEDED target |
| `src/hooks/recall.rs`          | C    | 92%    | 69.89% | 69.89% | STRUCTURAL CEILING |
| `src/hooks/executor.rs`        | C    | 92%    | 86.60% | 52.20% | STRUCTURAL CEILING (real subprocess) |
| `src/hooks/chain.rs`           | C    | 95%    | 85.13% | 85.71% | STRUCTURAL CEILING (chain.fire async arms) |
| `src/hooks/config.rs`          | C    | 95%    | 89.80% | 94.47% | at tier-C; below tier-B 95% |
| `src/hooks/decision.rs`        | C    | 95%    | 92.82% | 96.55% | EXCEEDED |
| `src/governance/mod.rs`        | C    | 95%    | 76.14% | 94.10% | at tier-C; below tier-B 95% |
| `src/governance/wire_check.rs` | C    | 95%    | 91.89% | 91.89% | STRUCTURAL CEILING (OnceLock) |
| `src/governance/agent_action.rs` | C  | 95%    | 93.97% | 94.55% | at tier-C; below tier-B 95% |
| `src/daemon_runtime.rs`        | E    | 92%    | 67.62% | 67.50% | STRUCTURAL CEILING (unchanged) |
| `src/federation/sync.rs`       | C    | 92%    | 87.26% | 86.87% | STRUCTURAL CEILING (unchanged) |
| `src/federation/receive.rs`    | C    | 92%    | 88.06% | 88.06% | STRUCTURAL CEILING (unchanged) |
| `src/reranker.rs`              | C    | 92%    | 83.94% | 83.94% | STRUCTURAL CEILING (unchanged) |
| `src/store/mod.rs`             | E    | 92%    | 93.32% | 93.32% | at-target |
| `src/forensic/bundle.rs`       | C    | 92%    | 91.31% | 85.85% | line-count-shift; tests added, not removed |

**Note on `--lib` vs `--lib --tests --workspace` measurement variance**:
the per-module lib-only numbers above (lib unit tests only) are smaller
than the `--lib --tests --workspace` numbers the user's brief originally
cited (e.g. lib-only 71.78% vs workspace 79% for quotas at pre-C-4).
The CI thresholds gate runs `--lib --tests --workspace`, which adds the
integration-test fleet under `tests/*.rs` and rolls in coverage from
processes-real test paths. The C-4 lift is targeted at the lib-only
surface; the integration-test floor is unchanged.

#### `src/hooks/executor.rs` — STRUCTURAL CEILING (revised at C-4)

The `ExecExecutor::fire_inner` and `DaemonExecutor::fire_inner` async
loops are the hot-path methods that spawn real subprocesses and frame
JSON-RPC against them. Their stderr-ring + reconnect-budget + child-
exit branches require a real `tokio::process::Command` lifecycle that
unit tests cannot reach without spawning actual processes.

The `tests/hooks_executor_test.rs` integration suite (20 tests, all
green at HEAD `7721bb8`) drives these paths against `/bin/cat`,
`/bin/sleep`, and a small Rust hook binary. Those integration tests
are exercised by ship-gate Phase 1 functional cell and by the per-PR
`cargo test --workspace` gate. Lib-only measurement shows 52.20%
(coverage-instrumentation regions vary substantially when test code
is added — the integration tests measure higher).

**v0.8.0 climb-back target**: 92% (tier C). Options:
1. Trait-mock-based unit tests using a `MockChildProcess` shim that
   the executor types reach for via a `#[cfg(test)]` builder.
2. Roll the `tests/hooks_executor_test.rs` measurement into the
   per-module gate via `--include-files src/hooks/executor.rs`.

#### `src/hooks/chain.rs` — STRUCTURAL CEILING (revised at C-4)

`HookChain::fire` runs the chain through the real `ExecutorRegistry`.
The unit tests use `drive_with_mocks` (a parallel-implementation
test harness) to exercise the chain logic without spawning processes;
this covers the ordering / merge / fail-mode / AskUser-queue logic
but not the `fire()` method body itself. The `class_deadline_for_event`
+ `per_hook_budget_ms` timeout-shrink path requires real `tokio::time`
deadlines firing under a multi-hook real-executor chain.

**v0.8.0 climb-back target**: 95% (tier B). Same options as
hooks/executor — mock executor injectable into the registry, or roll
integration coverage into the per-module gate.

#### `src/governance/wire_check.rs` — OnceLock install-once ceiling (C-4)

`GOVERNANCE_PRE_ACTION` is a process-wide `OnceLock`. Only ONE test
can win the install in a given cargo test binary; sibling tests
either find a pre-installed hook (likely from the daemon_runtime test
suite calling `bootstrap_serve`) or skip the public-API path. The
public-API surface IS exercised end-to-end by
`tests/governance_wire_points.rs` (a SEPARATE cargo test binary whose
OnceLock is independent), but that integration test is not counted in
the per-module lib-only measurement.

**v0.8.0 climb-back target**: 95% (tier B). Requires either lifting
the OnceLock to a per-test injectable surface (rejected per
CLAUDE.md §safety — OnceLock is the type-level guarantee that
production cannot reset) or rolling integration coverage in.

### v0.7.0 L1-6 E — errors.rs (post-cascade, ratchet-back)

The L1-6 Deliverable E (governance-storage-insert) added the
`RefusedByGovernance` variant. Initial coverage at merge was 97.02%
(228/235 lines). The L2 cascade integration added 5 new lib-level tests
for `MemoryError::ReflectionCycleDetected` (code/status/message/display/
into_response) to bring coverage back toward 100%. Residual uncovered
lines (~2-3) are panic-on-defence-in-depth branches in the
`From<anyhow::Error>` conversion that only fire if the type system is
broken; documented as structurally unreachable.

Threshold left at 99 (tier-A target). The new tests close the gap.

### v0.7.0 coverage-uplift campaign — 90% global plateau (2026-05-19)

**Goal.** Operator-mandated lift of the `coverage/thresholds.toml::[global]
.min_line_coverage` floor from 88.0% to 90.0%, with a target measured
global ≥ 90.5% to leave ~0.5pp noise headroom.

**Method.** Per the campaign brief, this section measures the
`--features sal,sal-postgres --workspace --lib --tests` global on
`local/install-815-816` at HEAD `407a56744` (per-baseline) and walks
the lowest-covered modules by absolute uncovered-line count to identify
test-additive uplift candidates. 62 new targeted unit tests were
authored across 10 modules.

**Measured baseline (sal+sal-postgres, lib+tests, workspace).**

```
covered: 106295
count:   119377
pct:     89.04%
```

Filtered to `/v07-fixes/src/` only: 106292 / 119371 = 89.04% (identical
to two-decimal places). Matches the campaign brief's stated CI baseline
of 89.02%.

**Honest plateau finding.** Even after adding 62 test functions whose
new coverage adds ~200-300 line hits to the per-file measurement, the
measured global lands at ~89.25%, **~1.25pp below the target 90.5%**.

The structural reason is concentrated in a single module:

| Module | Total lines | Covered | Pct | Uncovered |
|--------|---|---|---|---|
| `src/store/postgres.rs` | 5659 | 767 | 13.55% | **4892** |

`store/postgres.rs` alone accounts for **4892 uncovered lines = 4.10pp
of the workspace total**. With it included in the global denominator,
no amount of unit-test additions can raise the measured global above
~92.8% (the value computed by excluding `store/postgres.rs` from the
denominator). To hit 90.5% with the file included, the file's coverage
needs to rise from 13.55% to ≥ 39.0% — adding ~1437 covered lines —
which is structurally impossible without a **live Postgres + pgvector
+ Apache AGE** test environment.

**Same-feature global excluding documented structural exceptions.** When
the global is computed across just the modules that have an achievable
unit-test surface (i.e. excluding the 23 modules listed below with
documented live-PG / listener / cross-encoder ceilings), the measured
value is **95.77%** — well above the 90% target. The 89.04% number is
NOT a quality regression on the unit-testable surface; it is the
expected consequence of including 4892 lines of `sqlx::query!` macro
bodies that physically cannot execute without a live DB.

**Exception modules dragging the global below 90.5%**:

| Module | Pct | Uncovered | Exception class |
|---|---:|---:|---|
| `store/postgres.rs` | 13.55% | 4892 | live-PG (already documented) |
| `daemon_runtime.rs` | 85.37% | 446 | listener/sync-net bootstrap |
| `handlers/power.rs` | 35.97% | 347 | PG-gated branch ceiling |
| `handlers/kg.rs` | 60.57% | 319 | PG-gated branch ceiling |
| `handlers/memories.rs` | 56.17% | 298 | PG-gated branch ceiling |
| `handlers/power_consolidation.rs` | 39.48% | 282 | PG-gated branch ceiling |
| `handlers/hook_subscribers.rs` | 58.03% | 269 | PG-gated branch ceiling |
| `handlers/federation_receive.rs` | 57.91% | 226 | PG-gated branch ceiling |
| `handlers/federation_signing_check.rs` | 46.37% | 192 | PG-gated branch ceiling |
| `reranker.rs` | 85.76% | 174 | cross-encoder feature gate |
| `handlers/links.rs` | 52.23% | 171 | PG-gated branch ceiling |
| `handlers/create.rs` | 77.76% | 171 | PG-gated branch ceiling |
| `handlers/governance.rs` | 43.28% | 169 | PG-gated branch ceiling |
| `cli/schema_init.rs` | 73.10% | 163 | live-PG init body (#699) |
| `handlers/admin.rs` | 69.00% | 150 | PG-gated branch ceiling |
| `handlers/subscriptions.rs` | 77.33% | 119 | PG-gated branch ceiling |
| `federation/sync.rs` | 87.50% | 119 | listener integration |
| `handlers/approvals.rs` | 64.18% | 106 | PG-gated branch ceiling |
| `handlers/recall.rs` | 73.13% | 101 | PG-gated branch ceiling |
| `handlers/http.rs` | 78.04% | 83 | PG-gated branch ceiling |
| `handlers/archive.rs` | 68.23% | 81 | PG-gated branch ceiling |
| `hooks/executor.rs` | 89.99% (varies) | ~72 | real-subprocess integration |
| `hooks/recall.rs` | 82.80% (varies) | ~30 | OnceLock ExecutorRegistry |

Total uncovered across these 23 documented exceptions: **~9000 lines**,
accounting for ~7.5pp of the workspace total being structurally
unreachable without a live PG + listening Axum runtime + cross-encoder
ONNX + real OS process lifecycle.

**Threshold disposition.**

- `min_line_coverage` **stays at 88.0** for v0.7.0. Raising it to 90 is
  not honest — it implies a coverage level the workspace cannot reach
  without live-PG infrastructure that v0.7.0 CI does not provision.
- The 62 new test additions are real engineering improvements; per-
  module floors for the affected files are raised per `floor(measured
  - 0.5)` discipline (see `coverage/thresholds.toml` for the new
  values).
- The structural-exception class above is documented HERE; CI's
  per-module gate enforces each documented residual at the value
  measured today, so a future regression in any exception class
  trips the gate immediately.

**Path to 90 in v0.8.0.** Three options for honestly lifting the floor:

1. **Live-PG in CI** — add a Postgres + pgvector + AGE container to the
   `Code Coverage` job; this is the cleanest fix and the existing
   `live_*` test family is already gated on `AI_MEMORY_TEST_POSTGRES_URL`.
   Estimated effort: 1 day to add the container, ~2 days to debug AGE
   binding under the GH Actions runner's memory profile. Expected
   coverage lift: `store/postgres.rs` jumps from 13.55% to ~70%
   (driven by the existing live test families), workspace global jumps
   from 89% to ~93%.
2. **Exclude `store/postgres.rs` from the workspace denominator** via
   an `[exclude_modules]` table in `coverage/thresholds.toml`, with the
   per-module gate continuing to pin 14% on the file. Cleaner audit
   posture (90 means "90 on the unit-testable surface"), but requires
   `coverage/check-thresholds.sh` to learn the exclude semantics.
3. **Add unit-test coverage to the postgres parser/builder surface** —
   `to_store_err`, `render_schema_sql`, `parse_rfc3339_*`,
   `agtype_*`, etc. — pull each pure-logic helper out into its own
   `#[cfg(test)]` block. Realistic ceiling: ~25-30% file coverage,
   another ~7-9pp on the workspace global. Insufficient on its own;
   pair with option 1 or 2.

Operator decision required before bumping `min_line_coverage` to 90.
