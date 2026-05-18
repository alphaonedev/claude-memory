-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Provenance Gap 2 (issue #885, schema v46 sqlite).
--
-- Backfills the `memories.source_uri` first-class TEXT column
-- introduced at schema v38 (`0032_v07_form4_provenance.sql`) for
-- legacy rows that recorded the URI under `metadata.source_uri` or as
-- the first entry in the `citations[]` JSON array.
--
-- Before this backfill, callers that store source URIs through the
-- pre-Form-4 surfaces (operator `metadata.source_uri` writes, or the
-- atomisation curator dropping a `Citation` into the `citations`
-- array) could not be reciprocally queried via the
-- `idx_memories_source_uri` partial index because the column itself
-- stayed NULL — only the JSON-path scan over `metadata` would find
-- them, which is O(N).
--
-- # Backfill order
--
-- 1. Promote `metadata.source_uri` into the column when the column is
--    NULL and `json_extract(metadata, '$.source_uri')` yields a
--    non-empty string.
-- 2. For any remaining NULL column, lift the first entry's `uri` from
--    the JSON-encoded `citations` array.
--
-- # Idempotency
--
-- Every UPDATE is guarded by `WHERE source_uri IS NULL` so re-running
-- the migration on an already-backfilled DB is a no-op. Pure additive
-- on legacy rows that have neither metadata nor citations — those
-- stay at NULL, contribute zero index pages, and read back unchanged.
--
-- See `tests/source_uri_column.rs` for the regression-pin: 100
-- memories seeded with `source_uri` must use the
-- `idx_memories_source_uri` partial index (EXPLAIN QUERY PLAN
-- returns `SEARCH ... USING INDEX`, not `SCAN`).

UPDATE memories
   SET source_uri = json_extract(metadata, '$.source_uri')
 WHERE source_uri IS NULL
   AND json_valid(metadata) = 1
   AND json_extract(metadata, '$.source_uri') IS NOT NULL
   AND length(json_extract(metadata, '$.source_uri')) > 0;

UPDATE memories
   SET source_uri = json_extract(citations, '$[0].uri')
 WHERE source_uri IS NULL
   AND json_valid(citations) = 1
   AND json_array_length(citations) > 0
   AND json_extract(citations, '$[0].uri') IS NOT NULL
   AND length(json_extract(citations, '$[0].uri')) > 0;
