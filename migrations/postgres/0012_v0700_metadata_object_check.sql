-- v0.7.0 M15 — Postgres schema v30: enforce that `memories.metadata` is a
-- JSON object (not array, scalar, or NULL). The `scope_idx` and
-- `agent_id_idx` generated columns extract from JSONB via `->>` operators;
-- when metadata is anything other than an object the extraction silently
-- returns NULL, hiding policy/scope misconfiguration. The CHECK constraint
-- closes that gap so a malformed metadata payload is rejected at the
-- write boundary instead of degrading to "no scope" / "no agent_id" rows.
--
-- This migration is idempotent — re-running it against an already-stamped
-- v30 database is a no-op (NOT VALID skips existing rows; the constraint
-- is then VALIDATED, which scans the table once but only succeeds if all
-- rows pass).

-- Pre-flight: any pre-existing rows that violate the invariant need to be
-- repaired or removed before the constraint can be added. v0.6.x rows
-- always stamp metadata as `{}` (or a richer object), so this should
-- always be a no-op in practice; the guard is here so a hand-crafted row
-- from a future tool doesn't trip the migration.
DO $$
DECLARE
    bad_count BIGINT;
BEGIN
    SELECT COUNT(*) INTO bad_count
      FROM memories
     WHERE jsonb_typeof(metadata) IS DISTINCT FROM 'object';
    IF bad_count > 0 THEN
        RAISE EXCEPTION
            'M15 migration aborted: % rows have non-object metadata; \
             repair them (UPDATE memories SET metadata = ''{}''::jsonb WHERE id = ...) \
             before re-running the migration',
            bad_count;
    END IF;
END
$$;

ALTER TABLE memories
    ADD CONSTRAINT memories_metadata_is_object
    CHECK (jsonb_typeof(metadata) = 'object')
    NOT VALID;

ALTER TABLE memories
    VALIDATE CONSTRAINT memories_metadata_is_object;
