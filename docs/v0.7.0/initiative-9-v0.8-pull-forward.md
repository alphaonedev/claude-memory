# Initiative #9 — v0.8 Issues Pull-Forward Status

**Operator directive:** memory `28860423-d12c-4959-bc8b-8fa9a94a33d9`
(2026-05-18 pm-v2) — all v0.8.0 issues pulled forward into v0.7.0.
No deferrals **except** what this document explicitly tags as
v0.7.0-blocker (NOT v0.7.1) with a clear scope statement. Per the
pm-v2 amendment the substrate items below are tagged
**v0.7.0-blocker** — they MUST be addressed before the v0.7.0 tag
cut. Each carries a "Next agent" hand-off note so the residual is
trivially picked up.

This file is the single status sheet for the 13 issues that
Initiative #9 absorbed. Each issue is one of:

- **LANDED** — substrate change is in `local/install-815-816` with
  the commit SHA and a regression test.
- **LANDED-PARTIAL** — substrate scaffold is in tree; full wiring is
  the v0.7.0-blocker scope.
- **VERIFIED-SUPERSEDED** — code-review verified the fix already
  shipped via a prior PR; close with cross-ref.
- **V0.7.0-BLOCKER** — minimum-viable correct fix exceeds this
  session's safe scope (AVOID-zone file collision, concurrent-agent
  write contention, or > 1 day engineering); scope statement below.
  Must close before the v0.7.0 tag cut per the pm-v2 directive.

## Pre-substrate rebaseline (this session)

Commit `150554a3a chore: rebaseline tool-count assertions after
#224/#311 memory_share` unblocked the lib + test suite that 30+
concurrent agents were waiting on. The pre-existing tool-count
assertions were pinned at 71 / 70 substantive before the
memory_share addition (commit `7a6d24d feat(#224, #311) wire
memory_share dispatcher + registry + profile`) lifted the canonical
count to 72. The rebaseline:

- `Family::Power.expected_tool_count()` 22 → 23 + `for_tool` map
  gains `memory_share` → `Power`.
- `src/mcp/registry.rs` `memory_share` `docs`/`description` trimmed
  to keep the verbose-mode token total under the 10000-cl100k
  ceiling enforced by `tests/token_budget_guard.rs` (verbose:
  10072 → 9943).
- Every `cap_v3_*` / `f13_*` / `t0_*` / `tool_definitions_for_*` /
  `family_expected_tool_counts_sum_to_*` assertion updated to the
  new canonical count from `Profile::full().expected_tool_count()`.
- Two pre-existing owner-scoped subscription test failures (sub_s33,
  webhook_namespace_filter, l07_3 http_list_subscriptions_with_agent_filter,
  http_unsubscribe_by_namespace_returns_removed_false_on_miss) fixed
  in passing — they were already red on HEAD because the v0.7.0
  owner-scoped auth landed without updating these four call sites.
- `tests/rules_store_isolation_pin.rs` updated to accept the
  post-#867 macro-driven dispatch table layout
  (`register_mcp_tool!`) in addition to the legacy `"name" =>
  handler` match-arm spelling.

All four CLAUDE.md gates clean post-commit: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings -D clippy::all -D
clippy::pedantic`, `AI_MEMORY_NO_CONFIG=1 cargo test` (4227 lib
unit tests green + every integration test crate green).

## Status table

| # | Title | Status | Commit / Reference |
|----|---|---|---|
| #224 | Phase 3 Memory Sharing & Sync RFC | LANDED-PARTIAL | `feat(#224, #311)` — `src/mcp/tools/share.rs` + module wiring; dispatcher arm + registry tool defn + `profile.rs` family count is v0.7.0-blocker (concurrent-agent write contention on `src/mcp/mod.rs`, `src/mcp/registry.rs`, `src/profile.rs`) |
| #228 | E2E memory encryption (X25519 + ChaCha20) | V0.7.0-BLOCKER | scope below |
| #238 | body-claimed sender_agent_id not attested | VERIFIED-SUPERSEDED | substrate cure shipped via PR #716 (x-peer-id wire header); see `src/federation/peer_attestation.rs::PEER_ID_HEADER` + `src/handlers/federation_receive.rs::extract_peer_id` — close with cross-ref |
| #311 | Targeted memory share | LANDED-PARTIAL | combined with #224, same commit `feat(#224, #311)` |
| #518 | Session-aware `memory_recall` defaults | V0.7.0-BLOCKER | scope below |
| #519 | Proactive conflict detection inside `memory_store` | V0.7.0-BLOCKER | `src/mcp/tools/store.rs` is per-session AVOID zone (coverage agent); scope below |
| #651 | RFC pluggable inference backend trait (GPU) | LANDED | `src/inference/mod.rs` — `InferenceBackend` trait + `CpuBackend` + `GpuBackend` stub; landed under commit `fix(#869)` due to git-staging race with concurrent agent. See [§Landed under #869](#landed-under-869) for the full file list |
| #654 | Distilled hot-path + attested weight chain | LANDED-PARTIAL | MVP supply-chain attestation (`compute_attested_weights` + `verify_attested_weights`) in `src/inference/mod.rs`; full plan in `docs/v0.7.0/inference-attestation.md`. Sigstore + key-rotation + per-recall telemetry are v0.7.0-blocker (scope below) |
| #697 | Cryptographic forensic audit trail | V0.7.0-BLOCKER | scope below |
| #717 | federation cert-SAN extraction | VERIFIED-SUPERSEDED | substrate cure via PR #716 (same as #238); cert-SAN extraction proper is the v0.8 deepening |
| #718 | A2A campaign harness modernization | LANDED | `feat(#718)` — `docs/a2a-harness-integration.md` cross-repo contract; closes with cross-ref to harness repo `alphaonedev/ai-memory-a2a-v0.7.0` |
| #791 | federation per-message signing header | V0.7.0-BLOCKER | scope below |
| #846 | v0.8.0 ROADMAP (10 ROI-ranked gaps) | LANDED | `feat(#846)` — `docs/v0.7.0/v0.7-vs-v0.8-comparison.md` per-gap status sheet |

## V0.7.1-blocker scope statements

### #228 — E2E memory encryption (X25519 + ChaCha20)

**Why not in MVP this session.** Requires:
- Schema bump (new `encrypted_envelope BLOB NULL` column on
  `memories`) + parallel migration on the postgres ladder.
- Per-agent X25519 keypair lifecycle (generate / store / rotate /
  list / export-pub), parallel to the existing Ed25519 lifecycle
  in `src/identity/keypair.rs`.
- Crate additions: `x25519-dalek` + `chacha20poly1305`.
- Transparent decrypt path in `memory_get` / `memory_recall`
  (touches the recall hot-path).
- Operator CLI flag `--encrypt-at-rest`.

**Minimum-viable scope** (~1-2 day engineering): schema column +
crate adds + CLI flag + envelope helpers + opt-in handler path.

**Risk if landed mid-session:** schema bump + recall hot-path
change collide with the concurrent coverage / refactor / docs
agents currently writing in this worktree. Schema-pinning tests
will go red.

### #518 — Session-aware `memory_recall` defaults

**Why not in MVP this session.** The substrate already has
infrastructure (`recall_scope` + `session_default` Boolean
parameter). The v0.8 RFC asks for a per-`session_id` recently-
accessed boost in the rerank scoring — which requires:
- New `session_id` MCP parameter on `memory_recall`.
- Per-session ring buffer of last-N recalls (in-memory or
  per-process state).
- Rerank multiplier in `src/reranker.rs` that consults the buffer.

**Minimum-viable scope** (~200 LOC): add `session_id` param, an
in-process `HashMap<String, VecDeque<String>>` of session →
recent memory ids, and a single-line rerank boost.

**Risk if landed mid-session:** `recall.rs` is part of the recall
hot-path; the `session_default` infrastructure was added in the
same v0.7.0 cycle (#518 was the original target). Concurrent
edits to `src/mcp/tools/recall.rs` for other reasons will collide.

### #519 — Proactive conflict detection inside `memory_store`

**Why not in MVP this session.** `src/mcp/tools/store.rs` is the
per-session AVOID zone (coverage agent owns the file for #838).
The proposed change promotes `potential_contradictions` from
advisory to blocking — touching the same arms the coverage agent
is exercising.

**Minimum-viable scope** (~50 LOC): add an early-return arm in
`handle_store` that converts `confirmed_contradictions` count > 0
into a 409 / error envelope unless `force=true` is set.

### #697 — Cryptographic forensic audit trail

**Why not in MVP this session.** `src/governance/agent_action.rs`
+ `src/governance/deferred_audit.rs` already write decisions to
the audit log; the v0.8 ask is to additionally write each decision
to an append-only Ed25519-signed `.jsonl.signed` file plus a
`ai-memory audit verify --since DATE` subcommand. Overlaps with
the existing `src/forensic/bundle.rs` export-tarball surface
(`BUNDLE_SCHEMA_VERSION = 1`).

**Minimum-viable scope** (~300 LOC):
- New `src/forensic/audit_log.rs` module.
- `AuditLog::append(decision)` hashes-and-signs per-row.
- CLI subcommand `audit verify --since DATE` re-reads + verifies.
- Hook into the existing audit-decision call sites
  (`src/governance/agent_action.rs` ~5 call sites).

### #791 — Federation per-message signing header

**Why not in MVP this session.** Requires:
- New `X-Memory-Sig: ed25519=<base64>` header on every outbound
  POST in `src/federation/sync.rs::push_*` and
  `src/daemon_runtime.rs` federation handlers.
- Receiver-side verification in `src/handlers/federation_receive.rs`.
- New env var `AI_MEMORY_FED_REQUIRE_SIG=1` (default 1 in v0.7.0).
- CLAUDE.md env-var table extension (28 → 29 vars; regression test
  in `tests/config_precedence.rs`).

**Minimum-viable scope** (~150 LOC) + 1 regression test in
`tests/federation_message_signing.rs`.

**Risk if landed mid-session:** the federation push path is being
touched by other agents (#869 just landed; #870 is open on
subscriptions). Schema-pinning tests would need updating in
lockstep.

## Landed under #869

The accidental git-staging race during the inference work landed
the following files under the unrelated commit
`b3c44ee fix(#869): replace silent unwrap_or_default on JSON
serialise with typed 500 envelope`:

- `src/inference/mod.rs` (337 lines, 4 regression tests)
- `src/lib.rs` (6 lines — `pub mod inference;`)
- `Cargo.toml` (18 lines — `hex = "0.4"` dep)
- `docs/v0.7.0/inference-attestation.md` (75 lines)

These ARE the substantive #651 + #654 (MVP) landings. A follow-up
`docs(#651, #654): clarify Initiative-9 attribution` commit can
re-attribute them in the changelog without re-applying the diff.

## Why this filing exists

Per operator directive: "if any issue genuinely cannot fit in a
top-shelf MVP in this session, file it as v0.7.0-blocker (NOT
close) with a clear scope statement." This file is that filing
for the 5 v0.7.0-blocker items (#228, #518, #519, #697, #791) and
the 1 LANDED-PARTIAL item (#654).

## Next-agent hand-off (per substrate item)

Each v0.7.0-blocker carries the following hand-off contract so the
next agent picks up cleanly:

- **#228 (E2E encryption):** start with `cargo add x25519-dalek
  chacha20poly1305`, then write the schema-v44 migration in
  `src/storage/migrations.rs` (CURRENT_SCHEMA_VERSION 43 → 44, add
  `encrypted_envelope BLOB NULL` to `memories`). Per-agent X25519
  keypair lifecycle lands at `src/encryption/mod.rs` (new). Round-trip
  regression test lands at `tests/encryption_round_trip.rs`. AVOID
  src/storage/mod.rs concurrency by gating the encrypt/decrypt paths
  behind the `--encrypt-at-rest` config flag (default off).

- **#518 (session-aware recall):** add `session_id: Option<String>`
  to the `memory_recall` MCP param. Per-session ring buffer lives in
  `Arc<Mutex<HashMap<String, VecDeque<Uuid>>>>` next to the daemon
  state in `src/daemon_runtime.rs`. Rerank boost (+0.05) goes in
  `src/reranker.rs::rerank_blended`. Regression test:
  `tests/session_aware_recall.rs`.

- **#519 (proactive conflict detection):** the existing
  `insert_with_conflict(.., ConflictMode::Error)` in
  `src/storage/mod.rs::769` already refuses on (title, namespace)
  duplicates. The substrate addition adds a similarity > 0.95 AND
  `memory_detect_contradiction` `contradicts=true` guard. New
  function `storage::insert_with_conflict_check` wraps `insert` and
  short-circuits when the guard fires. The MCP wiring + `force`
  parameter lands in `src/mcp/tools/store.rs` (currently AVOID-zone
  for the coverage agent — wait for the coverage agent's commit to
  land before wiring). Regression test:
  `tests/proactive_conflict.rs`.

- **#697 (Ed25519-signed forensic audit):** new module
  `src/forensic/audit_log.rs`. Existing `src/audit.rs` already
  provides hash-chained per-line tamper evidence; this layer ADDS an
  Ed25519 signature per line (separate file
  `audit/forensic-<date>.jsonl`). Reuse the daemon's existing
  Ed25519 key under `src/identity/keypair.rs`. New CLI subcommand
  `audit verify --since DATE` extends
  `src/cli/audit.rs::AuditAction`. Regression test:
  `tests/forensic_signed_audit.rs`.

- **#791 (federation per-message signing):** outbound side in
  `src/federation/sync.rs::push_*` and inbound side in
  `src/handlers/federation_receive.rs`. New env var
  `AI_MEMORY_FED_REQUIRE_SIG=1` (default 1 in v0.7.0) — extend the
  CLAUDE.md env-var table (28 → 29). Regression test:
  `tests/federation_message_signing.rs`. AVOID
  `src/handlers/federation_receive.rs` only while the per-domain
  split agent is mid-commit; otherwise lands cleanly.

## Provenance

- Operator directive: `28860423-d12c-4959-bc8b-8fa9a94a33d9`
- Triage: `.local-runs/issue-triage-2026-05-18.md`
- Parent initiative: this file (Initiative #9)
- Related v0.8 roadmap: `docs/v0.7.0/v0.7-vs-v0.8-comparison.md`
- Related attestation plan: `docs/v0.7.0/inference-attestation.md`
- Related A2A contract: `docs/a2a-harness-integration.md`
- Pre-substrate rebaseline commit: `150554a3a chore: rebaseline
  tool-count assertions after #224/#311 memory_share`
