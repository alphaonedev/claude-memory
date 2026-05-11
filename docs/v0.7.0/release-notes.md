# ai-memory v0.7.0 — `attested-cortex` (release notes)

> **Status (2026-05-09):** v0.7.0 is **release-pending Wave 1-4 cert**.
> The original `attested-cortex` epic shipped at commit `fcdd2a5` on
> 2026-05-06; the Round-2 multi-agent NHI sweep (PR #643 against
> `round-2-fixes`) closed 13 follow-on findings (F6-F18) including 3
> blockers; the v0.7.0 A2A campaign re-cert and the operator-directed
> postgres+AGE first-class scope expansion (Wave 1-4) are landing in
> the same v0.7.0 tag rather than a separate v0.7.0.1 / v0.7.1.
>
> **Tag-cut criterion:** two consecutive 100% GREEN A2A rounds against
> the binary built from `round-2-fixes` after Wave 1-4 lands, **with
> both droplets pointed at a shared postgres+AGE backend** (Wave 4
> live-on-postgres acceptance gate).

## Headline

v0.7.0 closes the `attested-cortex` epic — **69/69 tasks across 11 tracks**
(A/B/C/D/E/F/G/H/I/J/K) — and ships **postgres + Apache AGE as a
first-class storage backend** including live daemon support
(`ai-memory serve --store-url postgres://…`), full schema parity with
sqlite (v15 → v28 port), 6-factor recall scoring parity, link
migration, and a new `ai-memory schema-init` CLI verb.

The substrate becomes both **more articulate** (capabilities v3 with
pre-computed calibration strings, named loaders, 52% MCP-tool token
reduction on the full profile) and **cryptographically trustworthy**
(per-agent Ed25519 attestation with append-only `signed_events` audit
chain, sidechain transcripts with `memory_replay`, programmable
20-event hook pipeline, opt-in Apache AGE acceleration, K1/G1
namespace-inheritance enforcement, real permission system with
deny-first semantics, A2A maturity).

## What's new since v0.6.4

### Headline new capability — postgres+AGE first-class

- **`ai-memory serve --store-url postgres://…`** — daemon-level
  adapter selection. The full HTTP + MCP surface routes through the
  SAL trait; sqlite is the default, postgres is opt-in.
- **`ai-memory schema-init`** — new CLI verb that bootstraps a fresh
  postgres store, including the AGE projection (or `--skip-age`
  for the CTE fallback) and the v28 schema. Idempotent on rerun.
- **Schema parity v28 across both backends** — the 13 v0.7-alpha
  postgres-missing migrations (governance inheritance, webhook
  subscriptions, audit chain, transcripts, signed events, agent
  quotas, link `attest_level`, A2A correlation, smart-load veto, KG
  temporal-index v2, tier-promotion metadata, subscription DLQ,
  `consolidated_from_agents` array) are now ported.
- **`PostgresStore::link()` and `::register_agent()` implemented** —
  retire the two `UnsupportedCapability` errors that v0.7-alpha
  surfaced.
- **6-factor recall scoring parity** — postgres recall now applies the
  same `priority` / `access_count` / `confidence` / `tier_bonus` /
  `recency` factors sqlite has. Pinned by
  `tests/recall_scoring_parity.rs`.
- **`migrate.rs` walks `from.list_links()`** — KG migrations now carry
  edges, not just nodes.
- **AGE 1.5 + PG 16 cypher-binding harness fix** — test-side only;
  production code never hit it. Unblocks the parity test suite on
  AGE 1.5.0.
- **Documentation** — operator how-to ([`docs/postgres-age-guide.md`](../postgres-age-guide.md))
  and migration runbook ([`docs/migration-v0.7.0-postgres.md`](../migration-v0.7.0-postgres.md)).

### Wave-3 Continuation 6 — F7 closure + mTLS-validated cert posture

- **Three new HTTP endpoints** close the Wave-4 cert-harness F7 gaps:
  - `POST /api/v1/quota/status` — `MemoryStore::quota_status` reads
    the `agent_quotas` table directly on postgres (no fallthrough to
    the empty scratch sqlite). Auto-inserts a default row on first
    call. Closes S61.
  - `POST /api/v1/kg/find_paths` — `MemoryStore::find_paths` lifts
    the SQLite recursive-CTE / Postgres AGE-Cypher-or-CTE path
    enumeration to the trait surface. Closes S65.
  - `POST /api/v1/links/verify` — `MemoryStore::verify_link` resolves
    the `(source, target?, relation?)` triple and re-verifies the
    canonical-CBOR signature against the enrolled peer key. Closes
    S52. Wire shape: `{verified, attest_level, signature_present,
    observed_by, source_id, target_id, relation, findings}`.
- **HTTPS / mTLS validated end-to-end.** The cert-closure run wires
  `--tls-cert`, `--tls-key`, and `--mtls-allowlist` flags into the
  daemon's systemd unit and exercises the full campaign from the
  cert harness with `TLS_MODE=mtls` + per-agent client certs. The
  `tls_handshake` block on each scenario report captures min/mean/max
  handshake durations so operators can quantify the perf overhead of
  switching from plain HTTP. See [`docs/postgres-age-guide.md` §
  HTTPS / mTLS configuration](../postgres-age-guide.md#https--mtls-configuration).
- **Test harness — per-agent client cert plumbing.**
  `Harness.client_cert_for(agent_id)` resolves
  `TLS_CLIENT_CERT_<stem>` / `TLS_CLIENT_KEY_<stem>` env vars per
  agent so each scenario authenticates as its caller. Each HTTP
  request emits curl `time_appconnect` / `time_connect` markers so
  the JSON report carries authoritative per-handshake timings.
- **Deploy script** (`scripts/deploy_wave4.sh`) gains an opt-in
  `DEPLOY_TLS=1` mode that distributes certs from `/tmp/a2a-v07-tls/`
  to each droplet's `/etc/ai-memory-a2a/tls/` and rewrites the
  systemd `ExecStart=` line idempotently.

### Track-level rollup (the original epic, unchanged)

- **Track A — Capabilities v3 response shape (5 tasks).** Adds
  `summary`, `to_describe_to_user`, `callable_now`,
  `agent_permitted_families` to the `memory_capabilities` response,
  plus `schema_version="3"` (additive over v2). Pre-computed per-agent
  calibration strings let LLMs converge on accurate first-answer
  descriptions instead of improvising.
- **Track B — Loader tools (5 tasks).** `memory_load_family` and
  `memory_smart_load(intent)` are promoted to **always-on first-class
  tools** (no longer hidden inside an introspection tool's parameter
  set). Includes harness detection from MCP `clientInfo` for the 11
  supported harnesses (Claude Code, Codex CLI, Grok CLI, Gemini CLI,
  Continue, Cursor, Cline, Aider, Goose, Claude Desktop, generic
  JSON-RPC).
- **Track C — Schema compaction (5 tasks).** **52% MCP tool-token
  reduction** on the full profile. Hard CI gate enforces ≤ 3,500
  input tokens for `--profile full` `tools/list`.
- **Track D — Per-harness positioning + tests (4 tasks).** Cross-harness
  benchmark; landing-page compatibility matrix; install-time
  system-prompt snippet; harness integration tests.
- **Track E — Discovery Gate T0 calibration cells (3 tasks).** Loader
  cells; T0 orchestration script; post-ship convergence verification.
- **Track F — Docs + release (6 tasks).** Migration guide, what's-new
  page, RFC, README updates, top-nav badges, this release-cut PR.
- **Track G — Hook Pipeline (11 tasks).** 20 lifecycle event types;
  `ExecExecutor` + `DaemonExecutor`; decision types
  (`Allow`/`Deny`/`Modify`/`Defer`); chain ordering; per-event
  timeouts; hot reload on `hooks.toml` mtime change;
  `on_index_eviction`; reranker batching; `pre_recall` daemon-mode
  hook; **R3 auto-link reference detector** as a reference hook
  binary; **R5 `pre_store` transcript-extraction reference hook**.
- **Track H — Ed25519 Attested Identity (6 tasks).** `ai-memory
  identity generate` CLI; outbound link signing; inbound signature
  verification on every link write; `attest_level` enum; `memory_verify`
  MCP tool; **append-only `signed_events` audit table** with
  hash-chained provenance.
- **Track I — Sidechain Transcripts (5 tasks).** `memory_transcripts`
  schema (BLOB + zstd-3); `memory_transcript_links` join table;
  per-namespace TTL; `memory_replay` MCP tool; **R5 `pre_store`
  transcript-extraction reference hook**.
- **Track J — Apache AGE Acceleration (8 tasks).** AGE detected at
  Postgres-SAL connect-time via `pg_extension` probe; Cypher
  implementations of `kg_query`, `kg_timeline`, `kg_invalidate`, and
  **R2 `find_paths`**; dual-path tests gated on
  `AI_MEMORY_TEST_AGE_URL`; AGE / CTE per-query performance budgets;
  `KgBackend { Cte, Age }` enum exposed via `Capabilities`.
- **Track K — A2A + Permissions + G1 cutline (11 tasks).** **K1/G1
  namespace-inheritance enforcement**; `pending_actions` timeout
  sweeper; `permissions.mode` enforcement gate (defaults to `enforce`
  per F8 fix); approval-event routing; A2A correlation IDs + ACK
  retries + TTL + replay protection; subscription DLQ + replay-from-cursor
  + HMAC; per-agent quotas with daily reset; unified permission
  pipeline; approval API on **HTTP + SSE + MCP** with HMAC and
  `remember=forever`; `ai-memory governance migrate-to-permissions`
  translator CLI.

### Round-2 NHI sweep findings (F1-F18, all closed in v0.7.0)

The v0.7.0 A2A campaign and the parallel post-ship NHI Round-2 sweep
surfaced 18 findings; all 18 are closed in the v0.7.0 ship.

| ID | Severity | Title | Status |
|---|---|---|---|
| F1 | P1 | namespace_owner doesn't walk parent chain — deep-child Owner write 403s | Closed (commit `e0d2086`, issue #644) |
| F2 | P1 | audit `sequence` resets to 1 across daemon restart | Closed (commit `e0d2086`, issue #645) |
| F3 | P3 | S70 import CLI flag drift (test-side) | Closed |
| F4 | P3 | `Harness.node_db_path()` helper for multi-droplet topology | Closed |
| F5 | P3 | AGE perf gate documentation | Closed |
| F6 | P3 | postgres SQL views + migrate-links + schema-init CLI surfaces | **Closing in v0.7.0 via Wave 1-4** (issue #646) |
| F7 | BLOCKER | HTTP `POST /memories` bypasses `agent_quotas` | Closed (commit `f9ef40a`) |
| F8 | SECURITY | `permissions.mode` defaults to `advisory` — flipped to `enforce` | Closed (commit `579afe2`, `63c46ab`) |
| F9 | release-notes | HTTP missing-required field returns 422 not 400 | Closed (commit `f9ef40a`) |
| F10 | release-notes | Embedder timeout silently produces un-indexed row at 201 | Closed (commit `f9ef40a`) |
| F11 | release-notes | `forget --pattern X` without `--namespace` is GLOBAL — `--confirm-global` now required | Closed (commit `579afe2`, `bd01978`) |
| F12 | release-notes | Ed25519 keypair NOT auto-generated on `serve` startup | Closed (commit `579afe2`, `63c46ab`) |
| F13 | release-notes | `memory_capabilities` schema/behavior drift | Closed (commit `66f48ae`) |
| F14 | release-notes | Smart-load router under-weights underscore tokens | Closed (commit `66f48ae`, `5b36d7c`) |
| F15 | release-notes | MCP `memory_store`/`memory_update` missing `metadata` in `inputSchema` | Closed (commit `66f48ae`) |
| F16 | release-notes | `agent_type` MCP enum closed but daemon permissive | Closed (commit `66f48ae`) |
| F17 | release-notes | `find_paths` `max_depth` cap; directed vs undirected docs | Closed (commit `082c999`, `f02d092`) |
| F18 | release-notes | `check_duplicate` similarity caps at ~0.92 for byte-identical strings | Closed (commit `082c999`, `63c46ab`) |

### Round-2-fixes folding (2026-05-11) — items originally triaged for v0.7.0.1, now in v0.7.0

Operator directive 2026-05-11: there will be no v0.7.0.1 patch release.
The following items fold into v0.7.0 directly.

| ID | Severity | Title | Status |
|---|---|---|---|
| #318 | high | MCP stdio writes bypass federation fanout | Closed in v0.7.0 — opt-in `mcp_federation_forward_url` forwards MCP `memory_store` to local HTTP daemon which runs `broadcast_store_quorum` |
| #355 | low | rustls-pemfile RUSTSEC-2025-0134 (unmaintained, transitive via axum-server) | Closed in v0.7.0 — `axum-server 0.7 → 0.8`; `cargo audit` clean |
| #507 | medium | `config.toml` `db = "~/..."` not expanded | Closed in v0.7.0 — `expand_tilde` helper in `AppConfig::effective_db` |
| #625 | low | E1/E2 orchestration scripts ported from bash to Rust binaries | Closed in v0.7.0 — `tools/t0-orchestrate/` + `tools/post-ship-converge/` crates; bash deleted; `#![cfg(unix)]` gates dropped |

Plus three v0.7.0 cert-driven fixes surfaced by Plan C R4:

- **L15 entrypoint wire** — `entrypoint.plan-c.sh` writes
  `auto_tag_model = "gemma3:4b"` to `config.toml` so auto_tag runs
  fast (~0.7s) instead of Gemma 4 e4b's thinking-mode 30+s
  timeout. Closes R4 S67 regression.
- **Postgres SAL `consolidate` upsert** — was a plain INSERT,
  exploded with `duplicate key value violates unique constraint
  "memories_title_ns_uidx"` on cert re-runs against a persistent
  postgres database. Rewrote as `ON CONFLICT (title, namespace)
  DO UPDATE` matching the adapter's standard upsert contract.
  Closes R4 S5 regression.
- **No-sal `federation.rs` build break** — `spawn_catchup_loop`
  unconditionally called `#[cfg(feature = "sal")]`-gated
  `spawn_catchup_loop_with_store`. Cfg-branched the body so the
  sqlite-only build compiles.

### Quality

- **Hard coverage gate ≥ 93%.** CI fails any PR below the line floor.
- **Clippy `-D pedantic` clean baseline** restored across nine files
  (#614).
- **Test race fixes** for the subscription `dispatch_count` race, the
  snippet env race, the keypair env race, the binary-spawn flake on
  macOS (OnceLock + PID-scoped target), and the b3 budget race.
- **52% MCP tool token reduction** on the full profile (Track C),
  measured against `cl100k_base`.
- **CI token budget gate** — hard 3,500-token ceiling on
  `--profile full` `tools/list` (Track C5).
- **A2A regression suite** — 76 scenarios consolidating ai2ai-gate
  v0.6.x baseline + v0.7.0 net-new + postgres+AGE substrate. Cert
  acceptance is two consecutive 100% GREEN rounds.

## Backward compatibility

- **MCP wire shape.** v3 capabilities are **additive** over v2; existing
  v0.6.4 SDKs continue to work against a v0.7.0 server.
- **5-tool default surface** is unchanged from v0.6.4 — `ai-memory mcp`
  still advertises `memory_store`, `memory_recall`, `memory_list`,
  `memory_get`, `memory_search` plus the always-on
  `memory_capabilities` bootstrap.
- **Hook pipeline** is **default off** — a v0.7.0 install with no
  `hooks.toml` behaves identically to v0.6.4 at the lifecycle layer.
- **Postgres backend** is opt-in. `ai-memory serve` without
  `--store-url` continues to use sqlite. Default builds without
  `--features sal-postgres` are unchanged byte-for-byte.
- **Schema migrations** v20 → v28 run automatically on first start of
  a sqlite-backed daemon and are idempotent. Postgres schema
  bootstrap is via `ai-memory schema-init` per the migration guide.

## Breaking changes

The v0.7.0 ship has **two intentional behavior changes** over v0.6.4
that may affect existing deployments:

### F8 — `permissions.mode` flips from `advisory` to `enforce`

**Before (v0.6.4 / v0.7.0-alpha):** fresh deploys had no write
enforcement by default — a security default-bad. The Round-2 NHI sweep
flagged this as a SECURITY DECISION.

**After (v0.7.0 ship):** `permissions.mode` defaults to `enforce`.
Operators who relied on the old default-permissive behavior must
opt back in explicitly:

```toml
# config.toml
[permissions]
mode = "advisory"
```

The first `ai-memory serve` boot prints a one-time migration banner
explaining the change.

### F11 — `forget --pattern` and `forget --tier` without `--namespace` require `--confirm-global`

**Before:** `ai-memory forget --pattern foo` silently deleted matching
memories across **all** namespaces.

**After:** the same command refuses to run without an explicit
`--confirm-global` flag. `--namespace`-scoped forget is unchanged.

```bash
# v0.6.4 behavior — global delete (now refused):
ai-memory forget --pattern 'PII:.*'

# v0.7.0 — must be explicit:
ai-memory forget --pattern 'PII:.*' --confirm-global
```

## Upgrade path

### From v0.6.4 (sqlite, staying on sqlite)

1. Backup `~/.local/share/ai-memory/memory.db`.
2. Install v0.7.0 (`brew upgrade ai-memory` / `cargo install ai-memory`
   / your distro path).
3. First start auto-migrates v20 → v28 (transcripts, signed_events,
   audit chain, attest_level on memory_links, …). Watch the daemon
   log for `schema migration: v20 → v28 complete`.
4. Read `docs/MIGRATION_v0.7.md` for the v0.6.4 → v0.7.0 surface
   changes (permissions.mode, forget safety, new MCP tools).

### From v0.6.4 (sqlite, switching to postgres)

Follow [`docs/migration-v0.7.0-postgres.md`](../migration-v0.7.0-postgres.md):

1. Provision postgres + Apache AGE + pgvector per
   [`docs/postgres-age-guide.md`](../postgres-age-guide.md).
2. `ai-memory schema-init --store-url postgres://…`.
3. `ai-memory migrate --from sqlite:///… --to postgres://… --dry-run`.
4. Real migration; verify row counts + content fingerprint.
5. Re-point the daemon at postgres via `--store-url` or
   `AI_MEMORY_STORE_URL`.
6. Confirm `/api/v1/capabilities` reports `store_backend: PostgresStore`
   and `kg_backend: Age`.

### From v0.7-alpha (postgres at schema v15)

1. `ai-memory schema-init --store-url postgres://… --upgrade` to walk
   v15 → v28 idempotently.
2. Restart the daemon.
3. (Optional) Re-run the migration tool to backfill links if your
   v0.7-alpha migration predated the Wave 1 link-walk fix:
   `ai-memory migrate --from sqlite:///… --to postgres://… --since
   <ISO8601>` — only the delta migrates.

## Operator references

- **Operator how-to:** [`docs/postgres-age-guide.md`](../postgres-age-guide.md)
- **Migration runbook:** [`docs/migration-v0.7.0-postgres.md`](../migration-v0.7.0-postgres.md)
- **Adapter-selection design:** [`docs/RUNBOOK-adapter-selection.md`](../RUNBOOK-adapter-selection.md)
- **What's new (visual):** [`docs/whats-new-v07.html`](../whats-new-v07.html)
- **v0.7.0 → v0.6.4 surface delta:** [`docs/MIGRATION_v0.7.md`](../MIGRATION_v0.7.md)
- **RFC (design rationale):** [`docs/v0.7/rfc-attested-cortex.md`](../v0.7/rfc-attested-cortex.md)
- **A2A campaign Pages:** https://alphaonedev.github.io/ai-memory-a2a-v0.7.0/
- **Test Hub Pages:** https://alphaonedev.github.io/ai-memory-test-hub/

## Tracking issues + PRs

- Master tracking: [#637](https://github.com/alphaonedev/ai-memory-mcp/issues/637)
- F1 (closed): [#644](https://github.com/alphaonedev/ai-memory-mcp/issues/644)
- F2 (closed): [#645](https://github.com/alphaonedev/ai-memory-mcp/issues/645)
- F6 (closing via Wave 1-4): [#646](https://github.com/alphaonedev/ai-memory-mcp/issues/646)
- v0.7.0 expanded postgres+AGE scope tracker: filed alongside this
  release note (Wave 1-4 closure anchor).
- Round-2 fixes PR: [#643](https://github.com/alphaonedev/ai-memory-mcp/pull/643)
  on `round-2-fixes`.

## Acknowledgements

The Round-2 NHI sweep was driven by a 5-agent parallel orchestration
against the live v0.7.0-alpha binary on a multi-droplet DigitalOcean
topology. The expanded postgres+AGE scope was driven by a 3-stream
parallel implementation under PR #643. The full A2A campaign artifact
trail is at https://alphaonedev.github.io/ai-memory-a2a-v0.7.0/.

— AlphaOne LLC, 2026-05-09
