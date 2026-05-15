# Cryptographic Forensic Audit Trail — Coverage Matrix

**Status as of branch `feat/v0.7.0-grand-slam` (HEAD `12a7f29`,
2026-05-14, post fold-J audit pass).** Updated to reflect that PE-1
(#694) / PE-2 (#695) / PE-3 (#696) all merged on the grand-slam branch
(commits `cb6cca9` / `5392162` / `07b4957` respectively).

This doc is the v0.7.0 honest single source of truth for the
cryptographic forensic audit trail. It is the substrate-side companion
to [`audit-trail.md`](./audit-trail.md) (which documents the on-disk
JSON audit log surface — a different and complementary subsystem).

Where this doc says "the chain", it means the SQLite/Postgres
`signed_events` table — the append-only, hash-chained, optionally
Ed25519-signed event store the substrate itself maintains. The
v0.8.0 epic (**#697**) drives this chain to 100% coverage.

Cross-references:

- **#691** substrate rules engine v2 (base layer)
- **#693** v0.7.0 Policy Engine Completion (Option B parent meta)
- **#694** PE-1 universal `AgentAction` wire-point coverage
- **#695** PE-2 Claude Code PreToolUse harness hook installer
- **#696** PE-3 deferred audit-log queue
- **#697** v0.8.0 100% Cryptographic Forensic Audit Trail closeout

Companion: [`docs/policy-engine.md`](../policy-engine.md).

---

## 1. Goal

The cryptographic forensic audit trail provides **tamper-evident
provenance for every substrate-visible action that crosses a
governance decision boundary**. A regulator or procurement auditor
can, given the database and the operator public key, walk the chain
end to end and verify:

- Every refusal verdict the engine produced.
- Every approval-API decision.
- Every reflection write with cross-peer provenance.
- Every schema migration the substrate applied.

Out of scope (closed by v0.8.0 **#697**): out-of-band agent actions
the substrate cannot see, hard-crash-lost rows in the deferred queue,
and read-action visibility. See §4.

---

## 2. Coverage matrix

| Event class | Current logging status at `c359e89` | `signed_events` row shape | Known gaps | v0.8.0 issue |
|---|---|---|---|---|
| Cross-row chain integrity | **Chain-logged today** (v0.7.0 V-4 closeout, #698) — every row carries `prev_hash` + `sequence`; [`verify_chain`](../../src/signed_events.rs) walks every row and flags chain breaks | `prev_hash BLOB` = SHA-256 over [`canonical_chain_bytes`](../../src/signed_events.rs) of the preceding row (ZERO_HASH for first); `sequence INTEGER` monotonic from 1, pinned by UNIQUE index | DELETE row N is detected at row N+1's prev_hash check; raw row-pruning operators must accept the documented chain break | — |
| Memory writes (`store` / `update` / `link` / `delete` / `archive` / `consolidate`) | **Chain-logged today** via `signed_events.append` (`src/signed_events.rs`) on every successful substrate write | `event_type = "memory.<verb>"`, `payload_hash` over canonical-JSON of the post-write row, `signature` (Ed25519 over `payload_hash`), `attest_level` ∈ {`unsigned`, `signed`} | none for the success leg | — |
| Reflection writes | **Chain-logged today** with `peer_origin` for cross-peer paths (L2-2 commit `2aef248`) | `event_type = "reflection.write"`, payload binds `(source_ids, depth, peer_origin)` | none | — |
| Governance refusals on agent-EXTERNAL surface (Bash / Write / Network / ProcessSpawn / Custom) via `check_agent_action` (audited path) | **Chain-logged today** synchronously, every call | `event_type = "governance.check"`, `payload_hash` over canonical `{action, decision}` JSON, `agent_id` carrier set | none | — |
| Governance refusals on substrate-INTERNAL pre-write hook (`check_agent_action_no_audit`) | **Chain-logged today** via PE-3 deferred queue (merged commit `07b4957`) | identical shape to the audited path — same canonical bytes / payload hash; emit deferred via tokio drain task in `src/governance/deferred_audit.rs::install_deferred_audit_drainer` | hard-crash drainer loss (process-local queue); see V08-PE-4 | **#697** V08-PE-4 closes durability (V08 closeout) |
| Approval-API decisions (L1-8) | **Chain-logged today** | `event_type = "approval.<decision>"`, binds approver identity + decision + correlation id | none | — |
| Schema migrations | **Chain-logged today** at boot | `event_type = "schema.migration"`, binds from-version + to-version + migration filename hash | none | — |
| Read actions (`memory_recall` / `memory_search` / `memory_list` / `memory_get` / `memory_session_boot`) | **NOT chain-logged** at engine level. Handler-layer `AuditAction::Recall` etc. row is emitted to the JSON audit log per [`audit-trail.md`](./audit-trail.md), but no `signed_events` row | n/a — v0.8.0 adds `event_type = "governance.read_check"` once V08-PE-2 lands | engine has no `AgentAction::Read` variant at HEAD | **#697** V08-PE-2 |
| Subprocess actions from Bash spawn chain (fork→exec under a permitted shell) | **NOT visible** to the engine at HEAD | n/a — v0.8.0 adds eBPF/dtrace surface and `event_type = "process.spawn_chain"` | invisible to the substrate without a kernel-side probe | **#697** V08-PE-3 |
| Out-of-band agent actions | **Unenforceable by definition** | n/a — substrate has no visibility | partial mitigations: V08-PE-1 mandatory-hook + V08-PE-6 TPM-bound binary integrity | **#697** V08-PE-1, V08-PE-6 |
| Hard-crash-lost deferred events | **Gap** — process-local queue | rows drop silently on SIGKILL between verdict and drain | persistent on-disk queue closes the gap | **#697** V08-PE-4 |

---

## 3. Reading the chain

Four operator-facing surfaces.

### 3.1 `ai-memory verify-signed-events-chain` (v0.7.0 V-4 closeout, #698)

Walks the SQL-side `signed_events` cross-row hash chain in
sequence-ascending order. For each row:

1. Verify the `sequence` column is `prior + 1` (first row: `1`).
2. Recompute `SHA-256(canonical_chain_bytes(row N-1))` and compare
   against row N's stored `prev_hash`. Mismatch flags a chain break
   at row N.
3. When `signature` is present and `attest_level = signed`, verify
   the Ed25519 signature against the operator-issued (or per-agent-
   issued) verifying key.

Exits 0 on chain GREEN; 1 on chain break. `--since <sequence>`
skips already-verified rows. `--format json` emits a
machine-parseable
[`signed_events::ChainVerificationReport`](../../src/signed_events.rs)
mirror.

The chain is the LOAD-BEARING tamper-evidence property of the SQL
substrate. Per-row Ed25519 signatures remain as defense-in-depth.
Implementation: `src/signed_events.rs::verify_chain` +
`src/cli/verify_signed_events.rs::run`. Schema columns:
`signed_events.prev_hash BLOB` + `signed_events.sequence INTEGER`
(SQLite v34 / Postgres v33).

### 3.2 `ai-memory verify-reflection-chain`

Walks `memory_links.reflects_on` edges backward from a target memory
to depth 0, verifies each Ed25519 signature, and emits a structured
chain-integrity report for the reflection ancestry. Distinct from
§3.1 (this surface walks edges, not the audit table).

### 3.3 `ai-memory export-forensic-bundle` (L2-5, commit `340367f`)

Produces a self-contained tarball: every `signed_events` row + the
in-scope reflection / link / approval rows + the operator pubkey + a
manifest. Designed to be handed to an external auditor without giving
them direct database access.

### 3.4 Raw `signed_events` query example

```sql
-- Every refusal verdict, newest first, for a given agent
SELECT id, agent_id, event_type, payload_hash, attest_level, timestamp
FROM signed_events
WHERE event_type = 'governance.check'
  AND agent_id = ?
ORDER BY timestamp DESC
LIMIT 100;

-- Refusal-only filter — decode the payload_hash row by row through
-- the canonical_bytes path; or pair with the `governance.check_dropped`
-- counter once V08-PE-4 lands.
```

The canonical-bytes recipe is stable across versions. A future audit
tool that wants to recompute hashes without the substrate binary can
follow `governance/agent_action.rs::canonical_bytes` plus the
matching emit in `emit_check_event`.

---

## 4. What's chain-logged today

Comprehensive list, all shipped at HEAD `c359e89`:

- **Cross-row chain integrity** (v0.7.0 V-4 closeout, #698) — every
  `signed_events` row carries `prev_hash BLOB` (SHA-256 over the
  canonical-bytes encoding of the preceding row, or 32 zero bytes
  for the first row) and `sequence INTEGER` (monotonic from 1,
  pinned by a UNIQUE index). The SQL chain is the daemon-local
  tamper-evidence property; the JSONL chain in `src/audit.rs`
  remains as the cross-host portable evidence format. Verify via
  `ai-memory verify-signed-events-chain` (chain GREEN exit 0; chain
  break exit 1). Schema bump: SQLite v33 → v34, Postgres v32 → v33.
- **All memory writes** via `signed_events.append`
  (`src/signed_events.rs`) on the success leg of every
  `storage::insert*` and `create_link_signed` path.
- **All reflection writes** with `peer_origin` set when the source
  came from a federation peer (L2-2 commit `2aef248`).
- **All governance refusals on the agent-EXTERNAL surface** via
  `check_agent_action` (the audited path) — every Bash /
  FilesystemWrite / NetworkRequest / ProcessSpawn / Custom check
  emits one row, regardless of decision (`Allow` / `Warn` /
  `Refuse`).
- **Approval-API decisions** (L1-8) — every operator approval /
  rejection of a pending action emits a `signed_events` row.
- **Schema migrations** — every `signed_events` table migration
  itself emits a row at boot identifying the from-version /
  to-version transition.

### 4.1 v0.7.0 V-4 closeout — SQL-side hash chain

The v0.7.0 [Policy Engine](https://github.com/alphaonedev/ai-memory-mcp/issues/693)
validation pass flagged V-4 (substrate-authority cross-row chain
property) as YELLOW: the directive's `monotonic_sequence == prior +
1` assertion required a `sequence` column on `signed_events` that
didn't exist pre-v34. V-4 closeout (#698) adds the column pair
inline:

| Property | Surface | Verification |
|---|---|---|
| Row-level append-only | Rust API surface, no public mutators | `signed_events::tests::append_only_invariant_no_mutators_in_src` |
| Per-row Ed25519 signature | `signed_events.signature` (filled when the writer holds a keypair) | `verify-reflection-chain` (for reflection rows); planned signature-leg in `verify-signed-events-chain --verify-signatures` |
| **Cross-row hash chain** (this closeout) | `signed_events.prev_hash` + `signed_events.sequence` (v34/v33) | `verify-signed-events-chain` walks every row, reports chain GREEN or first break |
| JSONL portable chain | `<audit_dir>/audit.log` line-by-line | `ai-memory audit verify` |

Tamper modes detected by `verify-signed-events-chain`:

- **Row DELETE**: row N+1's stored `prev_hash` no longer matches
  the recomputed canonical-bytes digest of the (now-missing) row N.
- **Row UPDATE** (any column in `canonical_chain_bytes` encoding):
  row N+1's `prev_hash` mismatch propagates the change downstream.
- **Sequence gap / duplicate / non-monotonic jump**: contiguity
  check fails.

The cross-row chain is the LOAD-BEARING tamper-evidence property;
per-row Ed25519 signatures remain as defense-in-depth.

---

## 5. What's NOT chain-logged today

Cold-honest gaps, every one tracked at **#697**:

- **Read actions** (recall / search / list / get / session_boot). The
  engine has no `AgentAction::Read` variant. V08-PE-2 adds it.
- **Subprocess actions from a Bash spawn chain.** A `Bash` rule
  fires against the literal argv the harness proposes. A
  fork+exec inside a permitted shell is born inside the kernel
  without another harness round-trip and is invisible to the
  engine. V08-PE-3 closes this with eBPF on Linux, dtrace on
  macOS.
- **Out-of-band agent actions.** Unenforceable by definition. The
  substrate cannot gate an action that never crosses the harness or
  daemon boundary. Partial mitigations: V08-PE-1 mandatory-hook
  profile (procurement-tier daemon refuses to serve when the
  PreToolUse hook is uninstalled); V08-PE-6 TPM-bound binary
  integrity (daemon attests the shipping binary against a signed
  manifest at boot).
- **Storage-hook refusals — PE-3 merged at HEAD `12a7f29` (#696,
  commit `07b4957`).** A refusal at the `storage::insert` pre-write
  hook short-circuits the SQL with no row written and emits
  `MemoryError::RefusedByGovernance` to the caller; PE-3 makes this
  refusal also typed AND chain-logged via the deferred queue
  (`src/governance/deferred_audit.rs::install_deferred_audit_drainer`)
  — the in-flight write transaction releases its lock before the
  audit row writes so deadlock is structurally impossible. The
  handler-layer `AuditAction::Store` row on the failure leg is also
  emitted to the JSON audit log per [`audit-trail.md`](./audit-trail.md).
  Hard-crash loss is the only remaining gap (process-local queue),
  closed by V08-PE-4 in v0.8.0.
- **Hard-crash-lost deferred events.** PE-3's queue is
  process-local. A SIGKILL / OOM / power loss between the verdict
  and the drain task's `append_signed_event` call loses pending
  rows. V08-PE-4 closes the gap with a persistent on-disk queue
  durable across daemon restart.

---

## 6. Verification

An auditor verifies the chain end to end with three independent
checks. v0.8.0 V08-PE-8 (**#697**) ships `ai-memory
verify-audit-trail` to do all three mechanically:

1. **Monotonic sequence check.** Every `signed_events` row carries a
   per-process monotonic sequence number. The verifier asserts
   strictly-increasing order. A gap surfaces as a precise row id and
   "expected N, got N+k" diagnostic.
2. **Ed25519 signature check per row.** When the row's `attest_level`
   is `signed`, the verifier recomputes the canonical bytes and
   verifies the signature against the operator-issued (or
   per-agent-issued) verifying key. A failure surfaces the row id
   and the verifying key id.
3. **Cross-reference against the expected event surface.** The
   verifier walks the substrate state (memories, links, approvals,
   migrations) and asserts that every state-changing event has a
   matching `signed_events` row. A missing row is a coverage
   regression. The current V08-PE-8 design produces a JSON
   "completeness report" with the missing event class enumerated.

The v0.8.0 verifier closes the loop. Today, the equivalent check is
manual: combine `ai-memory verify-reflection-chain` (steps 1 + 2) with
a hand-cranked cross-reference against the expected event surface for
step 3.

---

## 7. Severity classification

Current verdict shapes — `src/governance/agent_action.rs::Decision`:

- **Allow.** Action proceeds. The audited path still emits a
  `governance.check` row for the audit chain.
- **Warn { rule_id, reason }.** Action proceeds with a logged
  warning. `signed_events` row records the warning rule_id.
- **Refuse { rule_id, reason }.** Action blocked. `signed_events`
  row records the refusal rule_id.

v0.8.0 V08-PE-5 (**#697**) adds **Escalate { rule_id, prompt }** —
ambiguous decisions open an operator approval slot. The Escalate
verdict pairs with the L1-8 Approval-API surface (already shipped):
when an Escalate fires, the substrate emits a `pending_action` row,
the operator dashboard surfaces it, and the operator's
allow/deny decision joins the audit chain. The current verdict
vocabulary has no provision for this — V08-PE-5 closes the gap.

---

## 8. Operator response surface

What happens when audit-trail integrity is compromised — i.e. when
`ai-memory verify-reflection-chain` exits non-zero or when an
external SIEM detects a row mismatch:

1. **The verifier surfaces the precise failure.** Row id, failure
   mode (sequence gap / hash mismatch / signature failure /
   missing-row coverage gap), recovered row count up to the failure.
2. **A `tracing::error!` log line fires** at the same shape used by
   the L1-6 tampered-signature path. The line is structured for
   SIEM ingest.
3. **The substrate refuses to emit further `signed_events` rows
   until the operator clears the alert.** This is the
   chain-corruption response: rather than continue writing rows
   downstream of a known-bad point in the chain (which would taint
   the recovery path), the daemon logs a hard error and the
   write path consults the `signed_events` health gate. Storage
   writes still proceed for normal (non-audit-chain-bound)
   operations, but every governance check fails closed
   (Refuse with `audit_chain_corrupted` reason) until the
   operator runs the recovery verb.

The recovery verb is operator-side. The audit-chain integrity
property is single-direction: once compromised, only an
operator-with-physical-access can restore the chain to a known-good
state (typically by truncating to the last verified row and re-issuing
the operator key).

---

## 9. Federation auth matrix (v0.7.0 fold-A2A1.4, #702)

Federation endpoints (`/api/v1/sync/push` and `/api/v1/sync/since`)
authenticate either via mTLS cert-fingerprint pinning, shared
`x-api-key` secret, or both. The matrix below is the load-bearing
contract for any procurement-grade deployment that pins cross-host
quorum behaviour.

| Deployment mode  | Inbound `/api/v1/sync/*` requirement                              | Outbound POST authentication              | Backwards-compat note            |
|------------------|-------------------------------------------------------------------|-------------------------------------------|----------------------------------|
| mTLS-only        | rustls verifies client cert against `--mtls-allowlist`            | mTLS identity (`--quorum-client-cert/key`)| Pre-v0.7.0 mTLS behaviour preserved verbatim. |
| api-key-only     | `x-api-key` header (or `?api_key=` query param) MUST match `[api] api_key` | `x-api-key` header (forwarded automatically from `[api] api_key`) | The fold-A2A1.4 fix — without outbound forwarding, peer 401s and quorum_not_met fires. |
| mTLS + api-key   | mTLS verifies, api-key check is **skipped on `/api/v1/sync/*`** (mTLS already proves the peer); non-federation paths still require `x-api-key` | mTLS identity AND `x-api-key` (defense-in-depth) | Both auth layers configured. The bypass is scoped to `/api/v1/sync/*` so non-peer surfaces still demand the shared secret. |
| no-auth (legacy) | Anyone with network reach (operator MUST bind to loopback only)   | (no application-layer auth)               | Pre-v0.6.x default. Refused at boot on non-loopback bind since #248. |

**Why the mTLS bypass on `/api/v1/sync/*`**: the rustls `ClientCertVerifier`
(`src/tls.rs::FingerprintAllowlistVerifier`) has already verified the
peer's certificate against the operator-pinned allowlist before any
request body reaches handler code. Demanding the shared `x-api-key`
secret on top of that forces every peer to ALSO carry the secret —
which is exactly the cross-host gap that broke Phase B's test cell:
the leader's outbound POST forgot the header, the peer 401'd, and
quorum never converged. Skipping the api-key check on federation
endpoints when `mtls_enforced` is set restores the orthogonal auth
model — the mTLS layer authenticates peer-to-peer, the api-key layer
authenticates everything else.

**Why outbound `x-api-key` forwarding is mandatory**: a peer that
itself runs with `[api] api_key` configured rejects any POST that
doesn't carry the matching header. Pre-v0.7.0 the leader's federation
client built the request without the header even when the operator had
set `[api] api_key`, so cross-host quorum was unreachable in any
api-key deployment. The fix threads the leader's configured key into
`FederationConfig::build` and `post_once` attaches the header on every
outbound POST plus every catchup batch.

**Test coverage**: `tests/federation_x_api_key.rs` pins each matrix
row. `federation_outbound_forwards_x_api_key_when_configured` covers
the api-key-only row; `federation_outbound_omits_x_api_key_when_unconfigured`
covers the mTLS-only and no-auth rows (backwards-compat);
`mtls_authenticated_request_bypasses_api_key_check` covers the
mTLS+api-key row; `cross_host_quorum_w2_n3_with_api_key_converges`
pins the procurement-grade scenario end-to-end.

**Implementation surface**:

- `FederationConfig::api_key: Option<String>` (`src/federation/mod.rs`).
- `post_once` / `post_and_classify` / `bulk_catchup_push` attach
  `x-api-key` when `Some` (`src/federation/sync.rs`).
- `ApiKeyState::mtls_enforced: bool` (`src/handlers/transport.rs`) —
  true when the daemon was started with `--tls-cert` + `--tls-key` +
  `--mtls-allowlist`. `api_key_auth` skips the key check on
  `/api/v1/sync/*` when this is true.
- Threaded at boot in `src/daemon_runtime.rs::bootstrap_serve`.

### 9.1 Per-author attestation on `/sync/push` (v0.7.0 #238)

Pre-v0.7.0, every `POST /api/v1/sync/push` accepted whatever
`sender_agent_id` the body claimed and charged the matching
`agent_quotas` row, logged the matching `audit::AuditAction::Store`,
and recorded the matching `sync_state` clock entry. Any peer with a
valid mTLS cert could therefore mint audit-trail rows under any
agent's name — defeating per-agent integrity in the cryptographic
chain. Red-team #230 caught it; #238 closes it.

The fix:

| Inbound `x-peer-id` header | `body.sender_agent_id` | Operator config           | Result                                                                  |
|----------------------------|------------------------|---------------------------|-------------------------------------------------------------------------|
| `Some(p)`                  | `s == p`               | n/a                       | Accept (peer authoring as itself).                                      |
| `Some(p)`                  | `s` empty / absent     | n/a                       | Accept (legacy unauthored push).                                        |
| `Some(p)`                  | `s != p`               | `s ∈ allowed_sender_agent_ids[p]` | Accept (operator-pre-approved cross-author push).               |
| `Some(p)`                  | `s != p`               | not in allowlist          | **403 `{"error":"sender_agent_id_mismatch","claimed":"s","peer_header":"p"}`** |
| absent                     | `s` non-empty          | bypass unset              | **403 `{"error":"peer_id_header_missing"}`**                            |
| any                        | any                    | `AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1` | Accept (legacy compat; logged at WARN).                       |

**Operator-bound trust caveat.** Today's mTLS substrate
(`src/tls.rs::FingerprintAllowlistVerifier`) pins the peer's client
certificate by SHA-256 fingerprint but **does not propagate the
verified cert (or its SAN/CN) to handler code** — axum-server 0.8 has
no per-request extension for that. The `x-peer-id` header is therefore
a peer-claimed identity tied to a fingerprint **only by operator
deployment convention** (one cert ↔ one `x-peer-id`), not by a
cryptographic attestation surface. The cert-SAN extraction work is
tracked as a v0.8.0 follow-up to #238.

**Implementation surface**:

- `src/federation/peer_attestation.rs` — `PeerAttestationConfig`,
  `attest_sender`, `AttestError`, env-var contract.
- `src/handlers/federation_receive.rs::sync_push` — runs
  `attest_sender` before the postgres-dispatch branch so both
  backends share the refusal posture.
- `src/federation/sync.rs::post_once` + `bulk_catchup_push` —
  attaches `x-peer-id` on every outbound POST.
- `src/daemon_runtime.rs` (sync-daemon pull + push) — same.

**Env-var contract**:

- `AI_MEMORY_FED_PEER_ATTESTATION` — JSON map of
  `{peer_id: {allowed_sender_agent_ids: [...], allowed_namespaces: [...]}}`.
  Absent = empty allowlist; default-deny on cross-author claims.
- `AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1` — opt-out for legacy peers.

**Test coverage**: `tests/g_issue_238_sender_attestation.rs` (5
cases): header-matches-body, mismatch-no-allowlist, header-absent,
env-bypass, allowlist-permits.

### 9.2 Per-peer namespace scope on `/sync/since` (v0.7.0 #239)

The per-peer namespace allowlist for `/sync/since` lands in the
companion commit on this branch (issue #239). The `PeerScope`
schema documented in §9.1 already reserves the
`allowed_namespaces` field so operator-facing JSON does not churn
between the two security commits.

---

## 10. Forward roadmap

Eight sub-tasks under **#697** drive 100% coverage. Each closes one
row in §2:

- **V08-PE-1** Mandatory-hook profile — closes "out-of-band agent
  actions" partially.
- **V08-PE-2** Read-action gating — closes the "Read actions" row.
- **V08-PE-3** Subprocess-chain visibility — closes the "subprocess
  actions" row.
- **V08-PE-4** Persistent audit queue — closes the "hard-crash
  drainer loss" row.
- **V08-PE-5** Severity-based human escalation — adds the Escalate
  verdict, closes the "no human escalation" gap.
- **V08-PE-6** TPM-bound binary integrity — closes the "out-of-band"
  row's last partial mitigation.
- **V08-PE-7** Refuse-by-default profile — flips the seed rules from
  `enabled = 0` to `enabled = 1, attest_level = operator_signed` for
  procurement-tier deployments.
- **V08-PE-8** `ai-memory verify-audit-trail` — closes the
  end-to-end verification loop (steps 1 + 2 + 3 in §6).

Effort: 22–28 sessions · 3–4 weeks wall-clock · MEDIUM-HIGH risk.
Tracking: **#697**.

---

*Document classification: Public-facing OSS audit-trail coverage
matrix. v0.7.0 Option B single source of truth for the cryptographic
forensic chain. Updated at every §16-style integration gate.*
