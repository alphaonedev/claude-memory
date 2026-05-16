# Atomisation substrate primitive (v0.7.0 WT-1)

The v0.7.0 WT-1 wave ships substrate-native **atomisation**: a curator
pass that decomposes a long memory into 2-10 atomic propositions,
writes each atom as a first-class memory carrying an `atom_of`
back-pointer plus a signed `derives_from` edge, and archives the
parent so recall surfaces the atoms in its place. The decomposition is
auditable end-to-end — the parent → atoms chain ships inside the
forensic bundle alongside two `atomisation_complete` `signed_events`
rows so an offline reviewer reconstructs the lineage without DB access.

## Why it exists

Long-form memories (post-mortems, transcripts, structured notes)
recall poorly against pointed agent queries because FTS5 and the
HNSW embedding both blur over content that spans many propositions.
Splitting the long body into atom-sized rows lets recall match the
specific claim the agent asked about while preserving the parent in a
read-only archived state — and the substrate, not the application,
owns the discipline. The `derives_from` edge keeps lineage intact for
audit; the `atom_of` foreign key keeps the structural chain queryable
without joining through a relation table.

## Flow

```
insert long memory  →  curator (Gemma 4 + tiktoken-rs)  →  N atoms
       │                       │                              │
       │                       │                              ├─→ atom_of = parent_id (FK)
       │                       │                              ├─→ MemoryLink(derives_from, signed)
       │                       │                              └─→ post_store hook chain fires per atom
       │                       │
       │                       └──→ signed_events: atomisation_complete (×2)
       │
       └─→ archived_at stamped, atomised_into = N (separate transaction)
```

1. **Insert.** A normal `memory_store` write lands in the substrate.
2. **Curator.** The `LlmCurator` in
   [`src/atomisation/curator.rs`](../src/atomisation/curator.rs)
   issues a Gemma 4 prompt and validates the response per the
   `tiktoken-rs` `cl100k_base` token budget (default 200 tokens per
   atom). Out-of-budget atoms trigger a single retry; a second
   over-budget response collapses to `CuratorFailed` (no silent retry
   storm — the audit-honest STOP is deliberate).
3. **Per-atom write.** Each atom is written as its own
   `MemoryKind::Observation` row inside a fresh transaction so the
   `pre_store` / `post_store` / `pre_link` / `post_link` hook chain
   fires per atom. A governance refusal mid-batch surfaces with the
   refused atom index; prior atoms stay committed (they were valid
   writes by themselves).
4. **Archive.** The parent is archived in a *separate* transaction
   after the last atom commits — `archived_at` stamped via
   `metadata.atomisation_archived_at`, `atomised_into` set to the atom
   count. Splitting the archive write is deliberate: the per-atom
   hooks fire on a live parent, so the WT-1-C resolver can still walk
   the chain during hook callbacks.

## Operator interfaces

Three operator-facing surfaces drive the same engine, gated for
different latency / consent profiles.

### Namespace policy (auto)

The `auto_atomise` field on
[`crate::models::GovernancePolicy`](../src/models/namespace.rs)
enables the WT-1-D pre_store hook
([`src/hooks/pre_store/auto_atomise.rs`](../src/hooks/pre_store/auto_atomise.rs)).
When a namespace's `metadata.governance.auto_atomise = true`, every
successful `memory_store` enqueues a curator pass on a detached worker
thread. The store response **never blocks** on the curator — failures
inside the worker are notify-class (logged via `tracing::warn`, never
propagated). Operators opt in per namespace; the default is off.

### MCP tool (interactive)

The `memory_atomise` tool (Family::Power, WT-1-C) decomposes a memory
by id in the foreground. Returns
`{source_id, atom_ids, atom_count, archived_at}` on success. A second
call without `force_re_atomise=true` returns the existing atom ids as
an informational envelope (`already_atomised: true`). Smart-tier
gated: keyword-tier daemons refuse with `TIER_LOCKED` before any DB
read.

### CLI (batch)

`ai-memory atomise <memory_id>` (WT-1-F) is the operator-side wrapper:
same tier gating, same curator construction, stable exit codes (0
success, 1 informational, 3 tier-locked, 4 curator-failed,
5 governance-refused, 6 db-error). `--force` re-atomises a previously-
atomised source; old atoms are retained, `atomised_into` updates to
the fresh count. `--json` emits structured envelopes for shell
pipelines.

## Schema

Three columns and one link relation:

| Column / relation                | Direction          | Set by       |
|----------------------------------|--------------------|--------------|
| `memories.atom_of`               | atom → parent (FK) | atomiser     |
| `memories.atomised_into`         | parent → count     | atomiser     |
| `metadata.atomisation_archived_at` | parent timestamp | atomiser     |
| `MemoryLinkRelation::DerivesFrom`| atom → parent edge | atomiser     |

`atom_of` is a structural foreign key (schema v36); the
`derives_from` edge is the signed audit anchor (Ed25519 over the
canonical CBOR `SignableLink` bytes). The two duplicate each other
deliberately — the FK keeps `atom_of` queries cheap; the signed edge
keeps the relationship verifiable offline.

## LlmCurator

The production curator is `LlmCurator<OllamaClient>` in
[`src/atomisation/curator.rs`](../src/atomisation/curator.rs):

- **Model.** Gemma 4 (E2B at smart tier). The prompt is pinned in
  `GEMMA4_ATOMISATION_PROMPT_TEMPLATE` and surfaces the
  envelope `2 ≤ N ≤ 10 atoms, ≤ max_atom_tokens` directly to the LLM
  so a malformed response is rare.
- **Token budget.** Validated post-response with
  `tiktoken_rs::cl100k_base`. Atoms above the budget trigger a single
  retry with the explicit "you exceeded the budget" feedback prompt.
- **Audit-honest STOP.** After one malformed-response retry, a second
  failure collapses to `CuratorFailed` rather than looping. This is
  deliberate: silent retries hide a real prompt drift and burn token
  budget without operator consent.

## Recall semantics

Default recall surfaces atoms in place of the archived parent. The
SQL guard from WT-1-E is shared by `recall`, `recall_hybrid`, and
`search`:

```sql
AND NOT (
  m.atomised_into IS NOT NULL AND m.atomised_into > 0
  AND json_extract(m.metadata, '$.atomisation_archived_at') IS NOT NULL
)
```

The guard fires only when **both** signals are present (a partial-
state row — e.g. a crash between the column flip and the metadata
write — still surfaces under default recall so the operator can
recover the situation). The `include_archived = true` argument to
`recall` / `recall_hybrid` disables the filter; the forensic export
path uses it so an auditor sees the full chain.

## Forensic preservation

`ai-memory export-forensic-bundle --memory-id <id>` walks both
directions of the atomisation chain: from a parent id it folds in
every atom row; from an atom id it folds in the parent. The bundle
manifest carries an `AtomisationEnvelope` on each touched memory
(`atomised_into`, `archived_at`, `atom_ids`, `atom_of`) so the
auditor reconstructs the structure from a single envelope. Two
`atomisation_complete` `signed_events` rows ship in the bundle's
signed-events directory so the Ed25519 chain re-verifies offline.

The `--include-atomisation-chain=false` flag drops the chain
enrichment when an auditor only needs the canonical post-atomisation
surface and not the historical record.

## Synchronous mode

v0.7.x Form 2 (issue #755, Batman framework alignment) adds an
opt-in `AutoAtomiseMode::Synchronous` namespace policy variant. The
default behaviour (`Deferred`, equivalent to the WT-1-D semantics
above) remains unchanged; operators who need Batman's exact "decompose
THEN embed" order set the policy explicitly.

### Resolution table

| `auto_atomise` | `auto_atomise_mode` | Effective behaviour |
|----------------|---------------------|---------------------|
| `None` / `false` | any              | Off (no atomisation) |
| `Some(true)`     | `None`           | Deferred (legacy WT-1-D) |
| `Some(true)`     | `Some(Off)`      | Off (explicit disable wins) |
| `Some(true)`     | `Some(Deferred)` | Deferred (explicit) |
| any              | `Some(Synchronous)` | Synchronous (Form 2 path) |

### What changes in Synchronous mode

When the policy resolves to `Synchronous`:

1. The MCP `memory_store` handler SKIPS source embedding (line ~505
   in `src/mcp/tools/store.rs`).
2. `run_synchronous_auto_atomise` runs the curator pass INSIDE the
   handler, BEFORE the response returns.
3. Atoms are inserted as first-class memories on the standard write
   path; each gets its normal embed-on-insert pass.
4. The source memory is archived with `atomised_into = N` and
   `metadata.atomisation_archived_at = <RFC3339>` BEFORE the response
   returns — recall sees the atoms immediately, not the source blob.
5. The response envelope carries `atomise_mode: "synchronous"` and
   `atomise_outcome: "atomised" | "skipped_*" | "failed"` so the
   caller can verify the substrate did what the policy asked for.

### When to choose which mode

| Concern | Deferred (default) | Synchronous (Form 2) |
|---------|--------------------|----------------------|
| `memory_store` latency | ≤ 5% overhead | curator-bound (seconds) |
| Source visible until curator runs | Yes (as one blob) | No (atoms surface immediately) |
| Decompose-before-embed order | No (source embedded first) | Yes |
| Recall semantics post-write | Eventually atoms; source covers gap | Atoms only |
| Curator-failure blast radius | Notify-class (logged) | Notify-class (logged, source still committed unembedded) |

Pick `Synchronous` when an agent's next `memory_recall` MUST see
atom-grained results without waiting for the worker thread (e.g. a
hot batch-ingest path where the agent re-queries inside the same
agent turn). Pick `Deferred` when `memory_store` latency matters
more than recall freshness — the worker thread will catch up within
a few seconds in the steady state.

### Configuration

Set the policy on a namespace standard's `metadata.governance` blob:

```json
{
  "governance": {
    "write": "any",
    "auto_atomise_mode": "synchronous",
    "auto_atomise_threshold_cl100k": 500,
    "auto_atomise_max_atom_tokens": 200,
    "auto_atomise_max_retries": 1
  }
}
```

The threshold + max-atom-tokens fields are shared with the deferred
path; the only new field on the wire is `auto_atomise_mode`. Federation
peers running pre-Form-2 v0.7.0 deserialise the absent field as `None`
and fall back to the legacy `auto_atomise` boolean resolution, so no
replication drift occurs during a phased rollout.

### Synchronous-mode latency envelope (Cluster-F PERF-5)

When `auto_atomise_mode = "synchronous"`, the curator round-trip runs
INSIDE the operator's `memory_store` call. The curator-retry budget
therefore directly inflates the worst-case `memory_store` latency
envelope by the per-retry exponential backoff (100 ms → 500 ms →
2500 ms).

The substrate splits the retry budget by execution mode so the
Synchronous envelope stays tight without compromising the deferred
path's resilience:

| Mode | Default retries | Total attempts | Worst-case extra latency |
|------|-----------------|----------------|--------------------------|
| Deferred (worker thread) | 3 | 4 | n/a (off the hot path) |
| Synchronous (`pre_store`) | **1** | 2 | ~100 ms backoff |

The Synchronous default of **1** retry (the `AtomiserConfig::sync_curator_max_retries`
compiled default) caps the worst-case at a single backoff (100 ms).
Operators who need higher resilience on a specific Synchronous-mode
namespace at the cost of longer envelopes override per-namespace via
`auto_atomise_max_retries`:

```json
{
  "governance": {
    "auto_atomise_mode": "synchronous",
    "auto_atomise_max_retries": 3
  }
}
```

Setting `auto_atomise_max_retries: 3` restores the pre-Cluster-F
behaviour (4 total attempts, up to ~3.1 s extra envelope). The
deferred path ignores this override; it always uses
`AtomiserConfig::curator_max_retries` (default 3) since it runs on a
detached worker thread.

## See also

- [Cookbook recipe — basic flow](../cookbook/atomisation/01-basic-flow.sh) — hermetic end-to-end reproduction (no LLM).
- [`tests/atomisation/`](../tests/atomisation) — acceptance suite pinning curator + engine semantics.
- [`tests/auto_atomise/`](../tests/auto_atomise) — pre_store hook coverage (WT-1-D).
- [`tests/form_2_synchronous_atomise.rs`](../tests/form_2_synchronous_atomise.rs) — Form 2 synchronous-mode acceptance tests (#755).
- [`tests/wt1c_mcp_atomise.rs`](../tests/wt1c_mcp_atomise.rs) — MCP tool wire shape.
