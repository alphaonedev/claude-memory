-- v0.6.3.1 (P4, audit G1) — Governance inheritance backfill.
--
-- Adds the `inherit` field to existing `metadata.governance` policy
-- objects with default `true`, preserving the architecture page T2
-- promise of "Hierarchical policy inheritance (default at `org/`,
-- overridable at `org/team/`)".
--
-- Why a backfill at all? `GovernancePolicy::inherit` is deserialized
-- with `#[serde(default = "default_inherit")]` (true), so reads of
-- pre-existing rows already report `inherit: true`. The backfill
-- below makes the field **physically present** in stored JSON so:
--   1. The capabilities/standard-display surface is consistent — an
--      operator running `memory_namespace_get_standard` after upgrade
--      sees the field they can subsequently flip to false, without
--      first re-emitting the policy.
--   2. Downstream peers using SQL-side `json_extract(...,
--      '$.governance.inherit')` (e.g. external dashboards or future
--      operator tooling) get a non-null value across the board.
--   3. Sync/replication payloads carry the explicit field so older
--      peers without the model change still round-trip the value.
--
-- Idempotent: only updates rows where the inherit field is absent,
-- and only on memories that already carry a non-null governance
-- object. No-op on databases that haven't seen Task 1.8 yet.

UPDATE memories
SET metadata = json_set(metadata, '$.governance.inherit', json('true'))
WHERE json_extract(metadata, '$.governance') IS NOT NULL
  AND json_type(metadata, '$.governance') = 'object'
  AND json_extract(metadata, '$.governance.inherit') IS NULL;
