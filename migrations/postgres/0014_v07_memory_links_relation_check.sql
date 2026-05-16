-- v0.7.0 v0.7.1-fold (#687/#688) — promote `memory_links.relation`
-- validation to a SQL-side CHECK constraint on the Postgres backend.
--
-- Mirrors migrations/sqlite/0027_v07_memory_links_relation_check.sql
-- (schema v33). Postgres supports `ALTER TABLE ADD CONSTRAINT` for
-- CHECK clauses directly, so the rebuild dance the SQLite migration
-- performs is not required here.
--
-- Closed taxonomy mirrors `crate::validate::VALID_RELATIONS` exactly:
--   related_to / supersedes / contradicts / derived_from / reflects_on
--
-- Idempotent: the `DO $$ ... $$` block checks pg_constraint before
-- adding the constraint so re-running the migration is a no-op.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'memory_links_relation_check'
          AND conrelid = 'memory_links'::regclass
    ) THEN
        ALTER TABLE memory_links
            ADD CONSTRAINT memory_links_relation_check
            CHECK (relation IN ('related_to', 'supersedes', 'contradicts', 'derived_from', 'reflects_on'));
    END IF;
END $$;
