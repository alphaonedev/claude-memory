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

## Per-module status (post-Phase-B re-measure baseline)

Numbers below are taken from the `coverage/tier-classification.toml`
baseline; this file is the working record for the per-module
disposition after L0.7-6 lands.

| Module                          | Baseline | Disposition                                  |
|---------------------------------|----------|----------------------------------------------|
| `daemon_runtime.rs`             | 87.2%    | Closing gap with router + bootstrap tests.   |
| `embeddings.rs`                 | 91.6%    | Above target; pinning with format-error tests. |
| `handlers/mod.rs`               | 98.8%    | Above target.                                |
| `handlers/transport.rs`         | 61.9%    | Closing gap with auth + percent-decode + state tests. Listener / Axum HTTP runtime: integration-only. |
| `harness.rs`                    | 99.2%    | Above target.                                |
| `hnsw.rs`                       | 95.3%    | Above target.                                |
| `storage/connection.rs`         | 100.0%   | Above target; pinning R1-M2 trigger probe.   |
| `store/mod.rs`                  | 42.2%    | Closing gap with trait-default + enum tests. |
| `store/postgres.rs`             | 13.9%    | EXCEPTION: requires live Postgres; ship-gate compensation. |
| `store/sqlite.rs`               | 57.5%    | Closing gap with link / archive / governance tests. |
| `tls.rs`                        | 94.7%    | Above target; ironclaw-mtls cell compensation for residual mTLS handshake. |

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

### `src/daemon_runtime.rs` — bootstrap-only paths (PARTIAL EXCEPTION)

`run()`, `serve_http_with_shutdown*`, `serve()`, and the panic-handler
setup at the top of `serve()` only fire under a real process. They are
exercised by every ship-gate cell that boots the daemon.

**Ship-gate compensation**:

- Every ship-gate cell starts a real `ai-memory serve` and exercises
  the daemon under load.

**Unit-testable surface kept under per-module gate**:

- `is_write_command`, `passphrase_from_file`, `apply_anonymize_default`,
  `build_embedder` keyword/load-failure paths, `build_vector_index`
  empty/populated paths, `spawn_*_loop` smoke tests, `build_router`
  shape, `bootstrap_serve` smoke + federation variants.

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
