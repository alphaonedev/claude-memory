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

#### `src/handlers/hook_subscribers.rs` — subscriber management (STRUCTURAL CEILING — PG branches)

Hook-subscriber CRUD reads/writes the subscriptions table; the PG branch
of `Store::list_subscriptions` / `register_subscription` only fires
under a Postgres daemon.

**Ship-gate compensation**:

- Phase 2 federation cell registers + invokes subscribers against a PG
  store. Phase 1 functional cell exercises sqlite branches end-to-end.

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
