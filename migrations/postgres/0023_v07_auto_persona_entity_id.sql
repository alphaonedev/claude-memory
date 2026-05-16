-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 polish PERF-8 (issue #781) — auto-persona indexed entity-id
-- column replacing the content LIKE '%entity_X%' full-table scan
-- (postgres v41). Mirror of SQLite migration
-- 0036_v07_auto_persona_entity_id.sql; see that file for the design
-- rationale.
--
-- Postgres supports `ADD COLUMN IF NOT EXISTS` (14+) so the ALTER +
-- backfill + partial index can all live in this SQL file as one
-- idempotent batch.
--
-- Backfill: legacy reflection rows pick up `mentioned_entity_id` from
-- `metadata->>'entity_id'` (the structured tag) or from a `[entity:X]`
-- marker in the title. Rows whose reflection neither carried a
-- structured tag nor a marker stay at NULL.
--
-- # Schema parity vs SQLite
--
-- The auto-persona substrate executor (`PersonaGenerator`,
-- `hooks::post_reflect::auto_persona`) is SQLite-only — it threads a
-- `rusqlite::Connection` and runs in a spawned worker. The matcher
-- rewrite in PERF-8 therefore touches the SQLite query path only. The
-- Postgres column lands here to keep schema parity between the two
-- backends so a future PG-native auto-persona implementation can read
-- from the same indexed column without a second migration bump.

ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS mentioned_entity_id TEXT;

-- Backfill from structured metadata tag first; fall back to the
-- `[entity:X]` title marker (substring_index manual extraction).
UPDATE memories
SET mentioned_entity_id = metadata->>'entity_id'
WHERE memory_kind = 'reflection'
  AND mentioned_entity_id IS NULL
  AND metadata ? 'entity_id'
  AND length(metadata->>'entity_id') > 0;

-- Title-marker fallback: extract `[entity:X]` -> X. Uses
-- substring(... from ...) with a posix regex; rows without a marker
-- match nothing and stay at NULL.
UPDATE memories
SET mentioned_entity_id = substring(title from '\[entity:([^\]]+)\]')
WHERE memory_kind = 'reflection'
  AND mentioned_entity_id IS NULL
  AND title ~ '\[entity:[^\]]+\]';

-- Partial predicate scoped to reflection rows to mirror the SQLite
-- migration's index shape and keep matcher query plans deterministic
-- across both backends.
CREATE INDEX IF NOT EXISTS idx_memories_mentioned_entity
    ON memories(mentioned_entity_id, namespace)
    WHERE memory_kind = 'reflection';
