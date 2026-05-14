# Policy Engine Architecture

**Status as of branch `docs/policy-engine-architecture` (HEAD `c359e89`,
2026-05-14).** Documents what ships in v0.7.0 Option B, what is in flight,
and what is explicitly v0.8.0 future scope.

**Audit honesty.** Every capability described in the present tense maps to
shipped code at the HEAD above. Every "in flight" item names the issue
that tracks merge. Every "future scope" item names the v0.8.0 epic
sub-task and never reads as a present-tense claim.

Cross-references throughout:

- **#691** — substrate rules engine v2 (base layer; L1-6 A–E shipped).
- **#693** — v0.7.0 Policy Engine Completion (Option B parent meta).
- **#694** — PE-1 universal `AgentAction` wire-point coverage (in flight).
- **#695** — PE-2 Claude Code PreToolUse harness hook installer (in flight).
- **#696** — PE-3 deferred audit-log queue (in flight).
- **#697** — v0.8.0 100% Cryptographic Forensic Audit Trail closeout (epic).

---

## 1. Goal

Operator directive, 2026-05-14, verbatim:

> "Every tool call passes through a policy engine; the engine logs every
> refusal cryptographically; severity-classified rules can escalate to
> human."

The policy engine is the substrate component that implements that
property. v0.7.0 Option B closes ~95% of it. The v0.8.0 epic
(**#697**, eight sub-tasks V08-PE-1 … V08-PE-8) closes the remaining
~5% — read-action gating, subprocess-chain visibility, hard-crash
durability of the audit queue, severity-based escalation, and a
mechanical audit-trail completeness verifier.

The property has two enforcement modes — both shipped:

1. **Substrate-INTERNAL ops** (`memory_store`, `memory_link`,
   `memory_delete`, `memory_archive`, `memory_consolidate`,
   `memory_replay`) are **substrate-authoritative**: every write path
   mechanically consults the engine before the SQL `INSERT`. The agent
   cannot bypass.
2. **Agent-EXTERNAL ops** (`Bash`, `FilesystemWrite` outside the
   substrate, `NetworkRequest`, `ProcessSpawn`, `Custom`) are
   **substrate-rule-bound, harness-mediated**: the rule lives in the
   substrate's `governance_rules` table; the harness (Claude Code
   PreToolUse hook of type `mcp_tool`) consults the substrate via
   `memory_check_agent_action` and honors the decision. Mechanical at
   the harness-hook boundary (operator-configured), not at the agent
   attention boundary (probabilistic).

---

## 2. Components

### 2.1 `governance_rules` table

SQLite migration `migrations/sqlite/0024_v07_governance_rules.sql`
(schema row column `kind`, `matcher`, `severity`, `reason`, `namespace`,
`created_by`, `created_at`, `enabled`, `signature`, `attest_level`).
Seed rules `R001` … `R004` land at `enabled = 0` per the cold-start
contract — an upgrading operator never gets surprise refusals:

| ID | kind | matcher | severity | reason |
|---|---|---|---|---|
| `R001` | `filesystem_write` | `{"glob":"/tmp/**"}` | `refuse` | no `/tmp` writes |
| `R002` | `filesystem_write` | `{"glob":"/var/tmp/**"}` | `refuse` | no `/var/tmp` writes |
| `R003` | `filesystem_write` | `{"glob":"/private/tmp/**"}` | `refuse` | no `/private/tmp` writes (macOS realpath) |
| `R004` | `process_spawn` | `{"binary":"cargo","disk_free_min_gib":20}` | `refuse` | `cargo` refused below 20 GiB free |

Schema indexed by `(kind, enabled)` (`idx_governance_rules_kind_enabled`)
so the dominant query — "list enabled rules of this kind" — is one
index seek per `check_agent_action` invocation.

### 2.2 `RulesStore` — typed CRUD

`src/governance/rules_store.rs`. The **only** module allowed to read or
write `governance_rules` directly. Verbs:

- `insert`, `get`, `list`, `list_enabled_by_kind` (the load-time
  enforcement query — applies the L1-6 signature filter).
- `remove`, `set_enabled` (mutators; CLI verifies operator signature
  before calling).
- `update_signature` (persists Ed25519 signature + bumps `attest_level`
  to `operator_signed`).
- `canonical_bytes_for_signing` — stable serialization commitment over
  `{id, kind, matcher, severity, reason, namespace, created_by, enabled}`.
  Includes `enabled`, intentionally omits `created_at`. See §4.2.

### 2.3 `AgentAction` enum + variants

`src/governance/agent_action.rs::AgentAction`. The agent-external action
vocabulary the harness constructs from a proposed tool call:

| Variant | Wire `kind` | Carrier fields |
|---|---|---|
| `Bash` | `bash` | `command: String`, optional `cwd: PathBuf` |
| `FilesystemWrite` | `filesystem_write` | `path: PathBuf`, optional `byte_estimate: u64` |
| `NetworkRequest` | `network_request` | `host: String`, `scheme: String` |
| `ProcessSpawn` | `process_spawn` | `binary: String`, `args: Vec<String>` |
| `Custom` | `custom` | `custom_kind: String`, `payload: serde_json::Value` |

Variant names are the canonical `kind` strings in the
`governance_rules.kind` column (lower_snake). Adding a new variant is
wire-compatible — existing rules with unknown kinds are ignored, not
failed. v0.8.0 V08-PE-2 (**#697**) adds an `AgentAction::Read`
variant; the wire format is forward-compatible by construction.

### 2.4 `check_agent_action` — audited path

`src/governance/agent_action.rs::check_agent_action(conn, agent_id,
action) -> Result<Decision>`. The **public entry point** for the
agent-external path. Behavior:

1. `list_enabled_by_kind(conn, action.kind())` — filtered through the
   L1-6 signature gate (§4.2).
2. For each candidate rule, evaluate `matcher_applies(rule, action)`.
3. **First-refusal wins.** A `refuse`-severity match returns
   `Decision::Refuse { rule_id, reason }` and stops scanning.
4. If no `refuse` fires, the first `warn`-severity match returns
   `Decision::Warn { rule_id, reason }`; otherwise `Decision::Allow`.
5. **Every exit emits one row** to the `signed_events` chain with
   `event_type = "governance.check"` and `payload_hash` over the
   canonical `{action, decision}` JSON. This is the load-bearing
   audit chain for the v1.0 procurement review.

### 2.5 `check_agent_action_no_audit` — substrate pre-write hook path

`src/governance/agent_action.rs::check_agent_action_no_audit(conn,
action) -> Result<Decision>`. L1-6 Deliverable E. Identical
combinator, **no** `signed_events` emit. Two reasons:

1. **Re-entrancy.** The hook fires INSIDE `storage::insert` —
   i.e. while the caller already holds the substrate writer's
   connection. A second `append_signed_event` on a sibling connection
   would race the WAL writer lock; on the same connection it would
   corrupt the in-flight `INSERT`'s statement state.
2. **Symmetry.** The substrate-internal write path is already audited
   at every callsite (`handlers/http.rs` and `mcp/tools/store.rs`
   both emit an `AuditAction::Store` row on success / a typed
   `MemoryError` on failure). A second emit here would amplify.

PE-3 (**#696**) adds a deferred audit-log queue so refusals on this
path are still chain-logged — see §2.6.

### 2.6 `check_agent_action_deferred` — PE-3 deferred audit queue (in flight, **#696**)

**Status: not merged at HEAD `c359e89`.** Tracked at issue **#696**;
target branch `policy-engine/deferred-audit-log`. The function is the
audit-emitting variant of `check_agent_action_no_audit`: the rule
combinator runs synchronously inside the storage write path, but the
`signed_events` row is enqueued onto a process-local channel and
drained by a dedicated tokio task that opens its own connection — no
re-entrancy on the substrate writer.

The deferred queue is **process-local**. A hard crash (SIGKILL, OOM,
power loss) before drain loses pending refusal rows. V08-PE-4
(**#697**) closes that gap with a persistent on-disk queue (durable
across daemon restart).

Five tests pin the property — see §4.

### 2.7 Operator keypair (`~/.config/ai-memory/operator.key`, mode 0600)

`src/cli/rules.rs` — verbs `keygen`, `sign-seed`, `add --sign`,
`enable --sign`, `disable --sign`.

- **Private key**: `~/.config/ai-memory/operator.key`, mode `0600`,
  Ed25519 32-byte seed (base64 URL-safe, no pad). Permissions checked
  at every `load_operator_signing_key` call; a world-readable file
  produces a hard refusal with a precise error message — see the
  `keygen_writes_0600_and_load_refuses_open_permissions` test.
- **Public key**: `~/.config/ai-memory/operator.key.pub` (default,
  resolved via `dirs::config_dir()`) **or** the
  `AI_MEMORY_OPERATOR_PUBKEY` env var (base64 of the 32-byte
  verifying key). Either resolves a `VerifyingKey`; the env var wins
  when both are present.

### 2.8 Load-time signature verification (L1-6)

`src/governance/rules_store.rs::enforced_rule_passes`. The
**activation cliff** is operator pubkey presence:

- **Pubkey NOT resolved** (cold start, fresh install, test
  environment): every `enabled = 1` row passes through unchanged. The
  pre-L1-6 contract is preserved.
- **Pubkey resolved + row `attest_level = 'operator_signed'` +
  signature verifies**: rule is enforced.
- **Pubkey resolved + row `attest_level = 'operator_signed'` but
  signature does NOT verify** (tampered row, post-sign direct SQL
  mutation, wrong key): `tracing::error!` and SKIP. **The daemon does
  NOT crash.** A tampered rule must never bring down the substrate.
- **Pubkey resolved + row `attest_level = 'unsigned'`**:
  `tracing::warn!` and SKIP — enforced rules MUST be operator-signed
  once L1-6 is active.

The signature commits to `canonical_bytes_for_signing` over the rule's
content fields **plus `enabled`**. That last detail is the central
bypass-prevention property — see §4.2.

---

## 3. Wire-points

### 3.1 Shipped at HEAD `c359e89`

| Surface | Call site | AgentAction constructed | Refusal mapping |
|---|---|---|---|
| **Substrate write path** (HTTP `POST /api/v1/memories`, MCP `memory_store`, federation inbound, CLI `ai-memory mine`) | `src/storage/mod.rs::consult_governance_pre_write` (OnceLock-installed closure in `src/daemon_runtime.rs`, daemon `serve` only) | `AgentAction::Custom { custom_kind: "memory_write", payload: {namespace, tier, memory_kind, title} }` evaluated by `check_agent_action_no_audit` | HTTP `403 FORBIDDEN` + `GOVERNANCE_REFUSED` code; MCP `GOVERNANCE_REFUSED` error data; typed `MemoryError::RefusedByGovernance` |
| **MCP** `memory_check_agent_action` (Power family) | `src/mcp/tools/check_agent_action.rs::handle_check_agent_action` → `check_agent_action` (audited) | Caller-supplied — `{kind, ...}` per §2.3 | MCP tool returns `{"decision":"refuse", "rule_id": "...", "reason": "..."}`; harness PreToolUse hook of type `mcp_tool` reads this and blocks |
| **CLI** `ai-memory rules check` | `src/cli/rules.rs::cmd_rules` (`Check` variant) → `check_agent_action` (audited) | Caller-supplied via `--kind` + `--command`/`--path`/`--host`/`--binary` | Non-zero exit code on refuse; JSON output via `--json` |

All three production wire-points share the **same combinator** and
the **same `governance_rules` table**. The audit emit differs only
between the substrate-internal pre-write path (deferred via PE-3 once
merged) and the agent-external paths (audited synchronously today).

### 3.2 In-flight at v0.7.0 Option B

| Wire-point | Issue | Status |
|---|---|---|
| PE-1: universal `AgentAction` wire-point coverage — extends the construction surface so every harness-visible tool maps to an `AgentAction` variant | **#694** | Branch `policy-engine/wire-points`, not merged at `c359e89` |
| PE-2: Claude Code PreToolUse harness hook installer — `ai-memory install --harness claude-code --enforce-policy` configures the hook so the harness consults `memory_check_agent_action` before every Bash / Write / NetworkRequest / Process spawn | **#695** | Branch `policy-engine/harness-hook`, not merged at `c359e89` |
| PE-3: deferred audit-log queue — adds `check_agent_action_deferred` for the substrate pre-write hook so storage refusals are chain-logged | **#696** | Branch `policy-engine/deferred-audit-log`, not merged at `c359e89` |

This document is the v0.7.0 single source of truth for these
boundaries. When the sibling branches merge, the **Status** column in
the table above flips and the wire-point row migrates from §3.2 to
§3.1. Do not pre-claim merged behavior — Codex will read this.

---

## 4. Bypass-impossibility properties

All properties below are pinned by integration tests at HEAD `c359e89`
and by the PE-3 deferred-audit-log test suite once it merges.

### 4.1 Six L1-6 integration tests

File: `tests/governance_l16_activation.rs`. Tests:

1. **`enforced_rule_must_be_operator_signed`** — pubkey present +
   row `enabled = 1` but `attest_level = 'unsigned'` ⇒ rule is
   filtered at load (`tracing::warn!`); a `/tmp/foo` write is
   `Decision::Allow` because no enforced rule fires. Operator
   misconfiguration is observable, never silently permissive.
2. **`tampered_signature_rejects_at_load`** — pubkey present, row
   `attest_level = 'operator_signed'` but the stored signature is
   corrupted ⇒ filtered at load (`tracing::error!`); the rule does
   not fire. **Plain language: an attacker who flips one bit in a
   signed row gets the same outcome as an attacker who never signed
   it — silent skip with an audit-visible log line.**
3. **`direct_enabled_flip_bypass_attempt_fails`** — a signed
   `enabled = 0` row is mutated via direct SQL (`UPDATE
   governance_rules SET enabled = 1 WHERE id = ?`). Because
   `canonical_bytes_for_signing` commits to `enabled`, the recorded
   signature no longer verifies the new canonical bytes. The rule is
   skipped at load. **Plain language: you cannot turn a signed rule
   on or off via direct database edits — the operator must re-sign.**
4. **`keygen_writes_0600_and_load_refuses_open_permissions`** — the
   private key file is required to be mode `0600`; a world-readable
   key produces a refusal at load time, not at first-use. **Plain
   language: a leaked private key cannot be quietly used.**
5. **`sign_seed_idempotent`** — `ai-memory rules sign-seed` produces
   stable canonical bytes across repeat invocations even when the
   migration re-runs with `created_at` resets. **Plain language: the
   operator can re-sign the seed without invalidating prior
   signatures.**
6. **`rotated_operator_key_invalidates_prior_signatures`** — rotating
   the operator key invalidates every prior signature. The
   tampered-row branch fires for every previously-signed row until
   the operator re-signs under the new key. **Plain language: key
   rotation is a hard reset of the enforced rule set.**

### 4.2 Storage::insert hook unbypassability

File: `tests/governance_storage_insert_hook.rs`. Six tests:

1. **`hook_set_to_allow_lets_write_through`** — the allow leg is
   zero-cost: no `signed_events` write, no extra connection, no
   blocking SQL.
2. **`hook_set_to_refuse_returns_typed_error`** — refusal at the
   pre-write hook surfaces as `MemoryError::RefusedByGovernance`,
   not a generic `anyhow` chain. HTTP maps to `403 GOVERNANCE_REFUSED`.
3. **`hook_refusal_propagates_via_anyhow_downcast`** — the `anyhow`
   chain carries a typed marker (`storage::GovernanceRefusal`) that
   downcasts cleanly in `MemoryError::from(anyhow::Error)`.
4. **`hook_gates_all_three_insert_paths`** — `insert`,
   `insert_with_conflict`, and `insert_if_newer` all consult the hook.
   **Plain language: there is no "side door" insert path that
   skips the policy engine.**
5. **`cli_one_shot_does_not_install_hook`** — `ai-memory store` /
   `mine` / `import` (one-shot CLI ops) leave the OnceLock empty by
   design — the operator's direct substrate ops stay unimpeded.
   Only `ai-memory serve` installs the hook.
6. **`refusal_maps_to_http_403`** — the daemon end-to-end: an HTTP
   `POST /api/v1/memories` against an enforced refuse-rule returns
   `403` with body `{"code":"GOVERNANCE_REFUSED","reason": "..."}`.

The hook itself is a process-wide `OnceLock<Box<dyn Fn>>` in
`src/storage/mod.rs::GOVERNANCE_PRE_WRITE`. **Plain language:
installation is one-shot at the type level — no reset, no override,
no test-only escape hatch reachable from production. The hook
closure is consulted by every substrate write path; it cannot be
worked around without modifying the substrate source.**

### 4.3 PE-3 deferred-audit-log tests (in flight, **#696**)

Five tests on branch `policy-engine/deferred-audit-log`, target merge
under issue **#696**:

1. Deferred refusal eventually emits `governance.check` row with
   identical canonical-bytes / `payload_hash` as the audited path.
2. Drain is bounded — under flood the queue cap is honored and the
   `governance.check_drop` counter increments rather than the daemon
   blocking the write path.
3. Drain-on-shutdown — the daemon's graceful shutdown drains the
   queue before exit; clean stop is loss-free.
4. Crash before drain loses pending rows. Documented gap. Closed by
   V08-PE-4 (**#697**) with a persistent on-disk queue.
5. Cross-reference: an end-to-end test feeds a tampered signature
   through the hook → refusal verdict matches → audit row matches
   → chain verify passes. **Plain language: a refusal at the
   substrate-internal pre-write hook ends up cryptographically
   chain-logged via the same `signed_events` table the agent-external
   path uses today.**

When PE-3 merges, the §3.2 row migrates to §3.1 and the audit-trail
coverage matrix in `docs/security/audit-trail-coverage.md` flips the
"deferred chain-log" row from in-flight to shipped.

---

## 5. What v0.7.0 substrate authority IS

Substrate-visible action coverage at HEAD `c359e89`:

- **Memory writes** — every `storage::insert*` path (HTTP, MCP,
  federation, queue replay) consults `GOVERNANCE_PRE_WRITE` and
  refuses cleanly on a refuse-severity match.
- **Network requests via the daemon** — the daemon's outbound
  federation paths and webhook delivery are governance-bounded by the
  same engine when an `AgentAction::NetworkRequest` rule is in scope;
  full PE-1 coverage of `NetworkRequest` wire-points lands with
  **#694**.
- **Process spawns initiated by the substrate** — the seed rule
  `R004` demonstrates the contract (cargo refused on low-disk
  systems). PE-1 widens substrate-initiated spawns to consult the
  engine universally; **#694**.
- **Optional harness hook for PreToolUse** — when the operator
  installs the Claude Code PreToolUse hook via PE-2 (**#695**), the
  harness consults `memory_check_agent_action` before every Bash /
  Write / NetworkRequest / ProcessSpawn the agent proposes. The
  harness boundary is operator-configured; the substrate rules
  themselves are operator-signed.

---

## 6. What v0.7.0 substrate authority ISN'T

**Cold honesty.** These boundaries are out of scope at HEAD
`c359e89` and closed by v0.8.0 (**#697**, V08-PE-1 … V08-PE-8):

- **Out-of-band channels.** The substrate cannot gate actions that
  bypass the harness — an agent that reaches for `tokio::process` in
  its own out-of-substrate code is not bound by the engine. Bounded
  by the harness boundary, not the substrate. Unenforceable by
  definition. The mitigation is **mandatory-hook profile** (V08-PE-1,
  procurement-tier `--enforce`) and **TPM-bound binary integrity**
  (V08-PE-6) — the operator runs an attested binary that refuses to
  serve when the hook is uninstalled.
- **Read-action gating.** No `AgentAction::Read` variant ships at
  HEAD. Recall / search / list / get are gated through the K9
  `Permissions::evaluate` pipeline (memory-scoped) but the engine
  has no top-level "an agent is reading X" surface. V08-PE-2
  (**#697**) adds the variant.
- **Subprocess chains.** A `Bash` rule fires against the literal
  argv the harness proposes. A `bash -c "evil_thing"` invocation
  whose `evil_thing` then `fork()`s an unrelated child is invisible
  to the engine — the child is born inside the kernel without
  another harness round-trip. V08-PE-3 (**#697**) adds eBPF on Linux
  + dtrace on macOS to surface the chain.
- **Binary integrity attestation.** The engine does not verify the
  shipping binary against an attested manifest. An attacker with FS
  write access could replace `ai-memory` with a forked build that
  no-ops the hook. V08-PE-6 (**#697**) closes this with TPM-bound
  attestation.
- **Severity-based human escalation.** The engine has three verdict
  shapes — `Allow`, `Refuse`, `Warn`. There is no `Escalate` shape
  that opens an operator approval slot for ambiguous decisions.
  V08-PE-5 (**#697**) adds it with an operator dashboard and
  approval queue.
- **Hard-crash drainer loss.** PE-3's deferred queue is process-local.
  A SIGKILL between the refuse verdict and the drainer's `append`
  loses the audit row. V08-PE-4 (**#697**) makes the queue
  persistent across daemon restart.

Tracking: **#697** holds the v0.8.0 epic. The audit-trail coverage
matrix at [`security/audit-trail-coverage.md`](./security/audit-trail-coverage.md)
enumerates the same gaps from the audit-chain perspective.

---

## 7. Operator workflow

End-to-end activation of L1-6 enforcement, no theater:

```bash
# 1. Generate the operator keypair (mode 0600 enforced).
ai-memory rules keygen
# → writes ~/.config/ai-memory/operator.key (private, 0600)
# → writes ~/.config/ai-memory/operator.key.pub (public, 0644)

# 2. Sign the seed rules R001-R004 with the operator key.
ai-memory rules sign-seed
# → updates governance_rules.signature for each seed row
# → bumps attest_level from 'unsigned' to 'operator_signed'
# → idempotent: re-running produces identical signatures (canonical
#   bytes omit created_at on purpose)

# 3. Activate one rule. `--sign` is required — the CLI verifies the
#    operator key is loadable before flipping `enabled`.
ai-memory rules enable R001 --sign

# 4. (Optional) Install the harness hook so Claude Code consults the
#    substrate on every PreToolUse. Requires PE-2 (#695) — in flight.
ai-memory install --harness claude-code --enforce-policy

# 5. Verify with a smoke test. The check verb runs the audited path so
#    every check shows up in signed_events.
ai-memory rules check \
  --agent-id alice \
  --kind filesystem_write \
  --path /tmp/anything
# → exit 1, prints {"decision":"refuse","rule_id":"R001",
#                   "reason":"Operator hard rule (#691): no /tmp writes..."}

# 6. Confirm the audit chain captured the refusal.
ai-memory verify-reflection-chain  # or: ai-memory audit verify
```

The activation cliff is **step 2**. Before that, the substrate is in
pre-L1-6 mode and every enabled row passes through unchanged. After
step 2 + step 3, only operator-signed rows are enforced — every
other row is `warn!`-logged and skipped.

---

## 8. Audit-trail completeness coverage matrix

| Action class | Wire-point | Coverage at `c359e89` | Gap closed by |
|---|---|---|---|
| Substrate-INTERNAL writes (`memory_store`, `memory_link`, `memory_delete`, `memory_archive`, `memory_consolidate`) | `storage::insert*` + handler-emitted `AuditAction::Store` etc. | **100% audited** (handler emits success row; refusal emits a deferred row once PE-3 merges) | PE-3 (**#696**) for the refusal leg |
| Substrate-INTERNAL reads (`memory_recall`, `memory_search`, `memory_list`, `memory_get`, `memory_session_boot`) | `AuditAction::Recall` etc. in handler layer | **100% audited at handler layer** for visibility. Engine-level read gating is out of scope at HEAD — no `AgentAction::Read`. | V08-PE-2 (**#697**) |
| `memory_replay` (transcript cross-tenant read) | K9 `Op::MemoryReplay` gates the read; audit row on every call | **100% audited** | — |
| Agent-EXTERNAL `Bash` | `memory_check_agent_action` MCP tool consulted by harness PreToolUse | Audited every call via `check_agent_action` audited path. Harness coverage is operator-configured (PE-2 / **#695**) | PE-2 (**#695**) merges harness installer |
| Agent-EXTERNAL `FilesystemWrite` outside substrate | as above | as above | as above |
| Agent-EXTERNAL `NetworkRequest` | as above | as above | as above |
| Agent-EXTERNAL `ProcessSpawn` | as above | as above (universal PE-1 coverage tracked at **#694**) | PE-1 (**#694**) |
| Subprocess chains (bash spawn → fork → exec) | not visible at HEAD | **out of scope** | V08-PE-3 (**#697**) |
| Out-of-band agent actions | not visible by definition | **unenforceable** | partial: V08-PE-1 mandatory-hook + V08-PE-6 TPM (**#697**) |
| Hard-crash recovery of deferred queue | n/a until PE-3 merges; then process-local | gap | V08-PE-4 (**#697**) |
| Severity-based escalation (Escalate verdict) | absent at HEAD | gap | V08-PE-5 (**#697**) |

The same matrix from the audit-chain side, with row shapes and
verification examples, lives in
[`security/audit-trail-coverage.md`](./security/audit-trail-coverage.md).

---

## 9. Forward roadmap

The v0.8.0 closeout epic (**#697**) holds eight sub-tasks. Each
closes one observable gap from §6 / §8:

- **V08-PE-1** — Mandatory-hook profile (`--enforce` for
  procurement-tier deployments; daemon refuses to serve when the
  PreToolUse hook is not installed).
- **V08-PE-2** — Read-action gating (`AgentAction::Read` variant +
  wire-point coverage for the read surface).
- **V08-PE-3** — Subprocess-chain visibility (eBPF on Linux, dtrace
  on macOS; surfaces the fork+exec chain to the engine).
- **V08-PE-4** — Persistent audit queue (durable across daemon
  restart; closes the hard-crash gap in PE-3's process-local queue).
- **V08-PE-5** — Severity-based human escalation (`Decision::Escalate`
  with operator dashboard + approval queue).
- **V08-PE-6** — TPM-bound binary integrity (the daemon attests the
  shipping binary against a signed manifest at boot).
- **V08-PE-7** — Refuse-by-default profile (procurement-tier rule
  set that ships `enabled = 1, attest_level = operator_signed`
  out of the box — opt-out rather than opt-in).
- **V08-PE-8** — Audit-trail completeness verifier (`ai-memory
  verify-audit-trail` walks the `signed_events` chain end-to-end and
  cross-references against the expected event surface; closes the
  verification loop).

Effort: 22–28 sessions · 3–4 weeks wall-clock · MEDIUM-HIGH risk.
Tracking: **#697** with sub-tasks V08-PE-1 … V08-PE-8.

---

*Document classification: Public-facing OSS architecture documentation.
v0.7.0 Option B single source of truth for the policy engine. Updated
at every §16-style integration gate.*
