# Recursive learning (v0.7.0)

> **Status (2026-05-14):** The recursive-learning grand-slam ships in
> v0.7.0. Tasks 1-8 of the original recursive-learning add-on (issue
> [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655)) plus
> the L1 substrate stack (#666-680) and the L2 wave (#666-#673) all
> land on `feat/v0.7.0-grand-slam` and roll up into the v0.7.0 tag.
> The L2 wave extends the substrate primitive into a curator mode,
> federation-aware coordination, invalidation propagation, transcript
> replay union, forensic bundles, reflection-as-skill promotion, skill
> composition, and a reflection-aware reranker boost — every claim on
> this page maps to shipped code at commit `c359e89`.

ai-memory v0.7.0 ships a **substrate-native primitive for recursive
refinement**: an agent reads one or more memories, synthesises a
higher-order reflection (a lesson, pattern, contradiction-resolution,
etc.), and persists it with cryptographic-grade provenance back to
each source it reflects on. Reflection depth is bounded by a
substrate-enforced cap. No autonomous goal modification, no model
fine-tuning loops, no unbounded recursion — the substrate refuses
runaway recursion before any write opens.

This page is the engineering-precise primer. The procurement-level
introduction lives in [`v0.7.0/release-notes.md`](v0.7.0/release-notes.md).
The CHANGELOG block sits under v0.7.0 in [`../CHANGELOG.md`](../CHANGELOG.md).

## Conceptual model

A **reflection** is a higher-order memory whose `reflection_depth` is
strictly greater than zero and whose `reflects_on` provenance links
point back to one or more lower-depth source memories. The reflection
row is just another memory — the same recall, search, governance,
federation, attestation, and audit primitives apply to it. What's new
is the recursion contract:

- Every memory carries `reflection_depth: i32` (column on
  `memories`). Caller-minted (and pre-v0.7.0) rows are `depth = 0`.
- A reflection minted by `memory_reflect` over a set of sources S
  has `depth = max(depth_of_each_source) + 1`.
- The substrate writes one `reflects_on` link per source, with the
  reflection as `source_id` and the original as `target_id`. The
  arrow points **from** the reflection **to** the original — same
  directionality contract as `derived_from`.
- A `memory_reflect` call is atomic: the reflection insert and N
  `reflects_on` link writes happen inside a single transaction. Any
  failure rolls back the whole write — the reflection row never
  survives a half-written state.
- The reflection's `metadata.reflection_metadata` block records
  `reflected_on_source_ids`, `reflection_depth`, and
  `reflection_created_at` (RFC3339). Caller-supplied metadata keys
  win on collision (documented additive contract).

A reflection is **provenance-pinned, not just provenance-claimed.**
The `reflects_on` edges are the cryptographic-grade link — when the
v0.7.0 H-track Ed25519 keypair is configured, the link is signed and
verifiable via `memory_verify`. A downstream auditor can walk the
reflection chain and re-verify every step.

## The depth cap

Reflection depth is **substrate-enforced**, not application-enforced.
`db::reflect` (and the postgres parity `PostgresStore::reflect`)
consult `GovernancePolicy.max_reflection_depth` for the resolved
namespace before opening the write transaction. If the proposed depth
exceeds the cap, the substrate refuses with a structured error —
no partial write, no autonomous escape hatch.

### Why 3?

The compiled default is **3**. It bounds reflection recursion without
strangling the legitimate reflection-on-reflection chains the v0.8.0
Pillar 2.5 curator mode is being designed against. Operators who want
a different global default change the constant at the
`effective_max_reflection_depth` accessor; per-namespace overrides
ride the same JSON governance blob `resolve_governance_policy` already
walks leaf-first.

### Per-namespace override

`GovernancePolicy.max_reflection_depth: Option<u32>` is a pure JSON
metadata field on the standard-memory governance object. No schema
bump — it rides alongside the existing `write`, `promote`, `delete`,
`approver`, and `inherit` fields. The accessor is flat:

```rust
pub fn effective_max_reflection_depth(&self) -> u32 {
    self.max_reflection_depth.unwrap_or(3)
}
```

Resolution is leaf-first via `resolve_governance_policy` (same path
the rest of the K1/G1 governance enforcement walks), so a child
namespace's `max_reflection_depth = None` falls through to the
nearest ancestor that does set it, and finally to the compiled
default `3`.

### `Some(0)` is the kill-switch

The substrate-side check is `attempted > cap`, not `attempted >= cap`.
That means `cap = 0` refuses every reflection — depth `1` already
exceeds `0`, depth `2` exceeds it, and so on. **`Some(0)` is the
documented kill-switch** for a namespace (or subtree) that should
never accept reflection writes. Set it on a namespace whose contents
must remain caller-minted and you have a per-namespace opt-out from
the entire primitive.

## API surfaces

| Surface | Where | Notes |
|---|---|---|
| `memories.reflection_depth INTEGER NOT NULL DEFAULT 0` | SQLite schema v29 (`src/db.rs`); Postgres schema v31 ([`src/store/postgres_schema.sql`](../src/store/postgres_schema.sql), [`migrations/postgres/0013_v0700_reflection_depth.sql`](../migrations/postgres/0013_v0700_reflection_depth.sql)) | Caller-minted rows are 0; reflections are `max(source_depths) + 1`. UPSERT clauses take `MAX(old, new)` so federation merges preserve the higher-depth signal. |
| `Memory::reflection_depth: i32` | [`src/models.rs`](../src/models.rs) | `#[serde(default)]` keeps wire-compat with pre-v0.7.0 federation peers. `impl Default for Memory` ships in the same commit so future struct-field adds stop fanning out to ~50 test fixtures. |
| `GovernancePolicy::max_reflection_depth: Option<u32>` | [`src/models.rs`](../src/models.rs) | Per-namespace cap. `None` → compiled default 3. `Some(0)` → kill-switch. |
| `GovernancePolicy::effective_max_reflection_depth(&self) -> u32` | [`src/models.rs`](../src/models.rs) | Flat accessor. Does NOT walk ancestors — call `resolve_governance_policy` first, then this accessor on the result. |
| `reflects_on` relation | [`src/validate.rs`](../src/validate.rs) (`VALID_RELATIONS`); MCP enums in [`src/mcp.rs`](../src/mcp.rs) for `memory_link` / `memory_unlink`; `claude_help` prompt pipe-list | No schema migration required. `memory_links.relation` has no `CHECK (relation IN ...)` clause on either adapter — adding a label is a pure validator + documentation change. |
| `memory_reflect` MCP tool | [`src/mcp.rs`](../src/mcp.rs); substrate impl `db::reflect` in [`src/db.rs`](../src/db.rs); postgres parity `PostgresStore::reflect` in [`src/store/postgres.rs`](../src/store/postgres.rs) | `Family::Power`. Tool count 51 → 52. Atomic insert + N `reflects_on` link writes inside a single `BEGIN IMMEDIATE` / `COMMIT` block (SQLite) or `sqlx::Transaction` (Postgres). |
| `MemoryError::ReflectionDepthExceeded { attempted: u32, cap: u32, namespace: String }` | [`src/errors.rs`](../src/errors.rs) | HTTP `409 CONFLICT`, code `REFLECTION_DEPTH_EXCEEDED`. The structured triple is what downstream auditors and hook emitters need without parsing error strings. |

## Directionality contract for `reflects_on`

The reflection memory is the link's `source_id`; the original being
reflected on is the link's `target_id`. This matches the existing
`derived_from` convention:

```
reflection_memory  --reflects_on-->  original_source
(reflection_depth = N)               (reflection_depth = N-1 or less)
   ^^ link.source_id                    ^^ link.target_id
```

The arrow points **from** the newer/derived row **to** the original.
A consolidated memory `derived_from` its sources is the same shape —
the derived row is on the left, the source on the right. Operators
tracing reflection provenance walk edges *outward* from the reflection
to find its sources, exactly as they walk edges *outward* from a
consolidated memory to find the inputs that produced it.

## `find_paths` chain-walk behaviour

`db::find_paths`'s recursive CTE projects every edge in `memory_links`
**without filtering by relation label**. That means `reflects_on`
edges auto-participate in chain walks alongside the other relations —
operators tracing reflection provenance see chains surface naturally
without further work. The Task 3 regression test
(`tests/recursive_learning_task3_reflects_on.rs::
sqlite_find_paths_walks_reflects_on_edges`) pins this behaviour
against a 3-hop chain.

When walking a reflection chain, expect the path to alternate
between memories that were caller-minted (`reflection_depth = 0`) and
their reflections (`reflection_depth > 0`). A reflection of a
reflection of a reflection is a 3-edge `reflects_on` chain whose
terminal nodes carry depths `0`, `1`, `2`, `3` from leaf to root.

## Reproducibility

The end-to-end demo script is
[`scripts/reproduce-recursive-learning.sh`](../scripts/reproduce-recursive-learning.sh).
It builds the release binary, creates a fresh sqlite DB under
`.local-runs/repro-recursive-learning-<timestamp>/`, inserts three
sample memories, calls `memory_reflect` to produce a reflection at
depth=1, recursively reflects up to depth=3 (the default cap), and
demonstrates the refusal at depth=4 with a clearly-formatted
`REFLECTION_DEPTH_EXCEEDED` verdict block. Idempotent on re-run (each
invocation uses a fresh timestamped subdirectory).

The script honors the project no-`/tmp` HARD RULE — all scratch lives
under `.local-runs/`, which is gitignored.

## Audit record on depth-cap refusal

**Landed in v0.7.0 (Task 5/8, [commit `c61a05b`](https://github.com/alphaonedev/ai-memory-mcp/commit/c61a05b)).**

Every `db::reflect` call that would exceed the namespace's resolved
`max_reflection_depth` appends a row to the append-only `signed_events`
audit table *before* the cap refusal propagates back to the caller.
The row carries:

- `event_type = "reflection.depth_exceeded"` — the canonical type tag
  by which downstream auditors filter cap-refusal events out of the
  full `signed_events` stream.
- `attest_level = "unsigned"` — the substrate refusal *is* the
  operation being audited; per-event Ed25519 signing of refusal
  records is a separate Track-H Bucket-1.5 line item.
- A canonical-CBOR payload (RFC 8949 §4.2.1 — deterministic encoding,
  shortest form, sorted map keys) binding the seven enumerable
  provenance fields: `agent_id`, `attempted` (the depth that was
  refused), `cap` (the resolved namespace cap that was breached),
  `namespace`, `source_ids` (the ordered list of memories the
  refusal would have reflected on), `proposed_title` (the
  caller-supplied title of the reflection that was refused), and
  `created_at` (RFC3339).
- `payload_hash` — SHA-256 of those canonical-CBOR bytes. The hash is
  the substrate's tamper-evident commitment to the audit payload;
  downstream auditors re-encode the audit row's payload and compare
  the hash to detect mutation.

**PII guarantee.** The reflection's `content` body is deliberately
omitted from the audit payload. The cap-refusal audit captures only
the enumerable provenance the refusal needed to make its decision —
the proposed title is human-readable but the body is not. A caller
that placed PII in `content` and tripped the cap therefore does *not*
leak that body into the audit chain.

**Best-effort write semantics.** Audit-row insertion is best-effort:
on insertion failure (disk full, lock contention, table corruption),
the substrate logs at `WARN` via
`tracing::warn!(target: "signed_events", ...)` but the cap refusal
still propagates to the caller with the same
`ReflectError::DepthExceeded` shape. The wire contract is unchanged
by audit-write success/failure — operators reading
`/api/v1/audit/signed_events` reconcile gaps against the daemon's
`signed_events` warn-log target rather than the caller observing a
different error.

## Hook integration

**Landed in v0.7.0 (Task 6/8, [commit `fbf093c`](https://github.com/alphaonedev/ai-memory-mcp/commit/fbf093c)).**

The Track-G hook pipeline grows from 21 to 23 events with two new
`HookEvent` variants for the reflection primitive:

- **`PreReflect`** — decision-class hook, [`crate::hooks::EventClass::Write`](../src/hooks/timeouts.rs),
  5-second deadline budget (same as the other `Write`-class hooks
  `pre_store`, `pre_delete`, `pre_promote`, …). Fires inside
  `db::reflect_with_hooks` at **step 4** — after sources are loaded
  and the proposed depth is computed, **before step 5** runs the
  cap check, and well before the write transaction opens.
- **`PostReflect`** — notify-class hook,
  [`crate::hooks::EventClass::Write`](../src/hooks/timeouts.rs), 5-second
  deadline budget. Fires inside `db::reflect_with_hooks` at **step 7**
  — after `COMMIT` succeeds. Post-handlers read the fully-durable
  reflection memory + its `reflects_on` links via the same
  connection.

The hook decision surface is the narrow
[`ReflectHookDecision`](../src/db.rs) enum:

```rust
pub enum ReflectHookDecision {
    /// Continue with the reflection unchanged. Default decision.
    Allow,
    /// Reject the reflection. Propagates as
    /// `ReflectError::HookVeto { reason, code }` distinct from the
    /// Task 5 substrate cap refusal so callers can disambiguate
    /// caller-policy refusals from substrate-policy refusals.
    Deny { reason: String, code: u16 },
}
```

Returning `Deny` from a `PreReflect` handler short-circuits the
reflection and propagates as
`ReflectError::HookVeto`, which surfaces on the wire as
`"REFLECTION_HOOK_VETO (code=<N>): <reason>"`. Notify-class
`PostReflect` handlers cannot veto — their return value is ignored
beyond logging.

**Explicit non-interaction with the Task 5 audit.** A `PreReflect`
hook veto does **not** emit a Task 5 `reflection.depth_exceeded`
audit row. The Task 5 row is the substrate's tamper-evident record
that the *substrate* refused the reflection on cap grounds.
Caller-policy refusals (hook vetoes) carry their own provenance via
the hook's own audit channel — conflating them with substrate-cap
refusals would dilute the cap-refusal audit signal and mis-attribute
the refusal source.

## Curator mode — Pattern 4 (L2-1)

**Landed in v0.7.0 (L2-1, [commit `c3f6e82`](https://github.com/alphaonedev/ai-memory-mcp/commit/c3f6e82), [issue #666](https://github.com/alphaonedev/ai-memory-mcp/issues/666)).**

The substrate primitive (`memory_reflect`) is one synchronous write
per caller-driven reflection. The **reflection-pass curator** is the
asynchronous orchestrator that walks the namespace, clusters
`Observation`-kind memories by namespace + temporal proximity +
recall co-occurrence proxy, asks the configured LLM to summarise the
pattern, and persists each summary as a typed
`MemoryKind::Reflection` through the same substrate path
([`storage::reflect_with_hooks`](../src/storage/reflect.rs)). One
level of reflection per pass; multi-level chains form naturally over
repeated passes when the namespace governance `max_reflection_depth`
permits.

Key contracts ([`src/curator/reflection_pass.rs`](../src/curator/reflection_pass.rs)):

- **Opt-in per namespace.** `ReflectionPassConfig.enabled` defaults
  to `false`. The curator skips a namespace entirely unless an
  operator turns it on in the governance JSON. Reflection depends
  on the Ollama LLM and on a deliberate operator choice to write new
  rows under a namespace; we never enable it by default.
- **Eligibility gate.** Every cluster member must be
  `MemoryKind::Observation`. Reflections never fold into a parent
  reflection in this pass — that is what multi-pass execution buys
  the operator.
- **Cluster sizing.** `MIN_CLUSTER_SIZE = 3` (a pattern derived from
  two observations is just a pair); `MAX_CLUSTER_SIZE = 12`
  (prevents mega-merges where every observation in a namespace
  collapses into one reflection).
- **Temporal window.** 7 days (`TEMPORAL_WINDOW_DAYS`). Two
  observations within 7 days of each other (by `created_at`), in the
  same namespace, with `access_count >= 1` (substrate proxy for
  recall co-occurrence) are candidates.
- **Cap honored.** The curator's per-namespace `max_depth` is a
  guard rail *above* the substrate cap. The
  `GovernancePolicy.effective_max_reflection_depth` check inside
  `db::reflect` is still authoritative — the curator cannot launder
  depth past the substrate enforcement gate.
- **Atomicity inherited.** Persisting a curator-derived reflection
  goes through the same atomic insert + N `reflects_on` link writes
  as a caller-driven `memory_reflect`. Failure rolls back; no
  half-written cluster.

Operator-facing surface lives in `ai-memory curator --reflect`
([`src/cli/curator.rs`](../src/cli/curator.rs)). Operational runbook
sits at [`docs/RUNBOOK-curator-soak.md`](RUNBOOK-curator-soak.md).

## Federation behavior (L2-2)

**Landed in v0.7.0 (L2-2, [commit `0b1c9cc`](https://github.com/alphaonedev/ai-memory-mcp/commit/0b1c9cc), [issue #667](https://github.com/alphaonedev/ai-memory-mcp/issues/667)).**

Reflection-row federation is governed by **local territorial
sovereignty over depth**. A peer cannot launder depth across hosts
by syncing a depth-N reflection into a stricter receiver.

[`src/federation/reflection_bookkeeping.rs`](../src/federation/reflection_bookkeeping.rs)
guarantees three behaviours on the receive path:

1. **Origin stamping.** Every inbound reflection memory gets
   `metadata.reflection_origin = { peer_origin, original_depth,
   local_depth_at_arrival }` stamped on import. `peer_origin` is the
   substrate identity of the peer that pushed us the row;
   `signing_agent` is the original author (preserved across hops via
   `metadata.agent_id`); `original_depth` is the wire-truth depth as
   delivered; `local_depth_at_arrival` is the receiver's effective
   cap *at the moment of arrival* (so an after-the-fact tightening of
   the cap is visible on every imported row).
2. **Derived-write enforcement.** A NEW reflection derived locally
   from one or more imported rows is checked against the LOCAL cap
   regardless of the source peers' caps. Cross-peer chain extension
   cannot launder depth.
3. **Inspection surface.** The MCP tool
   `memory_reflection_origin` (tool count bump: 60 → 61 in this wave)
   answers "where did this reflection come from?" for any memory id,
   returning the structured
   `{memory_id, peer_origin, signing_agent, original_depth,
   local_depth_at_arrival, is_reflection}` envelope.

Depth on the column is **preserved** across federation — we never
silently rewrite incoming depth values. Enforcement happens on
write-time decisions about derived rows, not on import.

## Invalidation propagation (L2-3)

**Landed in v0.7.0 (L2-3, [commit `3f419be`](https://github.com/alphaonedev/ai-memory-mcp/commit/3f419be), [issue #668](https://github.com/alphaonedev/ai-memory-mcp/issues/668)).**

When a `Reflection`-kind memory is superseded by another reflection
(i.e. a `Reflection → Reflection` `supersedes` edge lands via
`memory_link`), the substrate fires
[`propagate_reflection_invalidation`](../src/notification/invalidation.rs).
For every memory whose `reflects_on` edge points at the now-superseded
reflection, the substrate writes one **notification memory** under
`<dependent.namespace>/_invalidations` carrying:

- `metadata.notification_kind = "reflection_invalidation"`
- a four-tuple `{dependent_id, invalidated_id, invalidating_id,
  timestamp}` so downstream curators / operators can act on the
  signal

The wave is **notification, NOT cascade.** Dependents are *flagged
for operator/curator review*, never auto-superseded. The substrate
refuses to mutate caller-visible rows under any invalidation pathway
— operator review remains the only path for promoting an
invalidation into an actual supersession.

Read-only inspection lives at the MCP
`memory_dependents_of_invalidated` tool (tool count bump 61 → 62 in
this wave). The tool returns
`{memory_id, count, dependents: [{id, namespace}]}` without firing
the walker; the walker only fires from the `memory_link` handler on
the Reflection→Reflection supersedes path.

## Reranker boost (L2-8)

**Landed in v0.7.0 (L2-8, [commit `90291c0`](https://github.com/alphaonedev/ai-memory-mcp/commit/90291c0), [issue #673](https://github.com/alphaonedev/ai-memory-mcp/issues/673)).**

Reflections are higher-information rows than the observations they
generalise over. The recall pipeline acknowledges that with a
**reflection-aware reranker boost** applied AFTER the cross-encoder
blend (see [`src/reranker.rs`](../src/reranker.rs)):

```text
per_depth_factor = 1.0 + per_depth_increment * min(reflection_depth, max_depth_cap)
final_score      = base_score * (kind == Reflection ? boost * per_depth_factor : 1.0)
```

The defaults are pinned in `ReflectionBoostConfig`:

| Field | Default | Behaviour |
|---|---|---|
| `boost` | `1.2` | Multiplicative boost for `Reflection`-kind rows. `1.0` is the documented kill-switch (reproduces pre-L2-8 ranking exactly). |
| `per_depth_increment` | `0.05` | Additional multiplier per depth level. |
| `max_depth_cap` | `3` | Mirrors `effective_max_reflection_depth`. Deeper rows clamp to this cap; the multiplier is bounded. |

The boost is **opt-in at the daemon level** — set via
`reflection_boost = { boost, per_depth_increment, max_depth_cap }`
in `config.toml` or the equivalent capabilities-fed runtime config.
A `boost = 1.0` config is reported honestly in capabilities as "no
ranking change" so operators can verify the kill-switch took effect.

## Reflection-as-skill (L2-6, closing the loop)

**Landed in v0.7.0 (L2-6, [commit `505c538`](https://github.com/alphaonedev/ai-memory-mcp/commit/505c538), [issue #671](https://github.com/alphaonedev/ai-memory-mcp/issues/671)).**

The closing-loop primitive of the grand slam: a `Reflection`-kind
memory at depth ≥ 1 can be **promoted** to a reusable Agent Skill via
the MCP tool `memory_skill_promote_from_reflection`. The substrate
constructs an agentskills.io-compliant SKILL.md whose frontmatter
carries:

- `metadata.derived_from_reflection_id` — the source reflection's id
- `metadata.original_reflection_depth` — the source reflection's
  depth at promotion time
- One `references/source_{i}.md` resource per `reflects_on` source

Promotion refuses depth-0 reflections (no synthesised insight to
promote) and refuses depths below
`namespace.governance.skill_promotion_min_depth` (default `1`). The
**round-trip digest guarantee** holds: promote → export → re-register
produces the IDENTICAL SHA-256 digest as the in-DB row — the lineage
is preserved cryptographically across promotion and re-registration.

Full surface is documented in [`docs/agent-skills.md`](agent-skills.md);
this section pins the substrate-side contract for the reflection ↔
skill bridge.

## Forensic export

The forensic-bundle and `verify-reflection-chain` surfaces are
documented in [`docs/forensic-export.md`](forensic-export.md). Both
are the procurement-grade audit path for reflection chains: a
single tar an external auditor can re-verify with no daemon state,
just the public keys of the signing agents.

## Substrate authority claim — v0.7.0 Option B foundation

v0.7.0 ships **Option B** of the substrate-authority programme:
the L1-6 substrate rules-enforcement engine
([`src/governance/rules_store.rs`](../src/governance/rules_store.rs),
[`src/governance/agent_action.rs`](../src/governance/agent_action.rs),
issue [#693](https://github.com/alphaonedev/ai-memory-mcp/issues/693))
ships **un-wired** to the live write path by default — it is the
operator-keypair-signed rule store, the bypass-impossibility test
fleet, and the `check_agent_action` enforcement helper, and it
runs on `governance::storage::insert` as a pre-write hook (L1-6
Deliverable E, [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691)).
The end-to-end "100% of write paths go through the substrate"
coverage is a separate v0.8.0 epic
([#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697))
that wires the rule engine into every adapter write path with the
full bypass-impossibility surface.

**What the substrate DOES today (v0.7.0):**

- Operator-signed Ed25519 attestation on every seed rule, verified
  on load (`verify_rule_signature` in
  [`src/governance/rules_store.rs`](../src/governance/rules_store.rs)).
- Bypass-impossibility integration tests covering the
  `storage::insert` pre-write hook, the `check_agent_action`
  helper, and the operator-keypair gating on rule mutations
  ([`tests/governance/`](../tests/governance/)).
- MCP read-only inspection of the rule corpus
  (`memory_rule_list`, `memory_check_agent_action`); rule
  mutation is operator-only via CLI/HTTP with the signed operator
  key per [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691)
  design revision 2026-05-13.
- The reflection-depth cap (`db::reflect`), reflection-derived
  caps in federation receive
  ([`src/federation/reflection_bookkeeping.rs`](../src/federation/reflection_bookkeeping.rs)),
  and the substrate `MemoryError::ReflectionDepthExceeded`
  refusal continue to enforce reflection-specific authority
  without depending on the rule engine wiring.

**What the substrate does NOT yet do (v0.7.0 → v0.8.0 #697):**

- 100% wiring of `check_agent_action` into every adapter write
  path (SQLite + Postgres). Today the hook is on `storage::insert`
  per L1-6 Deliverable E; other write surfaces are still being
  rolled into the engine as part of the v0.8.0 epic.
- Cascade rollback (Pillar 2.5) — see Forward roadmap.

The audit-honest framing: **substrate authority is a foundation in
v0.7.0, a complete cover in v0.8.0.** Operators evaluating the
authority claim today should read this section, the v0.7.0 release
notes, and [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)
together — and treat any "100% substrate authority" marketing
that elides the wiring gap as inaccurate.

## Forward roadmap

- **G7+** — MCP wire-in of `hooks.toml` → `ReflectHooks` bridge. The
  v0.7.0 `memory_reflect` MCP handler ships an unreachable
  `HookVeto` arm pending that bridge; the wire surface is forward-
  compatible but the production handler does not yet dispatch
  `pre_reflect` / `post_reflect` events. G7+ is the ticket where the
  bridge lands.
- **v0.8.0 Pillar 2.5 — cascade rollback.** The L2-3 invalidation
  pathway today is *notification only*: dependents of an
  invalidated reflection are flagged under
  `<dependent.namespace>/_invalidations` for operator review, never
  auto-superseded. Pillar 2.5 introduces an opt-in,
  operator-signed cascade-rollback verb that walks the
  `reflects_on` chain in the opposite direction and rolls back
  every dependent reflection downstream of an invalidated source
  inside a single transactional envelope. The substrate primitive
  (notification + the `memory_dependents_of_invalidated`
  inspection surface) is the precondition; the cascade is the v0.8
  operator-driven extension.
- **v0.8.0 substrate-authority epic
  ([#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)).**
  Wires the L1-6 substrate rules engine into 100% of adapter write
  paths with the full bypass-impossibility test fleet — the
  complete cover above the v0.7.0 Option B foundation.
- **v0.9.0 composition manifests.** A reflection chain becomes a
  *first-class composition manifest* — a composable, verifiable,
  depth-bounded primitive for codifying agent learnings into
  reusable, signed, machine-checkable assets. The v0.7.0
  `composes_with_reflections` SKILL.md frontmatter field
  ([`docs/agent-skills.md` §SKILL.md format](agent-skills.md#skillmd-format--frontmatter))
  is the wire-compatible precursor: the field name, type, and
  semantics carry forward; the v0.9 epic promotes it from
  declaration to enforceable manifest with cross-skill linkage
  and verifier tooling.

## Cross-references

- CHANGELOG entry: [`../CHANGELOG.md`](../CHANGELOG.md) §v0.7.0
  ("v0.7.0 recursive-learning add-on")
- Release notes: [`v0.7.0/release-notes.md`](v0.7.0/release-notes.md)
  §"Substrate-Native Recursive Learning Grand-Slam"
- Agent Skills primer: [`agent-skills.md`](agent-skills.md)
- Forensic export primer: [`forensic-export.md`](forensic-export.md)
- Tracker issue: [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655)
- L2 wave tracker issues: [#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666) (curator), [#667](https://github.com/alphaonedev/ai-memory-mcp/issues/667) (federation), [#668](https://github.com/alphaonedev/ai-memory-mcp/issues/668) (invalidation), [#669](https://github.com/alphaonedev/ai-memory-mcp/issues/669) (transcript-union replay), [#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670) (forensic bundle), [#671](https://github.com/alphaonedev/ai-memory-mcp/issues/671) (reflection-as-skill), [#672](https://github.com/alphaonedev/ai-memory-mcp/issues/672) (skill composition), [#673](https://github.com/alphaonedev/ai-memory-mcp/issues/673) (reranker boost)
- Substrate-authority issues: [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691) (rules engine, L1-6 Deliverable E), [#693](https://github.com/alphaonedev/ai-memory-mcp/issues/693) (rules engine v2 / Option B), [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697) (v0.8.0 100% coverage epic)
- Task 1 commit: [`f5d8a9e`](https://github.com/alphaonedev/ai-memory-mcp/commit/f5d8a9e)
- Task 2 commit: [`630a6db`](https://github.com/alphaonedev/ai-memory-mcp/commit/630a6db)
- Task 3 commit: [`b51a3f3`](https://github.com/alphaonedev/ai-memory-mcp/commit/b51a3f3)
- Task 4 commit: [`3dc76f3`](https://github.com/alphaonedev/ai-memory-mcp/commit/3dc76f3)
- Task 5 commit: [`c61a05b`](https://github.com/alphaonedev/ai-memory-mcp/commit/c61a05b)
- Task 6 commit: [`fbf093c`](https://github.com/alphaonedev/ai-memory-mcp/commit/fbf093c)
- L2 merge commits: [`c3f6e82`](https://github.com/alphaonedev/ai-memory-mcp/commit/c3f6e82) (L2-1), [`0b1c9cc`](https://github.com/alphaonedev/ai-memory-mcp/commit/0b1c9cc) (L2-2), [`3f419be`](https://github.com/alphaonedev/ai-memory-mcp/commit/3f419be) (L2-3), [`a50b34c`](https://github.com/alphaonedev/ai-memory-mcp/commit/a50b34c) (L2-4), [`bb870b3`](https://github.com/alphaonedev/ai-memory-mcp/commit/bb870b3) (L2-5), [`505c538`](https://github.com/alphaonedev/ai-memory-mcp/commit/505c538) (L2-6), [`0966b57`](https://github.com/alphaonedev/ai-memory-mcp/commit/0966b57) (L2-7), [`90291c0`](https://github.com/alphaonedev/ai-memory-mcp/commit/90291c0) (L2-8)
- v0.7.0 epic scope: [`v0.7/V0.7-EPIC.md`](v0.7/V0.7-EPIC.md)
- ROADMAP context: [`../ROADMAP2.md`](../ROADMAP2.md) §7.4 (recursive
  learning) and §Pillar 2.5 (reflection-pass curator + cascade rollback)
- Reproducibility script: [`../scripts/reproduce-recursive-learning.sh`](../scripts/reproduce-recursive-learning.sh)
