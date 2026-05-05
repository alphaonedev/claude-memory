# Migrating from v0.6.4 to v0.7.0

**v0.7.0 — `attested-cortex`** rolls together the v0.6.5 cortex-fluent legibility work with ROADMAP2 §7.3's full v0.7 trust + A2A maturity scope. The substrate becomes both **more articulate** (capabilities v3, named loaders, compacted schemas) and **cryptographically trustworthy** (Ed25519 attestation, sidechain transcripts, programmable hook pipeline, enforced namespace inheritance).

> **Status:** v0.7.0 migration draft — refined as tracks G/H/I/J/K land. Sections marked TODO point at work not yet merged.

---

## What's new at a glance

| Area | What ships | Default behavior | Opt-in surface |
|---|---|---|---|
| Capabilities v3 | `summary`, `to_describe_to_user`, `callable_now`, `agent_permitted_families` | Returned alongside v2 fields | Read v3 fields if present; v2 unchanged |
| Loader tools | `memory_load_family`, `memory_smart_load` | Always-on (replace `memory_capabilities --include-schema` ergonomics) | Call directly from agent loop |
| Hook pipeline | 20 lifecycle events, exec + daemon modes | **No change** — no hooks fire | `~/.config/ai-memory/hooks.toml` |
| Ed25519 attestation | Per-agent keypair, link signing, `attest_level` enum, `signed_events` audit table | `attest_level = "unsigned"` for legacy callers | `ai-memory identity generate` |
| Sidechain transcripts | zstd-3 BLOB store, `memory_transcript_links`, `memory_replay` | Off | `[transcripts]` config per namespace |
| Apache AGE acceleration | Cypher backend for KG ops, `memory_find_paths` | SQLite/CTE path unchanged | Install AGE Postgres extension |
| G1 inheritance enforcement | `resolve_governance_policy` walks the namespace chain | **Behavior change** for pre-v0.6.3.1 v0.6.x users | Per-policy `inherit: bool` (default `true`) |
| Permission system | Refactored governance with rules + modes + hooks → decision | `mode = "advisory"` preserves v0.6.4 semantics on first boot | `ai-memory governance migrate-to-permissions` |

---

## Why this matters

v0.7.0 closes two long-standing gaps at once.

**Legibility gap (cortex-fluent):** the 2026-05-05 NHI Discovery Gate verdict on v0.6.4 came back **6/6 PASS, GATE GREEN**, but reasoning-class LLMs (Grok 4.2 reasoning) didn't find the runtime loader because it lived inside an introspection tool's parameter set. v0.7.0 promotes loaders to first-class tools (`memory_load_family`, `memory_smart_load`) and pre-computes per-agent calibration in `memory_capabilities` v3.

**Trust gap (attested):** v0.6.3 left an Ed25519 `signature` column in `memory_links` that nothing populated (the v0.6.3 audit's "dead column" finding). Hook events were advertised via subscriptions but lifecycle hooks weren't programmable. Permissions were advisory, not enforced. Namespace inheritance was display-only — a parent `Approve` rule didn't actually block a child write. v0.7.0 fills the column, ships the hook pipeline, enforces inheritance, and lands an append-only `signed_events` audit chain.

---

## Action required

Most users on v0.6.4 see **no behavior change** unless they opt in. The exceptions:

1. **Pre-v0.6.3.1 v0.6.x users** — the G1 namespace inheritance fix (already shipped in v0.6.3.1) means parent `Approve` policies now block child writes. See [G1 inheritance fix](#g1-inheritance-fix-behavior-change) below.
2. **Operators with custom governance policies** — run `ai-memory governance migrate-to-permissions --dry-run` before upgrading to preview the migration.
3. **SDK consumers reading `memory_capabilities`** — v3 adds fields; v2 fields remain. No code changes required, but adopt v3 fields for richer per-agent calibration.

---

## Capabilities v3 schema additions

`memory_capabilities` v3 is **additive**. Every v0.6.4 v2 field stays at its current path and shape. v3 layers on:

| Field | Type | Purpose |
|---|---|---|
| `summary` | string | Top-level pre-computed description ("AI Memory MCP exposes a 5-tool core with N additional families available via runtime expansion.") |
| `to_describe_to_user` | string | Human-shaped summary the agent can paraphrase verbatim — eliminates calibration drift |
| `callable_now` | bool (per tool) | Whether this caller may invoke the tool right now (allowlist + profile aware) |
| `agent_permitted_families` | array<string> | Families this caller is allowed to expand into via `memory_load_family` |

**Wire shape (v2 → v3):** the response gains a top-level `schema_version: 3` field. Clients that pin `schema_version: 2` continue to receive the v2 shape — v2 stays supported through v0.7.x.

```json
{
  "schema_version": 3,
  "summary": "AI Memory MCP exposes a 5-tool core ...",
  "to_describe_to_user": "I have access to a memory substrate ...",
  "agent_permitted_families": ["core", "graph"],
  "tools": [
    {
      "name": "memory_store",
      "callable_now": true,
      "...": "..."
    }
  ]
}
```

Backward compat: v0.6.4 SDKs continue to work — they read v2 fields and ignore the new top-level keys.

> **TODO** — track A (A1-A5) lands the v3 fields; track B (B5) updates `memory_capabilities` description to point at the new loaders. Status will be linked here once merged.

---

## New tools

v0.7.0 adds the following MCP tools. Every tool is documented in `docs/API_REFERENCE.md` once the corresponding track merges.

| Tool | Track | One-line description |
|---|---|---|
| `memory_load_family(family)` | B1 | Always-on loader — registers the named family's tools without restarting the MCP server |
| `memory_smart_load(intent)` | B2 | Embedding-matched loader — picks the family that best fits a natural-language intent string |
| `memory_find_paths(source, target, max_depth=5)` | J7 (R2) | Returns paths through the knowledge graph; Cypher on AGE, recursive CTE on SQLite |
| `memory_replay(memory_id)` | I4 | Reconstructs the transcript chain for a memory by traversing `memory_transcript_links` |
| `memory_verify(link_id)` | H4 | Returns `{signature_verified, attest_level, signed_by, signed_at}` for a link |
| `memory_approval_pending` | K10 | Lists pending approval requests |
| `memory_approval_decide(id, decision, remember=forever?)` | K10 | Decides a pending approval; `remember=forever` enables progressive trust |

> **TODO** — tools land per their respective tracks. Track G hook pipeline lands first; H/I/J/K tools follow.

---

## Hook pipeline (opt-in)

v0.7.0 adds 20 lifecycle hook events at every memory operation point — a programmable extension surface that R3 (auto-link inference) and R5 (auto-extraction) build on.

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

**Event matrix (20 events):** `pre_store`, `post_store`, `pre_recall`, `post_recall`, `pre_search`, `post_search`, `pre_delete`, `post_delete`, `pre_promote`, `post_promote`, `pre_link`, `post_link`, `pre_consolidate`, `post_consolidate`, `pre_governance_decision`, `post_governance_decision`, `on_index_eviction`, `pre_archive`, `pre_transcript_store`, `post_transcript_store`.

**Decision contract:** hooks return one of `Allow`, `Modify(delta)` (pre- events only), `Deny{reason, code}`, or `AskUser{prompt, options, default}`. Chain ordering is priority-desc; first `Deny` short-circuits.

**Hot-path constraint:** `post_recall` and `post_search` default to `daemon` mode, preserving the v0.6.3 50ms recall p95 budget. `mode = "exec"` requires explicit override.

> **TODO** — track G (G1-G11) lands the pipeline. Per-task documentation will live under `docs/hooks/` once merged. See [`docs/v0.7/V0.7-EPIC.md`](v0.7/V0.7-EPIC.md#track-g--hook-pipeline-bucket-0) for the full task list.

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

**Append-only `signed_events` audit table** (schema v21) records every signed write — no UPDATE or DELETE through the application layer.

> **Hardware-backed key storage** (TPM / HSM / Secure Enclave) is **out of OSS scope** per ROADMAP2; available in the AgenticMem commercial layer.

> **TODO** — track H (H1-H6) lands attestation. See [`docs/v0.7/V0.7-EPIC.md`](v0.7/V0.7-EPIC.md#track-h--ed25519-attested-identity-bucket-1) for task detail.

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

Schema migration v21 → v22 adds `memory_transcripts` and `memory_transcript_links`. Background sweeper archives transcripts whose memories are all expired, then prunes after a grace period.

`memory_replay(memory_id)` reconstructs the transcript chain for a memory; returns the decompressed text plus span metadata.

> **TODO** — track I (I1-I5) lands transcripts. See [`docs/v0.7/V0.7-EPIC.md`](v0.7/V0.7-EPIC.md#track-i--sidechain-transcripts-bucket-17) for task detail.

---

## Apache AGE acceleration (opt-in)

v0.7.0 detects Apache AGE in Postgres at SAL initialization (`SELECT * FROM pg_extension WHERE extname='age'`). When present, KG operations route through Cypher; otherwise the recursive-CTE path used since v0.6.x stays in place.

Install AGE in your Postgres instance, restart `ai-memory`, and confirm via `ai-memory doctor --kg-backend` (which prints `kg_backend = "age"` or `"cte"`).

**Acceptance gate:** AGE p95 must beat CTE p95 by ≥30% at depth=5 to ship — the bench gate (`feat/v0.7-j-8-age-bench-gate`) enforces it. If AGE isn't faster on your hardware, stay on the CTE path.

> **TODO** — track J (J1-J8) lands AGE. See [`docs/v0.7/V0.7-EPIC.md`](v0.7/V0.7-EPIC.md#track-j--apache-age-acceleration-bucket-2) for task detail.

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

- **Rules** — declarative policies (the existing governance shape)
- **Modes** — `enforce` / `advisory` / `off`
- **Hooks** — programmable from Track G

→ a single `Decision`. Default deny-first; ask-by-default for ambiguous cases.

**Migration tool** (idempotent, dry-run by default):

```bash
ai-memory governance migrate-to-permissions               # dry-run
ai-memory governance migrate-to-permissions --apply       # commit
```

The dry-run prints the proposed `permissions` rows alongside the source `governance` rows; `--apply` writes them. Re-running is safe — already-migrated rows are skipped.

Honest disclosures from v0.6.3.1 close out:

- `permissions.mode = "advisory"` is now actually consulted by the gate (K3)
- `default_timeout_seconds` on `pending_actions` is now enforced by a 60s sweeper (K2)
- `approval.subscribers` events are now actually published through the subscription system (K4)
- `rule_summary` is now populated with a real ordered list of active governance rules (K5)

> **TODO** — track K (K1-K11) lands the permission system. K1 (G1 inheritance) is the **mandatory cutline** — even if everything else slips, K1 ships.

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
ai-memory doctor --kg-backend                             # cte | age
```

Schema migration v20 → v22 (audit_log → signed_events → memory_transcripts) runs automatically on first start. The migration is idempotent.

---

## What did **not** change

- v0.6.4 default tool surface (`--profile core`) is **unchanged**. The five core tools stay the only advertised tools by default.
- Existing v0.6.4 SDKs continue to work against a v0.7.0 server. Capabilities v3 fields are additive; v2 fields stay at their existing paths.
- Memory data — no migration required for stored memories; embeddings, archives, links, governance policies all carry forward.
- HTTP API endpoints — every v0.6.4 route stays at the same path with the same shape.
- The CLI surface (`ai-memory store`, `recall`, etc.) — every v0.6.4 subcommand continues to work unchanged.
- Boot manifest cost — `ai-memory boot` output is independent of attestation, hooks, transcripts, AGE.
- Hook pipeline — **default off.** A v0.7.0 install with no `hooks.toml` behaves identically to v0.6.4 at the lifecycle layer.

---

## Related

- [`docs/v0.7/V0.7-EPIC.md`](v0.7/V0.7-EPIC.md) — single-doc framework for the v0.7.0 sprint
- [`docs/MIGRATION_v0.6.4.md`](MIGRATION_v0.6.4.md) — predecessor migration guide
- [`docs/MIGRATION-v0.6.2-to-v0.6.3.md`](MIGRATION-v0.6.2-to-v0.6.3.md) — earlier migration
- [`ROADMAP2.md §7.3`](../ROADMAP2.md) — original v0.7 spec
- [`CHANGELOG.md`](../CHANGELOG.md) — full v0.7.0 entry (TODO until release tagged)
- v0.7.0 cert campaign in [`alphaonedev/ai-memory-test-hub`](https://github.com/alphaonedev/ai-memory-test-hub) (TODO — `campaigns/v0.7.md` filed at release)
