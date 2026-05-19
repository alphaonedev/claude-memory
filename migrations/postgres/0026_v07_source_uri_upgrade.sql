-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Provenance Gap 2 (issue #885) — postgres v43 mirror of SQLite
-- schema v46 (`migrations/sqlite/0040_v07_source_uri_backfill.sql`).
--
-- The `memories.source_uri` first-class TEXT column was added at
-- postgres v37 by `0019_v07_form4_provenance.sql` (and inlined into
-- `postgres_schema.sql:143` for greenfield deploys). However, existing
-- pre-v0.7.0 Postgres installs that pre-date the v37 migration also
-- need a guaranteed-present column + partial index BEFORE the v43
-- backfill can run. This migration:
--
--   1. Re-asserts `ADD COLUMN IF NOT EXISTS source_uri TEXT` so the
--      backfill below is safe even if a partial-init somehow skipped
--      the v37 arm.
--   2. Re-asserts the partial index
--      `idx_memories_source_uri ON memories(source_uri) WHERE
--      source_uri IS NOT NULL` so the reciprocal "from this document"
--      query path (`memory_search --source-uri X`,
--      `memory_kg_query --by-source-uri X`) hits an index, not an
--      O(N) JSON-path scan over `metadata`.
--   3. Backfills `source_uri` from `metadata->>'source_uri'` and then
--      from `citations[0]->>'uri'` for legacy rows whose URI was
--      recorded only in the JSON-encoded provenance columns.
--
-- # Backfill order
--
-- 1. Promote `metadata->>'source_uri'` into the column when the column
--    is NULL and the JSON path yields a non-empty string.
-- 2. For any remaining NULL column, lift the first entry's `uri` from
--    the JSON-encoded `citations` array (stored as TEXT to mirror
--    SQLite — we cast through `::jsonb` for the path extraction).
--
-- # Idempotency
--
-- - `ADD COLUMN IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS` are
--   Postgres-native replay-safe primitives.
-- - Every UPDATE is guarded by `WHERE source_uri IS NULL` so re-running
--   the migration on an already-backfilled DB is a no-op.
-- - Pure additive on legacy rows that have neither metadata nor
--   citations — those stay at NULL, contribute zero index pages, and
--   read back unchanged.
--
-- See `tests/store_parity_gaps.rs::verify_gap_2_source_uri` for the
-- regression-pin: 100 memories seeded with `source_uri` must use the
-- `idx_memories_source_uri` partial index (EXPLAIN returns
-- `Index Scan ... idx_memories_source_uri`, not a sequential scan).

ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS source_uri TEXT;

CREATE INDEX IF NOT EXISTS idx_memories_source_uri
    ON memories(source_uri) WHERE source_uri IS NOT NULL;

-- Backfill step 1: lift metadata.source_uri into the column.
UPDATE memories
   SET source_uri = metadata->>'source_uri'
 WHERE source_uri IS NULL
   AND jsonb_typeof(metadata) = 'object'
   AND metadata ? 'source_uri'
   AND length(metadata->>'source_uri') > 0;

-- Backfill step 2: lift citations[0].uri into the column for rows that
-- only have the URI in the citations array. `citations` is stored as
-- TEXT (mirroring SQLite); cast through ::jsonb for the path extract.
UPDATE memories
   SET source_uri = (citations::jsonb -> 0 ->> 'uri')
 WHERE source_uri IS NULL
   AND citations IS NOT NULL
   AND length(citations) > 2
   AND (
       -- Guard the jsonb cast: skip rows whose citations TEXT is not
       -- well-formed JSON (legacy hand-edited rows). The CASE-guard
       -- avoids `invalid input syntax for type json` from killing the
       -- entire transaction.
       CASE
         WHEN citations ~ '^\s*\[' THEN
            jsonb_array_length(citations::jsonb) > 0
         ELSE false
       END
   )
   AND (citations::jsonb -> 0 ->> 'uri') IS NOT NULL
   AND length(citations::jsonb -> 0 ->> 'uri') > 0;
