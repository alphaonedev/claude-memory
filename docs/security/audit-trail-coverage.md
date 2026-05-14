# Cryptographic Forensic Audit Trail — Coverage Matrix

**Status as of branch `docs/policy-engine-architecture` (HEAD `c359e89`,
2026-05-14).**

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
| Memory writes (`store` / `update` / `link` / `delete` / `archive` / `consolidate`) | **Chain-logged today** via `signed_events.append` (`src/signed_events.rs`) on every successful substrate write | `event_type = "memory.<verb>"`, `payload_hash` over canonical-JSON of the post-write row, `signature` (Ed25519 over `payload_hash`), `attest_level` ∈ {`unsigned`, `signed`} | none for the success leg | — |
| Reflection writes | **Chain-logged today** with `peer_origin` for cross-peer paths (L2-2 commit `2aef248`) | `event_type = "reflection.write"`, payload binds `(source_ids, depth, peer_origin)` | none | — |
| Governance refusals on agent-EXTERNAL surface (Bash / Write / Network / ProcessSpawn / Custom) via `check_agent_action` (audited path) | **Chain-logged today** synchronously, every call | `event_type = "governance.check"`, `payload_hash` over canonical `{action, decision}` JSON, `agent_id` carrier set | none | — |
| Governance refusals on substrate-INTERNAL pre-write hook (`check_agent_action_no_audit`) | **In flight** via PE-3 deferred queue | identical shape to the audited path — same canonical bytes / payload hash; emit deferred via tokio drain task | hard-crash drainer loss (process-local queue); see V08-PE-4 | **#696** (in flight); **#697** V08-PE-4 closes durability |
| Approval-API decisions (L1-8) | **Chain-logged today** | `event_type = "approval.<decision>"`, binds approver identity + decision + correlation id | none | — |
| Schema migrations | **Chain-logged today** at boot | `event_type = "schema.migration"`, binds from-version + to-version + migration filename hash | none | — |
| Read actions (`memory_recall` / `memory_search` / `memory_list` / `memory_get` / `memory_session_boot`) | **NOT chain-logged** at engine level. Handler-layer `AuditAction::Recall` etc. row is emitted to the JSON audit log per [`audit-trail.md`](./audit-trail.md), but no `signed_events` row | n/a — v0.8.0 adds `event_type = "governance.read_check"` once V08-PE-2 lands | engine has no `AgentAction::Read` variant at HEAD | **#697** V08-PE-2 |
| Subprocess actions from Bash spawn chain (fork→exec under a permitted shell) | **NOT visible** to the engine at HEAD | n/a — v0.8.0 adds eBPF/dtrace surface and `event_type = "process.spawn_chain"` | invisible to the substrate without a kernel-side probe | **#697** V08-PE-3 |
| Out-of-band agent actions | **Unenforceable by definition** | n/a — substrate has no visibility | partial mitigations: V08-PE-1 mandatory-hook + V08-PE-6 TPM-bound binary integrity | **#697** V08-PE-1, V08-PE-6 |
| Hard-crash-lost deferred events | **Gap** — process-local queue | rows drop silently on SIGKILL between verdict and drain | persistent on-disk queue closes the gap | **#697** V08-PE-4 |

---

## 3. Reading the chain

Three operator-facing surfaces.

### 3.1 `ai-memory verify-reflection-chain`

Walks the `signed_events` rows in monotonic-sequence order. For each
row:

1. Verify the sequence number is strictly increasing.
2. Recompute `payload_hash` over the canonical-bytes encoding for the
   row's `event_type`. Compare against the stored value.
3. When `signature` is present and `attest_level = signed`, verify
   the Ed25519 signature against the operator-issued (or
   per-agent-issued) verifying key.

Exits non-zero on the first failure. Prints the precise row id and
failure mode.

### 3.2 `ai-memory export-forensic-bundle` (L2-5, commit `340367f`)

Produces a self-contained tarball: every `signed_events` row + the
in-scope reflection / link / approval rows + the operator pubkey + a
manifest. Designed to be handed to an external auditor without giving
them direct database access.

### 3.3 Raw `signed_events` query example

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
- **Storage-hook refusals before PE-3 merges (#696).** At HEAD
  `c359e89`, a refusal at the `storage::insert` pre-write hook
  short-circuits the SQL with no row written and emits
  `MemoryError::RefusedByGovernance` to the caller — but does **not**
  emit a `signed_events` row, by design (re-entrancy on the
  substrate writer's connection). PE-3 (**#696**) makes this typed
  AND chain-logged via the deferred queue. The handler-layer
  `AuditAction::Store` row on the failure leg is still emitted to
  the JSON audit log per [`audit-trail.md`](./audit-trail.md).
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

## 9. Forward roadmap

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
