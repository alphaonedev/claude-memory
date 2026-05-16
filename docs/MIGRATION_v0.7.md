# Migrating from v0.6.4 to v0.7.0

**v0.7.0 — `attested-cortex`** rolls together the v0.6.5 cortex-fluent legibility work with ROADMAP2 §7.3's full v0.7 trust + A2A maturity scope, **plus** (per operator directive 2026-05-09) the originally-v0.7.1 postgres+AGE first-class work, **plus** the post-grand-slam ship-readiness wave (Batman Forms 1-6 + 7th-form Option-B foundation + QW-1/2/3 + reconciliation security sweep). The substrate becomes both **more articulate** (capabilities v3, named loaders, compacted schemas, Batman `MemoryKind` vocabulary, persona/atomisation/multistep-ingest primitives) and **cryptographically trustworthy** (Ed25519 attestation, sidechain transcripts, programmable 25-event hook pipeline, enforced namespace inheritance, V-4 cross-row signed-events hash chain).

> **Status:** Released 2026-05-15 at HEAD `c9472c1`. v0.7.0 closes the `attested-cortex` epic at 69/69 tasks across 11 tracks (A/B/C/D/E/F/G/H/I/J/K), plus the grand-slam recursive-learning + Agent Skills + L1-6 substrate-rules wave, plus the post-grand-slam Forms 1-6 + 7th-form + QW closeout (PRs [#761](https://github.com/alphaonedev/ai-memory-mcp/pull/761)-[#766](https://github.com/alphaonedev/ai-memory-mcp/pull/766)). See [`CHANGELOG.md`](../CHANGELOG.md) for the full release entry and [`docs/v0.7.0/release-notes.md`](v0.7.0/release-notes.md) for the per-area walk-through. The canonical post-grand-slam feature truth lives at [`docs/internal/v070-feature-inventory.md`](internal/v070-feature-inventory.md).

---

## What's new at a glance

| Area | What ships | Default behavior | Opt-in surface |
|---|---|---|---|
| Capabilities v3 | `summary`, `to_describe_to_user`, `callable_now`, `agent_permitted_families`, `schema_version="3"` | Returned alongside v2 fields | Read v3 fields if present; v2 unchanged |
| Loader tools | `memory_load_family`, `memory_smart_load` join `core` | **Always-on under `--profile core`** (now 7 tools, was 5) | Replace `memory_capabilities --include-schema` ergonomics |
| Hook pipeline | **25 lifecycle events**, exec + daemon modes | **No change** — no hooks fire | `~/.config/ai-memory/hooks.toml` ([doc](hook-pipeline.md)) |
| Ed25519 attestation | Per-agent keypair, link signing, `attest_level` enum, `signed_events` audit table | `attest_level = "unsigned"` for legacy callers | `ai-memory identity generate` |
| Signed-events V-4 chain | Cross-row `prev_hash` + `sequence` SHA-256 chain | On for new daemons (backfilled by `migrate_v34_backfill_chain`) | `ai-memory verify-signed-events-chain` ([doc](signed-events-v4.md)) |
| Sidechain transcripts | zstd-3 BLOB store, `memory_transcript_links`, `memory_replay` | Off | `[transcripts]` config per namespace ([doc](sidechain-transcripts.md)) |
| Apache AGE acceleration | Cypher backend for KG ops, `memory_find_paths` | SQLite/CTE path unchanged | Install AGE Postgres extension |
| Postgres-first SAL | `ai-memory serve --store-url postgres://…`, `ai-memory schema-init` | Sqlite default unchanged | Build with `--features sal-postgres` |
| G1 inheritance enforcement | `resolve_governance_policy` walks the namespace chain | **Behavior change** for pre-v0.6.3.1 v0.6.x users | Per-policy `inherit: bool` (default `true`) |
| Permission system | Refactored governance with rules + modes + hooks → decision | `permissions.mode = "enforce"` (was `"advisory"` in v0.6.4) | `ai-memory governance migrate-to-permissions` ([doc](governance.md)) |
| Federation hardening | mTLS + X-API-Key + fingerprint allowlist | Same as v0.6.4 if not configured | Three new `AI_MEMORY_FED_*` env vars ([doc](federation.md)) |
| K8 quota tool | `memory_quota_status` + `/api/v1/quota/status` | Per-agent daily quota tracked, surfaced on demand | [doc](k8-quotas.md) |
| K10 SSE approvals | `/api/v1/approvals/stream` with mandatory HMAC | Off when `permissions.mode = "off"` | [doc](k10-sse-approvals.md) |
| Batman Form 1 (online dedup-and-synthesis) | Single-batch action-emitting LLM on store | Opt-IN via existing autonomy flag | `legacy_per_pair_classifier = true` to revert |
| Batman Form 2 (synchronous atomise) | `memory_atomise` MCP tool + pre-store hook | `auto_atomise_mode = Off` default | `Synchronous | Deferred` ([doc](atomisation.md)) |
| Batman Form 3 (multi-step ingest) | `memory_ingest_multistep` MCP tool | Caller-driven | [doc](multistep-ingest.md) |
| Batman Form 4 (fact provenance) | Citations + source-URI + atom-grain spans | Caller-driven via `memory_store` payload | [doc](provenance.md) |
| Batman Form 5 (auto-confidence) | `memory_calibrate_confidence` MCP tool | Shadow mode by default | Four `AI_MEMORY_CONFIDENCE_*` env vars ([doc](confidence-calibration.md)) |
| Batman Form 6 (MemoryKind vocab) | 10-variant enum, optional auto-classify pre-store hook | `auto_classify_kind = off` default | `regex_only \| regex_then_llm` ([doc](memory-kind-vocab.md)) |
| Batman 7th-form (Layer-4 wiring) | Operator-signed rules `R001..R004`, `memory_check_agent_action`, `memory_rule_list` | Substrate-INTERNAL writes gated; agent-EXTERNAL `callable_now` flag | v0.8.0 full cover per [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697) |
| QW-1 file-backed reflection export | `memory_export_reflection` MCP tool | Opt-IN per namespace | `auto_export_reflections_to_filesystem = true` |
| QW-2 persona-as-artifact | `memory_persona` + `memory_persona_generate` tools, `MemoryKind::Persona` | Opt-IN per namespace | `auto_persona_trigger_every_n_memories = N` ([doc](persona.md)) |
| QW-3 context-offload primitive | `memory_offload` + `memory_deref` tools | Caller-driven | [doc](context-offload.md) |

---

## Why this matters

v0.7.0 closes three long-standing gaps at once.

**Legibility gap (cortex-fluent):** the 2026-05-05 NHI Discovery Gate verdict on v0.6.4 came back **6/6 PASS, GATE GREEN**, but reasoning-class LLMs (Grok 4.2 reasoning) didn't find the runtime loader because it lived inside an introspection tool's parameter set. v0.7.0 promotes loaders to first-class tools (`memory_load_family`, `memory_smart_load`) and pre-computes per-agent calibration in `memory_capabilities` v3.

**Trust gap (attested):** v0.6.3 left an Ed25519 `signature` column in `memory_links` that nothing populated (the v0.6.3 audit's "dead column" finding). Hook events were advertised via subscriptions but lifecycle hooks weren't programmable. Permissions were advisory, not enforced. Namespace inheritance was display-only — a parent `Approve` rule didn't actually block a child write. v0.7.0 fills the column, ships the hook pipeline, enforces inheritance, and lands an append-only `signed_events` audit chain with cross-row hash chaining (V-4 closeout #698).

**Write-time-investment gap (Batman):** the v0.7.0 audit at commit `53b4d39` ([`docs/internal/batman-framework-audit.md`](internal/batman-framework-audit.md)) found 0 of Batman's 6 forms cleanly IMPLEMENTED. The post-grand-slam Forms 1-6 wave + the 7th-form Option-B foundation closed every gap; HEAD `c9472c1` ships all 7 forms IMPLEMENTED.

---

## Action required

Most users on v0.6.4 see **two intentional behavior changes** + a few opt-in surfaces:

1. **`permissions.mode` flips `advisory` → `enforce`** (F8 — Round-2 NHI sweep). Operators who relied on default-permissive behavior must opt back in via `[permissions] mode = "advisory"` in config.toml.
2. **`ai-memory forget --pattern` / `--tier` without `--namespace` requires `--confirm-global`** (F11). Scoped forget is unchanged.
3. **Pre-v0.6.3.1 v0.6.x users** — the G1 namespace inheritance fix (already shipped in v0.6.3.1) means parent `Approve` policies now block child writes. See [G1 inheritance fix](#g1-inheritance-fix-behavior-change) below.
4. **Operators with custom governance policies** — run `ai-memory governance migrate-to-permissions --dry-run` before upgrading to preview the migration.
5. **SDK consumers reading `memory_capabilities`** — v3 adds fields; v2 fields remain. No code changes required, but adopt v3 fields for richer per-agent calibration.

---

## Capabilities v3 schema additions

`memory_capabilities` v3 is **additive**. Every v0.6.4 v2 field stays at its current path and shape. v3 layers on:

| Field | Type | Purpose |
|---|---|---|
| `schema_version` | string (`"3"`) | Top-level version tag. |
| `summary` | string | Top-level pre-computed description ("AI Memory MCP exposes a 7-tool core with N additional families available via runtime expansion.") |
| `to_describe_to_user` | string | Human-shaped summary the agent can paraphrase verbatim — eliminates calibration drift. |
| `callable_now` | bool (per tool) | Whether this caller may invoke the tool right now (allowlist + profile aware). |
| `agent_permitted_families` | array<string> | Families this caller is allowed to expand into via `memory_load_family`. |
| `memory_kind_vocab` | object | Form-6 vocabulary + auto-classify modes (see [`docs/memory-kind-vocab.md`](memory-kind-vocab.md)). |

Clients that pin `schema_version: 2` continue to receive the v2 shape — v2 stays supported through v0.7.x.

Backward compat: v0.6.4 SDKs continue to work — they read v2 fields and ignore the new top-level keys.

To inspect the live v3 response (replace the legacy `doctor --capabilities=v3` recipe with):

```bash
ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq .memory_kind_vocab
```

---

## New MCP tools

v0.7.0 adds the following tools relative to v0.6.4. Every tool ships
in [`src/mcp/registry.rs`](../src/mcp/registry.rs) and is grouped into
a `Family` (see [`src/profile.rs`](../src/profile.rs)). Full
post-grand-slam inventory: [`docs/internal/v070-feature-inventory.md`](internal/v070-feature-inventory.md).

| Tool | Family | Track / form | One-line description |
|---|---|---|---|
| `memory_load_family(family)` | Core | B1 | Always-on loader — registers the named family's tools without restarting the MCP server. |
| `memory_smart_load(intent)` | Core | B2 | Embedding-matched loader — picks the family that best fits a natural-language intent string. |
| `memory_find_paths(source, target, max_depth=5)` | Graph | J7 | Returns paths through the knowledge graph; Cypher on AGE, recursive CTE on SQLite. |
| `memory_replay(memory_id, depth?)` | Graph | I4 / L2-4 | Reconstructs the transcript chain for a memory by traversing `memory_transcript_links`. |
| `memory_verify(link_id)` | Graph | H4 | Returns `{signature_verified, attest_level, signed_by, signed_at}` for a link. |
| `memory_pending_list` | Power | K10 | Lists pending approval requests. **Note:** the original v0.7-alpha drafts called this `memory_approval_pending`; the shipped name is `memory_pending_list`. |
| `memory_pending_approve(id, …)` | Power | K10 | Approves a pending action. HMAC-signed body required. |
| `memory_pending_reject(id, …, remember=forever?)` | Power | K10 | Rejects a pending action; `remember=forever` enables progressive trust. |
| `memory_subscription_dlq_list` | Power | K7 | Lists dead-letter subscription deliveries. |
| `memory_subscription_replay` | Power | K7 | Replays a DLQ entry. |
| `memory_quota_status` | Power | K8 | Returns the caller's per-agent daily quota row. See [`docs/k8-quotas.md`](k8-quotas.md). |
| `memory_reflect` | Power | Recursive-learning Task 4/8 | Substrate primitive that synthesises a reflection over a recall set; depth-capped per namespace. |
| `memory_reflection_origin` | Power | L2-2 / #667 | Reads `reflection_origin` metadata that federation receivers stamp on import. |
| `memory_dependents_of_invalidated` | Power | L2-3 / #668 | Notifies dependents when a Reflection→Reflection `supersedes` edge fires. |
| `memory_check_agent_action` | Power | 7th-form / #691 | Dry-run check against the operator-signed rule corpus. |
| `memory_rule_list` | Power | 7th-form / #691 | Read-only listing of the rule corpus. |
| `memory_export_reflection` | Power | QW-1 | File-backed reflection-chain export. |
| `memory_offload` | Power | QW-3 | Move a large blob out of the agent context window into addressable blob storage. |
| `memory_deref` | Power | QW-3 | Read-side companion to `memory_offload`. |
| `memory_atomise` | Power | WT-1-C / Form 2 | Curator decomposes a long memory into 2-10 atomic propositions. |
| `memory_persona` | Power | QW-2 | Read/write a `MemoryKind::Persona` row. |
| `memory_persona_generate` | Power | QW-2 | Synthesise a persona over the entity's observation set. |
| `memory_ingest_multistep` | Power | Form 3 / #756 | Multi-step ingest orchestrator with deterministic helpers + prompt-cache reuse. |
| `memory_calibrate_confidence` | Power | Form 5 / #758 | Per-source baseline calibration sweep (shadow mode by default). |
| `memory_skill_register` | Other | L1-5 | Register a SKILL.md-format Agent Skill. |
| `memory_skill_list` | Other | L1-5 | List skills. |
| `memory_skill_get` | Other | L1-5 | Read a skill. |
| `memory_skill_resource` | Other | L1-5 | Read a resource referenced by a skill. |
| `memory_skill_export` | Other | L1-5 | Export a skill back to SKILL.md. |
| `memory_skill_promote_from_reflection` | Other | L2-6 / #671 | Promote a Reflection (depth ≥ 1) to a SKILL.md-format Agent Skill. |
| `memory_skill_compositional_context` | Other | L2-7 / #672 | Return a skill body + bounded reflection set ranked by recency + recall count. |

---

## Per-form migration notes

### Form 1 — online dedup-and-synthesis ([#754](https://github.com/alphaonedev/ai-memory-mcp/issues/754))

The store path now issues a **single batch action-emitting LLM call** that emits `add | update | delete | no-op` per existing memory candidate, replacing the v0.6.x per-pair binary yes/no classifier. The new path is gated by the same `autonomous_hooks` toggle that gated the v0.6.x classifier; behavior is opt-in.

To preserve the v0.6.x per-pair behavior on a specific namespace, set on the namespace standard:

```jsonc
{
  "governance": {
    "legacy_per_pair_classifier": true
  }
}
```

The shipped code path is `src/synthesis/mod.rs`. Tests:
[`tests/form_1_synthesis.rs`](../tests/form_1_synthesis.rs).

### Form 2 — synchronous atomise-before-embed ([#755](https://github.com/alphaonedev/ai-memory-mcp/issues/755))

New `memory_atomise` MCP tool + `auto_atomise_mode` namespace-policy field with three settings:

- **`Off`** (default) — caller-driven, no curator pass.
- **`Deferred`** — curator runs out-of-band after the write commits.
- **`Synchronous`** — curator runs inside the pre-store hook chain; the parent memory is archived once atoms commit.

Token budget is governed by `auto_atomise_threshold_cl100k` (default 800) and `auto_atomise_max_atom_tokens` (default 200). Each atom is a first-class memory with an `atom_of` back-pointer and a signed `derives_from` edge.

Operator doc: [`docs/atomisation.md`](atomisation.md). Cookbook: [`cookbook/atomisation/01-basic-flow.sh`](../cookbook/atomisation/01-basic-flow.sh).

### Form 3 — multi-step ingest orchestrator ([#756](https://github.com/alphaonedev/ai-memory-mcp/issues/756))

`memory_ingest_multistep` threads deterministic Jaccard+FTS helpers through prompt-cache-stable LLM stages. The two reference variants (two-phase Understand-Anything exemplar; four-step OpenKB exemplar) both produce reports with a single distinct cache key per run — provable by the cookbook recipe.

Operator doc: [`docs/multistep-ingest.md`](multistep-ingest.md). Cookbook: [`cookbook/multistep-ingest/01-two-phase.sh`](../cookbook/multistep-ingest/01-two-phase.sh).

### Form 4 — fact provenance ([#757](https://github.com/alphaonedev/ai-memory-mcp/issues/757))

No new MCP tool; provenance rides on the existing `memory_store` / `memory_atomise` payloads. Schema migration `0032_v07_form4_provenance.sql` adds the columns. Federation wire shape stays backward-compatible — pre-v0.7.0 peers ignore the new fields.

Operator doc: [`docs/provenance.md`](provenance.md).

### Form 5 — auto-confidence + shadow-mode calibration + freshness decay ([#758](https://github.com/alphaonedev/ai-memory-mcp/issues/758))

`memory_calibrate_confidence` MCP tool + four env vars:

- `AI_MEMORY_AUTO_CONFIDENCE` — enable on-write confidence assignment.
- `AI_MEMORY_CONFIDENCE_SHADOW` — calibrate in shadow mode (no live writes).
- `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE` — shadow-mode sample rate (default 0.1).
- `AI_MEMORY_CONFIDENCE_DECAY` — enable per-memory freshness decay.

Schema migration `0033_v07_form5_confidence_calibration.sql`. Capability registry entry: `CapabilityConfidenceCalibration` at `src/config.rs:1331`.

Operator doc: [`docs/confidence-calibration.md`](confidence-calibration.md).

### Form 6 — MemoryKind Batman vocabulary ([#759](https://github.com/alphaonedev/ai-memory-mcp/issues/759))

10-variant `MemoryKind` enum: `Observation` (default) + `Reflection`, `Persona`, `Concept`, `Entity`, `Claim`, `Relation`, `Event`, `Conversation`, `Decision`. **No schema migration** — `memories.memory_kind TEXT NOT NULL DEFAULT 'observation'` has no CHECK constraint, so new variants land as new string values on the existing column.

Optional `auto_classify_kind` pre-store hook on each namespace standard: `off | regex_only | regex_then_llm`. See [`docs/memory-kind-vocab.md`](memory-kind-vocab.md) for the per-variant matrix and recall-filter wire shapes.

### 7th-form — agent-EXTERNAL Layer-4 wiring ([#760](https://github.com/alphaonedev/ai-memory-mcp/issues/760))

**Status:** Option-B foundation SHIPPED in v0.7.0; full cover at v0.8.0 per [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697).

What v0.7.0 ships:

- Operator-keypair-signed seed rules `R001..R004` (`~/.config/ai-memory/operator.key`).
- `memory_check_agent_action` MCP tool — dry-run check against the rule corpus.
- `memory_rule_list` MCP tool — read-only listing.
- Substrate `storage::insert` pre-write hook surfaces structured refusal via the `RuleRefused` error variant.
- `ai-memory install --harness claude-code --enforce-policy` wires the policy at install time.

What v0.7.0 does NOT ship (v0.8.0 cover):

- Agent-EXTERNAL Bash / FilesystemWrite / NetworkRequest / ProcessSpawn enforcement is `callable_now=false` at the substrate boundary — the rule corpus is consulted on the substrate write path only.

Audit-honest framing: see [`docs/RECURSIVE_LEARNING.md` §Substrate authority claim](RECURSIVE_LEARNING.md#substrate-authority-claim--v070-option-b-foundation). Operator doc: [`docs/policy-engine.md`](policy-engine.md) + [`docs/governance/agent-action-rules.md`](governance/agent-action-rules.md).

---

## Hook pipeline (opt-in)

v0.7.0 ships **25 lifecycle hook events** (20 baseline + 5 grand-slam additions) at every memory operation point — a programmable extension surface that R3 (auto-link inference) and R5 (auto-extraction) build on.

Default: **no behavior change.** Hooks fire only when `~/.config/ai-memory/hooks.toml` exists and registers them.

```toml
[[hook]]
event = "post_store"
command = "/usr/local/bin/auto-link-detector"
priority = 100
timeout_ms = 5000
mode = "daemon"
enabled = true
namespace = "team/*"
```

**Event matrix (25 events):**

- 20 baseline: `pre_store`, `post_store`, `pre_recall`, `post_recall`, `pre_search`, `post_search`, `pre_delete`, `post_delete`, `pre_promote`, `post_promote`, `pre_link`, `post_link`, `pre_consolidate`, `post_consolidate`, `pre_governance_decision`, `post_governance_decision`, `on_index_eviction`, `pre_archive`, `pre_transcript_store`, `post_transcript_store`.
- 5 grand-slam additions: `pre_recall_expand` (G10), `pre_reflect` + `post_reflect` (recursive-learning Task 6/8), `pre_compaction` + `on_compaction_rollback` (L1-7).

**Decision contract:** hooks return one of `Allow`, `Modify(delta)` (pre- events only), `Deny{reason, code}`, or `AskUser{prompt, options, default}`. Chain ordering is priority-desc; first `Deny` short-circuits.

**Hot-path constraint:** `post_recall` and `post_search` default to `daemon` mode, preserving the v0.6.3 50ms recall p95 budget. `mode = "exec"` requires explicit override.

Operator doc: [`docs/hook-pipeline.md`](hook-pipeline.md).

---

## Ed25519 attestation (opt-in)

v0.7.0 fills the dead `signature` column shipped in v0.6.3 with real cryptographic attestation. Per-agent Ed25519 keypair (operator-supplied — not derived from `agent_id`). Outbound signing on every `memory_links` write. Inbound verification against the `observed_by` claim.

```bash
ai-memory identity generate --agent-id ai:claude-code@host:pid-12345
ai-memory identity list
ai-memory identity export-pub --agent-id ai:claude-code@host:pid-12345
```

Keys live at `~/.config/ai-memory/keys/<agent_id>.{pub,priv}` with mode 0600 / 0644.

**`attest_level` enum:** `unsigned` (no keypair present — preserves v0.6.4 backward compat), `self_signed` (active agent has a keypair; outbound writes signed), `peer_attested` (federated link verified against the peer's known public key).

**Append-only `signed_events` audit table** records every signed write — no UPDATE or DELETE through the application layer. The V-4 closeout ([#698](https://github.com/alphaonedev/ai-memory-mcp/issues/698)) added a cross-row hash chain (`prev_hash` BLOB + `sequence` INTEGER) so the chain itself is tamper-evident, not just each row. Verify with:

```bash
ai-memory verify-signed-events-chain --format json
```

Operator doc: [`docs/signed-events-v4.md`](signed-events-v4.md).

> **Hardware-backed key storage** (TPM / HSM / Secure Enclave) is **out of OSS scope** per ROADMAP2; available in the AgenticMem commercial layer.

---

## Sidechain transcripts (opt-in per namespace)

v0.7.0 adds raw conversation/reasoning trail storage in zstd-3-compressed BLOBs, linked to derived memories via `memory_transcript_links`. Substrate for R5 auto-extraction.

Default: **off.** Opt in per namespace via `[transcripts]` config:

```toml
[transcripts."team/*"]
enabled = true
ttl_days = 30
archive_after_days = 7
```

Background sweeper archives transcripts whose memories are all expired, then prunes after a grace period. `memory_replay(memory_id, depth?)` reconstructs the transcript chain; `depth=0` reproduces the pre-L2-4 shape.

Operator doc: [`docs/sidechain-transcripts.md`](sidechain-transcripts.md).

---

## Apache AGE acceleration (opt-in)

v0.7.0 detects Apache AGE in Postgres at SAL initialization (`SELECT * FROM pg_extension WHERE extname='age'`). When present, KG operations route through Cypher; otherwise the recursive-CTE path used since v0.6.x stays in place.

Install AGE in your Postgres instance and restart `ai-memory`. To confirm which backend is live, inspect the v3 capabilities response (which records `kg_backend`):

```bash
ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq '.kg_backend'
```

Or, from the HTTP surface:

```bash
curl -s http://127.0.0.1:9077/api/v1/capabilities | jq '.kg_backend'
```

Returns `"age"` when AGE is detected, `"cte"` when it falls through. (The v0.7-alpha drafts referenced a `ai-memory doctor --kg-backend` flag; that flag was not shipped — use the capabilities-response surface above.)

**Acceptance gate:** AGE p95 must beat CTE p95 by ≥30% at depth=5 to ship — the bench gate (`feat/v0.7-j-8-age-bench-gate`) enforces it. If AGE isn't faster on your hardware, stay on the CTE path.

---

## G1 inheritance fix (behavior change)

> **Note:** this fix already shipped in **v0.6.3.1**. If you upgraded through v0.6.3.1 → v0.6.4 → v0.7.0, you have it already. This section is for users still on **pre-v0.6.3.1 v0.6.x** jumping straight to v0.7.0.

**The change:** `resolve_governance_policy(namespace)` now walks `build_namespace_chain(namespace)` and returns the first non-null policy encountered, not just the leaf. A parent `Approve` policy will now block a child write that previously slipped through.

**Worked example:**

- Parent namespace `team` has `GovernancePolicy(approve_required=true)`
- Child namespace `team/alice` has no policy
- A write to `team/alice` **now requires approval** (pre-v0.6.3.1 it did not)

**Mitigation for backward compat:** every `GovernancePolicy` row gains an `inherit: bool` field (default `true`). Existing rows are backfilled to `inherit=true`. To preserve pre-v0.6.3.1 behavior on a specific child policy, set `inherit=false` so its parent's policy is not consulted.

```sql
UPDATE governance_policies
   SET inherit = false
 WHERE namespace = 'team/alice';
```

Audit log records which ancestor's policy fired on every gate decision.

---

## Permissions migration

v0.7.0 refactors the existing `governance` system into:

- **Rules** — declarative policies (the existing governance shape, now operator-keypair-signed under 7th-form).
- **Modes** — `enforce` (v0.7.0 default) / `advisory` / `off`.
- **Hooks** — programmable from Track G.

→ a single `Decision`. Default deny-first; ask-by-default for ambiguous cases.

**Migration tool** (idempotent, dry-run by default):

```bash
ai-memory governance migrate-to-permissions               # dry-run
ai-memory governance migrate-to-permissions --apply       # commit
```

The dry-run prints the proposed `permissions` rows alongside the source `governance` rows; `--apply` writes them. Re-running is safe — already-migrated rows are skipped.

Honest disclosures from v0.6.3.1 close out:

- `permissions.mode = "advisory"` is now actually consulted by the gate (K3).
- `default_timeout_seconds` on `pending_actions` is now enforced by a 60s sweeper (K2).
- `approval.subscribers` events are now actually published through the subscription system (K4).
- `rule_summary` is now populated with a real ordered list of active governance rules (K5).

Operator docs: [`docs/governance.md`](governance.md), [`docs/policy-engine.md`](policy-engine.md), [`docs/k10-sse-approvals.md`](k10-sse-approvals.md).

---

## Federation hardening

v0.7.0 hardens the v0.6.x federation surface with mTLS + X-API-Key + SHA-256 cert fingerprint allowlist + three new `AI_MEMORY_FED_*` env vars:

- `AI_MEMORY_FED_PEER_ATTESTATION` — require peer Ed25519 attestation on inbound sync.
- `AI_MEMORY_FED_SYNC_TRUST_PEER` — trust the peer's claim about origin agent on inbound sync (default deny).
- `AI_MEMORY_FED_TRUST_BODY_AGENT_ID` — trust the wire body's `agent_id` claim (default deny — use the authenticated peer cert).

Operator doc: [`docs/federation.md`](federation.md).

---

## Upgrade steps

```bash
# 1. Stop the running daemon
ai-memory stop

# 2. Backup your DB before the schema migration
cp ~/.local/share/ai-memory/memory.db ~/.local/share/ai-memory/memory.db.v0.6.4.bak

# 3. Upgrade the binary (pick your channel)
brew upgrade ai-memory                                    # Homebrew
cargo install --git https://github.com/alphaonedev/ai-memory-mcp ai-memory --locked  # crates.io / source

# 4. Preview the permissions migration
ai-memory governance migrate-to-permissions               # dry-run

# 5. Apply if happy
ai-memory governance migrate-to-permissions --apply

# 6. (Optional) Generate an Ed25519 keypair for outbound link signing
ai-memory identity generate --agent-id "$(ai-memory identity suggest-id)"

# 7. Restart
ai-memory start

# 8. Verify
ai-memory doctor --tokens
ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq '.kg_backend'
ai-memory verify-signed-events-chain --format json
```

Schema migrations (sqlite v20 → v34, postgres 0012 → 0020) run automatically on first start of a sqlite-backed daemon and are idempotent. Postgres schema bootstrap is via `ai-memory schema-init` per [`docs/migration-v0.7.0-postgres.md`](migration-v0.7.0-postgres.md).

---

## What did **not** change

- The original v0.6.4 5-tool default surface is **preserved in spirit** — the v0.7 B1/B2 loaders (`memory_load_family`, `memory_smart_load`) joined the `core` family, bringing the default count to 7. No tool was removed from `core`.
- Existing v0.6.4 SDKs continue to work against a v0.7.0 server. Capabilities v3 fields are additive; v2 fields stay at their existing paths.
- Memory data — no migration required for stored memories; embeddings, archives, links, governance policies all carry forward.
- HTTP API endpoints — every v0.6.4 route stays at the same path with the same shape. v0.7.0 adds 8 net-new routes (see [`docs/internal/v070-feature-inventory.md` §"8 new HTTP routes"](internal/v070-feature-inventory.md)).
- The CLI surface (`ai-memory store`, `recall`, etc.) — every v0.6.4 subcommand continues to work unchanged.
- Boot manifest cost — `ai-memory boot` output is independent of attestation, hooks, transcripts, AGE.
- Hook pipeline — **default off.** A v0.7.0 install with no `hooks.toml` behaves identically to v0.6.4 at the lifecycle layer.

---

## Related

- [`docs/v0.7.0/release-notes.md`](v0.7.0/release-notes.md) — full release notes (incl. post-grand-slam ship-readiness wave).
- [`docs/internal/v070-feature-inventory.md`](internal/v070-feature-inventory.md) — canonical feature truth.
- [`docs/v0.7/V0.7-EPIC.md`](v0.7/V0.7-EPIC.md) — single-doc framework for the v0.7.0 sprint.
- [`docs/MIGRATION_v0.6.4.md`](MIGRATION_v0.6.4.md) — predecessor migration guide.
- [`docs/MIGRATION-v0.6.2-to-v0.6.3.md`](MIGRATION-v0.6.2-to-v0.6.3.md) — earlier migration.
- [`docs/migration-v0.7.0-postgres.md`](migration-v0.7.0-postgres.md) — sqlite → postgres migration runbook.
- [`ROADMAP2.md §7.3`](../ROADMAP2.md) — original v0.7 spec.
- [`CHANGELOG.md`](../CHANGELOG.md) — full v0.7.0 entry.
