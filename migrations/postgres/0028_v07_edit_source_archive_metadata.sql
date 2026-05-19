-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Provenance Gap 5 (issue #888) — postgres v45 mirror of the
-- SQLite Gap-5 split-write surface (see `src/storage/mod.rs::
-- update_with_archive_on_supersede`).
--
-- # Background
--
-- Gap 5 introduces an "append-and-archive" write path: when the caller
-- supplies `edit_source IN ('llm', 'hook')`, the substrate archives
-- the OLD memory row with `archive_reason = 'superseded'` and inserts a
-- NEW row carrying the patched content. The supersede lineage is
-- recorded via TWO mechanisms (not three):
--   1. `archived_memories.archive_reason = 'superseded'` on the OLD row
--   2. `new_memory.metadata.superseded_id` forward pointer on the NEW row
-- A third `memory_links` row with `relation = 'supersedes'` is
-- intentionally NOT written because the FK `target_id REFERENCES
-- memories(id)` would reject it (the archived row has left the live
-- `memories` table). See issue #895 for the archive-cross-ref follow-on.
--
-- # What this migration does
--
-- On the SQLite path, `archived_memories.archive_reason` is a free-form
-- TEXT column — no CHECK constraint to expand. The same is true on
-- Postgres: `postgres_schema.sql:350` declares `archive_reason TEXT NOT
-- NULL DEFAULT 'ttl_expired'` with no CHECK clause. The `'superseded'`
-- value therefore lands without any DDL change to the column itself.
--
-- This migration nevertheless lays in two performance + audit
-- affordances the Gap-5 read paths rely on:
--
--   1. `idx_archived_reason` — a btree on
--      `(archive_reason, archived_at)` so the "show me everything
--      superseded in the last day" audit query is index-driven instead
--      of a sequential scan over the full archive table. The compound
--      `(reason, time)` shape pins the time-bounded prefix scan that
--      the audit CLI emits.
--
--   2. `idx_memories_metadata_superseded` — a partial GIN-or-btree
--      index on `metadata->>'superseded_id'` covering rows whose
--      metadata carries a forward pointer to an archived predecessor.
--      Lets "find the live row that superseded archived X" resolve
--      with an index lookup. Partial (WHERE metadata ?
--      'superseded_id') keeps the index small — only a tiny
--      fraction of live rows carry a superseded_id pointer.
--
-- # Idempotency
--
-- Both `CREATE INDEX IF NOT EXISTS` clauses are Postgres-native
-- replay-safe primitives. No ALTER TABLE here, so no need for
-- per-step pg_constraint probes.

-- (1) Compound (reason, time) index for archive audits.
CREATE INDEX IF NOT EXISTS idx_archived_reason
    ON archived_memories(archive_reason, archived_at DESC);

-- (2) Partial expression index on the forward-pointer JSON path.
-- The `(metadata->>'superseded_id')` expression is IMMUTABLE-equivalent
-- for index purposes since `metadata` is a JSONB column with a stable
-- text-extract operator. Postgres declines to create an index on a
-- nullable expression unless the WHERE clause narrows; the partial
-- predicate keeps the index small and serves the exact lookup pattern
-- Gap-5 audits use.
CREATE INDEX IF NOT EXISTS idx_memories_metadata_superseded
    ON memories((metadata->>'superseded_id'))
    WHERE metadata ? 'superseded_id';
