# ai-memory v0.7.0 — `attested-cortex` (release notes)

## Release procedure (operator-gated, post v0.7.0)

v0.7.0 separates CI verification from publish. `ci.yml` runs on every
push + PR + tag (lint, check matrix, feature gates, dockerfile-validate,
coverage). `release.yml` runs ONLY on explicit `workflow_dispatch` and
handles the actual 5-channel fanout (binary builds + GitHub Release +
crates.io + Homebrew tap + GHCR Docker + Fedora COPR).

To publish a tag:

```bash
# 1. Create the signed tag locally
git tag -s v<X.Y.Z> -m "..."

# 2. Push the tag — fires ci.yml verification only
git push origin v<X.Y.Z>

# 3. Wait for ci.yml to land GREEN (Check matrix is the release gate)

# 4. Manually trigger publish — operator-gated, intentional
gh workflow run release.yml \
  --repo alphaonedev/ai-memory-mcp \
  -f tag=v<X.Y.Z>
```

Pre-release tags (SemVer `-` suffix, e.g. `v0.7.0-rc.1`) auto-skip the
downstream stable channels (crates.io, Homebrew, Docker, COPR) so
operator dry-runs are safe.

This separation closes the historical gap where CI passing on a tag-push
auto-fired the entire publish pipeline. The act of releasing is now a
deliberate, named action — not a side effect of green tests.

---

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
(A/B/C/D/E/F/G/H/I/J/K) plus the grand-slam recursive-learning + Agent
Skills + L1-6 substrate-rules wave (and the V-4 closeout #698 cross-row
hash chain) — and ships **postgres + Apache AGE as a first-class storage
backend** including live daemon support
(`ai-memory serve --store-url postgres://…`), full schema parity with
sqlite (Wave 1-4 narrative v15 → v28 port; terminal v0.7.0 ship is
sqlite v34 / postgres v33 after L0.7 + L2 wave + V-4 closeout), 6-factor
recall scoring parity, link migration, and a new `ai-memory schema-init`
CLI verb.

The substrate becomes both **more articulate** (capabilities v3 with
pre-computed calibration strings, named loaders, 52% MCP-tool token
reduction on the full profile) and **cryptographically trustworthy**
(per-agent Ed25519 attestation with append-only `signed_events` audit
chain — with V-4 cross-row hash chain at v34 (#698) — sidechain
transcripts with `memory_replay`, programmable
25-event hook pipeline, opt-in Apache AGE acceleration, K1/G1
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

### Security hardening — federation red-team P2 closeouts

Two red-team #230 findings on `/api/v1/sync/*` are closed in v0.7.0
proper rather than deferred to v0.8.0:

- **[#238](https://github.com/alphaonedev/ai-memory-mcp/issues/238)
  Body-claimed `sender_agent_id` is now attested against the wire-
  level `x-peer-id` header**, with an operator-configured allowlist
  for legitimate cross-author claims. Mismatched claims return
  `403 sender_agent_id_mismatch`; a missing header returns
  `403 peer_id_header_missing`. Legacy peers can opt in to pre-v0.7.0
  behaviour via `AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1`. See
  [`docs/security/audit-trail-coverage.md` §9.1](../security/audit-trail-coverage.md#91-per-author-attestation-on-syncpush-v070-238).
- **[#239](https://github.com/alphaonedev/ai-memory-mcp/issues/239)
  `/api/v1/sync/since` now applies a per-peer namespace allowlist**
  to the projection before returning rows. Default-deny posture for
  peers without an operator-configured allowlist (empty page + WARN);
  legacy "full dump" posture preserved via
  `AI_MEMORY_FED_SYNC_TRUST_PEER=1`. Response envelope gains
  `excluded_for_scope: <count>` + `scope_status: …` for honest
  partial-view diagnostics. See
  [`docs/security/audit-trail-coverage.md` §9.2](../security/audit-trail-coverage.md#92-per-peer-namespace-scope-on-syncsince-v070-239).

**Cert-SAN extraction follow-up.** Today's mTLS substrate
(`FingerprintAllowlistVerifier`) pins client certificates by SHA-256
fingerprint but does not propagate the cert's SAN/CN to handler code
(axum-server 0.8 has no per-request extension surface for that).
v0.7.0 closes the substantive integrity gaps using the `x-peer-id`
header convention bound to fingerprints via operator deployment
runbook. The cryptographic-attestation surface (cert SAN ↔ peer-id
binding inside the verifier) lands in v0.8.0 — tracked as a follow-up
to #238/#239.

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
- **Track G — Hook Pipeline (11 tasks).** 25 lifecycle event types (20 Track G baseline + `pre_recall_expand` G10 + `pre_reflect`/`post_reflect` recursive-learning Task 6/8 + `pre_compaction`/`on_compaction_rollback` L1-7);
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

### Substrate-native recursive refinement (issue [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655))

ai-memory v0.7.0 ships **substrate-native recursive refinement with
cryptographic provenance and bounded depth**, alongside the broader
attested-cortex epic and the Anthropic dreaming research preview. An
agent reads one or more memories, synthesises a higher-order
reflection (a lesson, pattern, contradiction-resolution, etc.), and
persists it with cryptographic-grade provenance back to each source
it reflects on. The reflection memory is just another memory row —
the same recall, search, governance, federation, attestation, and
audit primitives apply to it. The recursion is what's new.

**Bounded by design — not by aspiration.** Reflection depth is
substrate-enforced, not application-enforced: every reflection write
goes through a single `db::reflect` substrate function that consults
`GovernancePolicy.max_reflection_depth` (per-namespace), falls back
to a compiled default of 3, and refuses any reflection whose
proposed depth exceeds the cap with a structured
`REFLECTION_DEPTH_EXCEEDED` error (HTTP 409). The cap is set in JSON
governance metadata so operators can tune it per namespace without
a schema migration. A per-namespace cap of `Some(0)` is a documented
kill-switch — every reflection refuses, regardless of depth — for
deployments that want to opt every namespace under that subtree out
of the primitive entirely. **No autonomous goal modification, no
model fine-tuning loops, no unbounded recursion.**

Concrete API hooks shipped in Tasks 1-4 of the epic (commits below;
Tasks 5-8 land on the same branch and roll up into this v0.7.0 tag):

- **New column** ([commit `f5d8a9e`](https://github.com/alphaonedev/ai-memory-mcp/commit/f5d8a9e), Task 1/8) —
  `memories.reflection_depth INTEGER NOT NULL DEFAULT 0` on SQLite
  (schema v29) and Postgres (`CURRENT_SCHEMA_VERSION 31`). The
  `Memory` struct gains the field with `#[serde(default)]` so v0.6.4
  federation peers continue to round-trip cleanly. UPSERT clauses on
  both adapters take `MAX(old, new)` so federation merges preserve
  the higher-depth signal.
- **New governance field** ([commit `630a6db`](https://github.com/alphaonedev/ai-memory-mcp/commit/630a6db), Task 2/8) —
  `GovernancePolicy.max_reflection_depth: Option<u32>` (pure JSON
  metadata; no schema bump). Accessor
  `effective_max_reflection_depth()` returns `3` when unset; `Some(0)`
  is the documented kill-switch.
- **New relation** ([commit `b51a3f3`](https://github.com/alphaonedev/ai-memory-mcp/commit/b51a3f3), Task 3/8) —
  `reflects_on` joins the canonical link relation set
  (`related_to` / `supersedes` / `contradicts` / `derived_from` /
  `reflects_on`). Directionality matches `derived_from`: the
  reflection row is `source_id`, the original being reflected on is
  `target_id`. `db::find_paths` auto-walks the new label — reflection
  chains surface naturally in chain-walk queries without further
  work.
- **New MCP tool** ([commit `3dc76f3`](https://github.com/alphaonedev/ai-memory-mcp/commit/3dc76f3), Task 4/8) —
  `memory_reflect` (Power family, tool count 51 → 52). Atomic insert
  of a reflection memory + N `reflects_on` link writes in a single
  transaction; any link-insert failure rolls back the entire write.
  Postgres parity via inherent `PostgresStore::reflect`.
- **New error code** (Task 4/8) — `MemoryError::ReflectionDepthExceeded
  { attempted: u32, cap: u32, namespace: String }`. HTTP status
  `409 CONFLICT`, code `REFLECTION_DEPTH_EXCEEDED`. The structured
  triple is what downstream auditors and hook emitters need without
  parsing error strings.

The relevant CHANGELOG block sits under the same v0.7.0 heading
("v0.7.0 recursive-learning add-on"). Conceptual model, depth-cap
rationale, directionality contract, and the
`find_paths` chain-walk behaviour are written up in
[`docs/RECURSIVE_LEARNING.md`](../RECURSIVE_LEARNING.md). The
reproducibility script is at
[`scripts/reproduce-recursive-learning.sh`](../../scripts/reproduce-recursive-learning.sh) —
a self-contained Bash demo that builds the release binary, inserts
three sample memories into a fresh sqlite DB under `.local-runs/`,
reflects on them at depth=1, recursively reflects up to depth=3
(the default cap), and demonstrates the refusal at depth=4 with a
clear `REFLECTION_DEPTH_EXCEEDED` verdict block.

#### Cryptographic provenance for cap refusals (Task 5/8)

Every `memory_reflect` call that would exceed the namespace's resolved
`max_reflection_depth` now appends a row to the append-only
`signed_events` audit table before the cap refusal propagates back to
the caller. The row carries `event_type = "reflection.depth_exceeded"`
and a canonical-CBOR (RFC 8949 §4.2.1) payload binding
`(agent_id, attempted, cap, namespace, source_ids, proposed_title,
created_at)` under a SHA-256 `payload_hash`. The row is written with
`attest_level = "unsigned"` (the substrate refusal is the operation
being audited; per-event Ed25519 signing of refusal records is a
separate Track-H Bucket-1.5 line item). The reflection's content body
is deliberately omitted from the payload — only enumerable provenance
fields are part of the signed bytes, so PII the caller may have placed
in `content` never enters the audit chain. Audit-write failures are
best-effort: logged at `WARN` (`target = "signed_events"`) but the
substrate cap refusal still propagates so the wire contract stays
unchanged for callers. ([commit `c61a05b`](https://github.com/alphaonedev/ai-memory-mcp/commit/c61a05b).)

#### Hook integration (Task 6/8)

The Track-G hook pipeline grows from 21 to 23 events with two new
`HookEvent` variants:

- **`pre_reflect`** — decision-class hook, `EventClass::Write`, 5-second
  deadline budget. Fires inside `db::reflect_with_hooks` step 4, BEFORE
  the depth-cap check evaluates and BEFORE the write transaction opens.
  A handler returning `Deny { reason, code }` short-circuits the
  reflection and propagates as
  `ReflectError::HookVeto`
  (`"REFLECTION_HOOK_VETO (code=<N>): <reason>"`) — distinct from the
  Task 5 cap refusal on the wire so callers can tell substrate-policy
  refusals apart from caller-policy refusals.
- **`post_reflect`** — notify-class hook, `EventClass::Write`, 5-second
  deadline budget. Fires inside `db::reflect_with_hooks` step 7, AFTER
  `COMMIT` succeeds. Post-handlers read the fully-durable reflection
  memory and its `reflects_on` links via the same connection — useful
  for notification fan-out, federation push, audit-side-channel sinks,
  and the v0.8.0 reflection-pass curator's bookkeeping path. Notify
  handlers cannot veto; return values are ignored beyond logging.

The pipeline event count is `21 → 23`, not `20 → 22` — the G10 hot-path
`pre_recall_expand` event had already raised the floor from 20 to 21
before Task 6 landed. Hook vetoes do **not** emit the Task 5
`reflection.depth_exceeded` audit row: caller-policy refusals carry
their own provenance via the hook's own audit channel, and conflating
them with substrate-cap refusals would dilute the audit signal. The
MCP-side wire-in of `hooks.toml` → `ReflectHooks` is deferred to G7+;
the v0.7.0 `memory_reflect` MCP handler ships an unreachable
`HookVeto` arm against that bridge so the wire surface is forward-
compatible without yet emitting hook events from the production
handler. ([commit `fbf093c`](https://github.com/alphaonedev/ai-memory-mcp/commit/fbf093c).)

### Substrate-Native Recursive Learning Grand-Slam (NEW)

> **Operator-level summary.** The v0.7.0 grand-slam wave extends the
> recursive-learning substrate primitive (issue [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655))
> from "an MCP verb that mints a reflection memory" into a complete
> substrate-native learning loop: a curator that reflects across a
> namespace asynchronously, federation-aware cross-peer depth
> bookkeeping, invalidation propagation, transcript replay union,
> procurement-grade forensic export, reflection-to-skill promotion,
> skill ↔ reflection composition, and a reflection-aware reranker
> boost. The L1-6 substrate rules-enforcement engine ships the
> operator-keypair-signed rule store and the bypass-impossibility
> test fleet. Schema bumps to v33 (L2 wave CHECK constraint on
> `memory_links.relation`) and then v34 (V-4 closeout #698: `signed_events`
> cross-row hash chain — `prev_hash BLOB` + `sequence INTEGER`) per the
> v0.7.1-fold decision (`05e0cb9a`). Postgres parity is at v33 (the V-4
> closeout maps to postgres v33 since the postgres ladder ran one step
> behind). The MCP tool count moves from 60 → 71 over the L2 wave + Batman Forms 1-6 + 7th-form + QW-1/2/3 closeout (the +8 over the original 63 narrative cover Forms/QW/L2 additions enumerated below); the
> full reflection narrative lives in
> [`docs/RECURSIVE_LEARNING.md`](../RECURSIVE_LEARNING.md), the
> Agent Skills surface in [`docs/agent-skills.md`](../agent-skills.md),
> and the forensic-export surface in
> [`docs/forensic-export.md`](../forensic-export.md).

#### Schema and tool-surface deltas

- **Schema v34 (sqlite) / v33 (postgres) — terminal v0.7.0 ship.** The
  L2 wave first bumped sqlite to v33 (`memory_links.relation` CHECK
  constraint promoted from v23 trigger to SQL-side CHECK covering
  `related_to | supersedes | contradicts | derived_from | reflects_on`);
  the V-4 closeout (#698) then added migration 0028
  (`signed_events.prev_hash BLOB` + `signed_events.sequence INTEGER`
  + UNIQUE index — the SQL-side cross-row hash chain that flips the
  V-4 validation from YELLOW to GREEN) for the final sqlite v34 floor.
  Postgres parity mirror lands at v33 (postgres ran one step behind
  the sqlite ladder). Per `05e0cb9a` v0.7.1-fold decision (no separate
  v0.7.1 release; both bumps land in the v0.7.0 tag).
  ([`src/storage/migrations.rs`](../../src/storage/migrations.rs)
  `CURRENT_SCHEMA_VERSION = 34`;
  [`src/store/postgres.rs`](../../src/store/postgres.rs)
  `CURRENT_SCHEMA_VERSION = 33`.)
- **MCP tool count 60 → 71** (post-grand-slam at HEAD `c9472c1`;
  authoritative count from `Profile::full().expected_tool_count()` in
  [`src/profile.rs`](../../src/profile.rs) and verified by
  `grep -oE '"memory_[a-z_]+"' src/mcp/registry.rs | sort -u | wc -l`).
  The L2 wave added three tools:
  `memory_dependents_of_invalidated` (L2-3 / #668),
  `memory_skill_promote_from_reflection` (L2-6 / #671), and
  `memory_skill_compositional_context` (L2-7 / #672). The L1-5
  Agent Skills substrate added 5 tools (`memory_skill_register`,
  `_list`, `_get`, `_resource`, `_export`) earlier in the grand-slam
  branch. The L2-2 federation-aware reflection coordination added
  `memory_reflection_origin`. The post-grand-slam Forms / QW wave
  added a further 8: `memory_atomise` (WT-1 / Form 2), `memory_ingest_multistep`
  (Form 3 / #756), `memory_calibrate_confidence` (Form 5 / #758),
  `memory_check_agent_action` + `memory_rule_list` (7th-form / #691),
  `memory_export_reflection` (QW-1), `memory_persona` +
  `memory_persona_generate` (QW-2), `memory_offload` + `memory_deref`
  (QW-3). See [`docs/agent-skills.md`](../agent-skills.md) for the
  Skills per-tool wire surface; the canonical post-grand-slam
  inventory lives in [`docs/internal/v070-feature-inventory.md`](../internal/v070-feature-inventory.md).

#### L1-6 substrate rules engine (issues [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691), [#693](https://github.com/alphaonedev/ai-memory-mcp/issues/693))

The v0.7.0 Option B substrate-authority foundation:

- **Operator-keypair-signed seed rules.** Every seeded rule
  (`R001..R004`) is signed with the operator's Ed25519 private key
  via the new `ai-memory rules sign` CLI; the daemon verifies the
  signature on load and refuses to start when a rule's
  `attest_level = "signed"` but the signature does not verify
  against the enrolled operator public key.
  ([`src/cli/rules.rs`](../../src/cli/rules.rs),
  [`src/governance/rules_store.rs`](../../src/governance/rules_store.rs).)
- **Bypass-impossibility integration tests.** A dedicated test
  fleet (`tests/governance/`, commit
  [`6038f85`](https://github.com/alphaonedev/ai-memory-mcp/commit/6038f85))
  exercises every adapter write path that goes through the substrate
  `storage::insert` pre-write hook and asserts the rule corpus is
  consulted on each call. The fleet is the regression anchor for
  the v0.8.0 "100% coverage" epic.
- **MCP read-only inspection.** `memory_rule_list` and
  `memory_check_agent_action` provide structured read access to the
  rule corpus and a dry-run rule check. Per design revision
  2026-05-13, **mutation is operator-only** via CLI/HTTP with the
  signed operator key — the MCP surface cannot add, remove, enable,
  or disable rules.
- **Pre-write hook on `storage::insert`.** L1-6 Deliverable E
  ([commit `1b877ce`](https://github.com/alphaonedev/ai-memory-mcp/commit/1b877ce),
  [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691))
  wires `check_agent_action` into the `storage::insert` pre-write
  path. The HTTP handler surfaces the structured refusal via the
  new `RuleRefused` error variant
  ([`src/errors.rs`](../../src/errors.rs)). Other adapter write
  paths (link insert, consolidate, reflect, federation receive)
  continue to enforce reflection-specific authority via the
  existing reflection-depth cap; the v0.8.0 epic
  ([#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697))
  is where the rule engine wires into 100% of write paths.

**Audit-honest framing.** Substrate authority is a **foundation in
v0.7.0, a complete cover in v0.8.0**. Operators evaluating the
authority claim today should read
[`docs/RECURSIVE_LEARNING.md` §Substrate authority claim](../RECURSIVE_LEARNING.md#substrate-authority-claim--v070-option-b-foundation)
alongside this section. Any "100% substrate authority" marketing
that elides the wiring gap is inaccurate.

#### L2 wave — what landed

| Task | Commit | Issue | Headline |
|---|---|---|---|
| L2-1 reflection-pass curator | [`c3f6e82`](https://github.com/alphaonedev/ai-memory-mcp/commit/c3f6e82) | [#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666) | Asynchronous curator clusters `Observation`-kind memories and mints reflections via the substrate path. Opt-in per namespace; honors the substrate cap; one level of reflection per pass. |
| L2-2 federation reflection coordination | [`0b1c9cc`](https://github.com/alphaonedev/ai-memory-mcp/commit/0b1c9cc) | [#667](https://github.com/alphaonedev/ai-memory-mcp/issues/667) | Cross-peer depth bookkeeping. Receivers stamp `metadata.reflection_origin = {peer_origin, original_depth, local_depth_at_arrival}` on import and enforce the **local** cap on derived writes — federation cannot launder depth. `memory_reflection_origin` MCP tool. |
| L2-3 invalidation propagation | [`3f419be`](https://github.com/alphaonedev/ai-memory-mcp/commit/3f419be) | [#668](https://github.com/alphaonedev/ai-memory-mcp/issues/668) | A Reflection→Reflection `supersedes` edge fires the walker; one notification memory is written per dependent under `<dependent.namespace>/_invalidations`. **Notification, NOT cascade.** Cascade rollback is v0.8.0 Pillar 2.5. |
| L2-4 transcript replay union | [`a50b34c`](https://github.com/alphaonedev/ai-memory-mcp/commit/a50b34c) | [#669](https://github.com/alphaonedev/ai-memory-mcp/issues/669) | `memory_replay` on a reflection memory returns the **union** of transcripts reachable by walking `reflects_on` to the source observations. Caller controls the walk depth (`depth=N`); `depth=0` reproduces the pre-L2-4 shape. |
| L2-5 forensic bundle | [`bb870b3`](https://github.com/alphaonedev/ai-memory-mcp/commit/bb870b3) | [#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670) | `ai-memory export-forensic-bundle` + `verify-forensic-bundle`: deterministic POSIX-ustar tar, byte-identical mod timestamp, operator-signed when keypair is on disk. AgenticMem Attest tier integration. See [`docs/forensic-export.md`](../forensic-export.md). |
| L2-6 reflection-as-skill | [`505c538`](https://github.com/alphaonedev/ai-memory-mcp/commit/505c538) | [#671](https://github.com/alphaonedev/ai-memory-mcp/issues/671) | `memory_skill_promote_from_reflection` promotes a reflection (depth ≥ 1) to a SKILL.md-format Agent Skill. Each `reflects_on` source becomes a `references/source_{i}.md` resource. Round-trip digest-identical to a hand-authored SKILL.md. Closes the recursive-learning loop. |
| L2-7 skill composition | [`0966b57`](https://github.com/alphaonedev/ai-memory-mcp/commit/0966b57) | [#672](https://github.com/alphaonedev/ai-memory-mcp/issues/672) | `composes_with_reflections` SKILL.md frontmatter declares a skill's affinity for one or more reflection-bearing namespaces. `memory_skill_compositional_context` returns the body + bounded reflection set ranked by recency + recall_count. Per-namespace `max_reflection_depth` is the authoritative ceiling — composition cannot bypass the cap. |
| L2-8 reflection-aware reranker boost | [`90291c0`](https://github.com/alphaonedev/ai-memory-mcp/commit/90291c0) | [#673](https://github.com/alphaonedev/ai-memory-mcp/issues/673) | Reranker applies `boost * (1 + per_depth_increment * min(depth, cap))` to `Reflection`-kind memories AFTER the cross-encoder blend. Defaults `1.2` / `0.05` / `3`. `boost = 1.0` is the documented kill-switch. |

#### Agent Skills (Pillar 1.5)

The L1-5 Agent Skills ingestion substrate landed on the grand-slam
branch as the substrate path for
[agentskills.io](https://agentskills.io/)-compliant `SKILL.md`
modules:

- **7 MCP tools** in the `memory_skill_*` family. See
  [`docs/agent-skills.md`](../agent-skills.md) for the per-tool
  wire surface.
- **Round-trip digest guarantee** — register → export → re-register
  produces the identical SHA-256 digest. Survives transport,
  federation, and the v0.7 → v0.8 schema revision.
- **Ed25519 attestation** on every signed row when an operator
  keypair is on disk.
- **SKILL.md format** with `composes_with_reflections` frontmatter
  field (L2-7) — declares a skill's affinity for reflection
  namespaces with a per-entry `min_depth` floor.

The closing-loop bridge: a reflection memory ↔ a skill manifest.
Operators codify learnings into skills via
`memory_skill_promote_from_reflection`, then activate them on
demand with `memory_skill_compositional_context` returning a
bounded reflection context alongside the skill body.

#### Forensic export (AgenticMem Attest)

The L1-3 `verify-reflection-chain` and L2-5
`export-forensic-bundle` / `verify-forensic-bundle` triad ships the
procurement-grade evidence path. Full surface in
[`docs/forensic-export.md`](../forensic-export.md). Headlines:

- **Deterministic tar bundle.** Byte-identical mod timestamp.
- **In-process POSIX ustar.** No `tar` crate dependency.
- **Manifest carries per-file SHA-256 + optional Ed25519
  signature.** Auditor re-verifies with no daemon state and no
  network.
- **`AgenticMem Attest` evidence tier.** The OSS-side artefact
  pairs with the operator-keypair attestation chain to deliver the
  full Attest-tier evidence packet on demand.

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

## Post-grand-slam ship-readiness wave (2026-05-15)

After the original `attested-cortex` epic landed at `fcdd2a5` (2026-05-06)
and the Round-2 NHI sweep closed F1-F18, a final ship-readiness wave
folded into the v0.7.0 tag rather than slipping to v0.7.1:

- **Batman 6-form audit + Forms 1-6 + 7th-form closeout** (PRs
  [#761](https://github.com/alphaonedev/ai-memory-mcp/pull/761)-
  [#766](https://github.com/alphaonedev/ai-memory-mcp/pull/766),
  merged 2026-05-15). The
  [`docs/internal/batman-framework-audit.md`](../internal/batman-framework-audit.md)
  audit at commit `53b4d39` found 0/6 forms cleanly IMPLEMENTED + 4
  partial + 2 absent. The Forms wave closed every gap to **all 7 forms
  IMPLEMENTED at HEAD `c9472c1`**:
  - **Form 1 (online dedup-and-synthesis, issue [#754](https://github.com/alphaonedev/ai-memory-mcp/issues/754)).**
    Single-batch action-emitting LLM call on `memory_store`.
    `src/synthesis/mod.rs`. Opt back into the v0.6.x per-pair
    classifier via `legacy_per_pair_classifier = true` on the
    namespace standard.
  - **Form 2 (synchronous atomise-before-embed, issue [#755](https://github.com/alphaonedev/ai-memory-mcp/issues/755)).**
    `auto_atomise_mode = Synchronous` pre-store hook in
    `src/hooks/pre_store/auto_atomise.rs`. New `memory_atomise` MCP
    tool. Doc: [`docs/atomisation.md`](../atomisation.md).
  - **Form 3 (multi-step ingest orchestrator, issue [#756](https://github.com/alphaonedev/ai-memory-mcp/issues/756)).**
    `memory_ingest_multistep` threads deterministic helpers (Jaccard
    overlap, FTS classifier) before prompt-cache-stable LLM stages.
    `src/multistep_ingest/{mod,executor,helpers,pipeline,cache}.rs`.
    Doc: [`docs/multistep-ingest.md`](../multistep-ingest.md). Cookbook:
    [`cookbook/multistep-ingest/01-two-phase.sh`](../../cookbook/multistep-ingest/01-two-phase.sh).
  - **Form 4 (fact provenance, issue [#757](https://github.com/alphaonedev/ai-memory-mcp/issues/757)).**
    Citations + source-URI + atom-grain spans ride on existing
    `memory_store` / `memory_atomise` payloads. Schema migration
    `0032_v07_form4_provenance.sql`. Doc:
    [`docs/provenance.md`](../provenance.md).
  - **Form 5 (auto-confidence + shadow calibration + freshness decay,
    issue [#758](https://github.com/alphaonedev/ai-memory-mcp/issues/758)).**
    `memory_calibrate_confidence` MCP tool +
    `src/confidence/{mod,calibrate,shadow,decay}.rs`. Env vars
    `AI_MEMORY_AUTO_CONFIDENCE`, `AI_MEMORY_CONFIDENCE_SHADOW`,
    `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE`,
    `AI_MEMORY_CONFIDENCE_DECAY`. Schema migration
    `0033_v07_form5_confidence_calibration.sql`. Doc:
    [`docs/confidence-calibration.md`](../confidence-calibration.md).
  - **Form 6 (`MemoryKind` Batman vocabulary, issue [#759](https://github.com/alphaonedev/ai-memory-mcp/issues/759)).**
    10-variant `MemoryKind` enum (`Observation` default + 9 specific
    variants). Optional `auto_classify_kind` pre-store hook
    (`off | regex_only | regex_then_llm`). No CHECK constraint on
    `memories.memory_kind` — future variants land additively. Doc:
    [`docs/memory-kind-vocab.md`](../memory-kind-vocab.md).
  - **7th-form (agent-EXTERNAL Layer-4 wiring, issue [#760](https://github.com/alphaonedev/ai-memory-mcp/issues/760);
    full cover at v0.8.0 per [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)).**
    Option-B foundation: operator-keypair-signed seed rules
    `R001..R004`, `memory_check_agent_action` + `memory_rule_list`
    MCP tools, substrate `storage::insert` pre-write hook. Doc:
    [`docs/policy-engine.md`](../policy-engine.md) +
    [`docs/governance/agent-action-rules.md`](../governance/agent-action-rules.md).
- **QW (Tencent quick-wins).** Four QW items are referenced in the
  Tencent positioning work; **three carry code (QW-1/QW-2/QW-3); QW-4
  is a docs-only deliverable** (competitive positioning page +
  landscape comparison, no substrate or wire-surface change). Per
  [`docs/internal/v070-ship-readiness-adrs.md` ADR-1](../internal/v070-ship-readiness-adrs.md#adr-1--qw-4-disposition-docs-only-no-code-feature).
  - **QW-1** file-backed reflection chain export — `memory_export_reflection`
    + `auto_export_reflections_to_filesystem` namespace policy.
    Default destination `~/.ai-memory/reflections/<ns>/<id>.md`.
    Cookbook: `cookbook/file-backed-export/`.
  - **QW-2** persona-as-artifact — `memory_persona` + `memory_persona_generate`,
    `MemoryKind::Persona` rows, `auto_persona_trigger_every_n_memories`
    + `auto_export_personas_to_filesystem` namespace policy. Doc:
    [`docs/persona.md`](../persona.md). Cookbook: `cookbook/persona/`.
  - **QW-3** context offload primitive — `memory_offload` + `memory_deref`
    move large tool outputs out of the agent context window into an
    addressable blob store with a background TTL sweep. Doc:
    [`docs/context-offload.md`](../context-offload.md). Cookbook:
    `cookbook/context-offload/`.
  - **QW-4** *(docs-only — competitive positioning)* — Tencent
    landscape page at [`docs/positioning.md`](../positioning.md).
    Not a code feature; included for inventory completeness so a
    procurement reader counting "QW items shipped" against the
    Tencent analysis sees the same denominator.
- **Reconciliation security sweep (11 late-cycle commits, merged
  into trunk at `64528b1`).** K9 governance gate on `handle_kg_invalidate`
  (`a41c08f`), K10 SSE `host:` prefix bypass (`7496a6e`), K10 HMAC
  method+pending_id binding (`99ffacc`), K10 HMAC nonce single-use 300s
  window (`a69325f`), K10 SSE lagged-event count strip (`d1f6c9f`),
  SSRF IPv4-mapped-IPv6 + NAT64 (`3ab72dc`), `invalidate_link` BEGIN
  IMMEDIATE wrap (`2c77537`), hooks executor secret-redaction
  (`cbe934c`), H8 rebound-namespace `Ask` walk (`69ad41c`), I1
  zstd-decompression cap config-driven (`26fab06`). Pinned by
  [`tests/k10_approval_security.rs`](../../tests/k10_approval_security.rs),
  [`tests/i1_zstd_bomb.rs`](../../tests/i1_zstd_bomb.rs),
  [`tests/h2_invalidate_link_signed.rs`](../../tests/h2_invalidate_link_signed.rs).
- **Default tool surface.** The original v0.6.4 narrative said
  "5 default tools". The actual v0.7.0 `--profile core` surface is
  **7 tools** (`memory_store`, `memory_recall`, `memory_list`,
  `memory_get`, `memory_search`, `memory_load_family`,
  `memory_smart_load`) per `Family::Core.expected_tool_count()` in
  [`src/profile.rs`](../../src/profile.rs). The `memory_capabilities`
  bootstrap remains always-on regardless of profile.
- **Six new operator-focused docs landed alongside this wave:**
  [`docs/hook-pipeline.md`](../hook-pipeline.md),
  [`docs/federation.md`](../federation.md),
  [`docs/k8-quotas.md`](../k8-quotas.md),
  [`docs/k10-sse-approvals.md`](../k10-sse-approvals.md),
  [`docs/sidechain-transcripts.md`](../sidechain-transcripts.md),
  [`docs/signed-events-v4.md`](../signed-events-v4.md).
- **Canonical feature inventory.** The full post-grand-slam feature
  truth lives at [`docs/internal/v070-feature-inventory.md`](../internal/v070-feature-inventory.md)
  (453 commits ahead of v0.6.4, +233,589/−23,541 lines, 71 MCP tools,
  28 net-new since v0.6.4, 17 net-new `AI_MEMORY_*` env vars, 8 new
  HTTP routes, 20 sqlite + 10 postgres new migrations).

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
- **Schema migrations** v20 → v34 run automatically on first start of
  a sqlite-backed daemon and are idempotent (the Wave 1-4 v15 → v28
  port was the initial postgres+AGE land; subsequent in-flight v0.7.0
  work added v29-v30 for L0.7-1/L1-1 recursive-learning, v33 for the
  L2 wave `memory_links.relation` CHECK, and v34 for the V-4 closeout
  #698 `signed_events` cross-row hash chain). Postgres schema bootstrap
  is via `ai-memory schema-init` per the migration guide.

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
3. First start auto-migrates v20 → v34 (transcripts, signed_events,
   audit chain, attest_level on memory_links, recursive-learning
   `reflection_depth`, `memory_links.relation` SQL-side CHECK, and the
   V-4 closeout `signed_events.prev_hash` + `sequence` cross-row hash
   chain). Watch the daemon log for `schema migration: v20 → v34
   complete`.
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
   v15 → v33 idempotently (Wave 1-4 ported v15 → v28; subsequent
   L0.7 / L2 wave / V-4 closeout added v29-v33 on the postgres side).
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
- **Recursive learning primer:** [`docs/RECURSIVE_LEARNING.md`](../RECURSIVE_LEARNING.md)
- **Agent Skills primer:** [`docs/agent-skills.md`](../agent-skills.md)
- **Forensic export primer:** [`docs/forensic-export.md`](../forensic-export.md)
- **Curator soak runbook:** [`docs/RUNBOOK-curator-soak.md`](../RUNBOOK-curator-soak.md)
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
