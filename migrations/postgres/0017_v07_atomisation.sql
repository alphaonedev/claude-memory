-- v0.7.0 WT-1-A — schema v35 (postgres) atomisation foundation.
-- Mirrors SQLite schema v36 (`migrations/sqlite/0030_v07_atomisation.sql`).
--
-- Postgres supports `ADD COLUMN IF NOT EXISTS` (14+) so the migration
-- is a pure idempotent DDL batch. The CHECK constraint on
-- `memory_links.relation` is dropped and re-added with the extended
-- taxonomy (closed set now: related_to / supersedes / contradicts /
-- derived_from / reflects_on / derives_from). Idempotent via
-- `pg_constraint` probe; fresh installs inherit the extended
-- constraint inline from `postgres_schema.sql`.
--
-- # Columns
--
-- - `atomised_into INTEGER` — NULL on legacy rows; positive integer on
--   rows that have been atomised. Mirrors SQLite v36.
-- - `atom_of TEXT REFERENCES memories(id)` — for atom rows, points back
--   to the parent. NULL on non-atom rows. The FK is `ON DELETE NO
--   ACTION` so atomisation parents cannot be deleted while atoms still
--   reference them.
--
-- # CHECK constraint
--
-- `memory_links_relation_check` was added at v32 (postgres) / v33
-- (sqlite) with the closed five-relation set. WT-1-A adds
-- `derives_from` so atomisation provenance edges (atom -> parent) can
-- be expressed as both a structural FK (`memories.atom_of`) AND a
-- typed, signable, federation-safe `memory_links` row. Drop + re-add
-- under one transaction; pg_constraint probe keeps the migration
-- idempotent on a partially-stamped DB.

ALTER TABLE memories ADD COLUMN IF NOT EXISTS atomised_into INTEGER;
ALTER TABLE memories ADD COLUMN IF NOT EXISTS atom_of       TEXT
    REFERENCES memories(id);

CREATE INDEX IF NOT EXISTS idx_memories_atom_of
    ON memories(atom_of) WHERE atom_of IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_memories_atomised_into
    ON memories(atomised_into) WHERE atomised_into > 0;

-- Replace the v32 CHECK with the extended taxonomy. Idempotent: probe
-- pg_constraint and skip when the extended constraint is already in
-- place. The drop-add dance is atomic inside the calling transaction.
DO $$
BEGIN
    -- If the extended constraint is already in place (fresh install via
    -- postgres_schema.sql, or a previous run of this migration), skip.
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'memory_links_relation_check_wt1a'
          AND conrelid = 'memory_links'::regclass
    ) THEN
        -- Drop the v32 constraint if present (named
        -- `memory_links_relation_check`). The IF EXISTS form is the
        -- Postgres-native idempotency primitive.
        ALTER TABLE memory_links
            DROP CONSTRAINT IF EXISTS memory_links_relation_check;
        ALTER TABLE memory_links
            ADD CONSTRAINT memory_links_relation_check_wt1a
            CHECK (relation IN ('related_to', 'supersedes', 'contradicts',
                                'derived_from', 'reflects_on', 'derives_from'));
    END IF;
END $$;
