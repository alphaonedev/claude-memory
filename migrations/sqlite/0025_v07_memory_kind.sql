-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 L1-1 — typed MemoryKind::Reflection enum (schema v30).
--
-- Backfill: rows whose metadata.type = 'reflection' (the pre-v30
-- back-compat marker written by db::reflect) are promoted to
-- memory_kind = 'reflection'. All other rows keep the SQL-DEFAULT
-- value 'observation' (already written by the ALTER TABLE step in
-- migrations.rs).  The CASE guards json_valid() so corrupt-metadata
-- rows are treated as 'observation' rather than crashing the backfill.
--
-- The ALTER TABLE itself is emitted from Rust (SQLite has no
-- ADD COLUMN IF NOT EXISTS); this file holds only the idempotent
-- backfill UPDATE and the supporting index.

UPDATE memories
   SET memory_kind = 'reflection'
 WHERE memory_kind = 'observation'
   AND json_valid(metadata)
   AND json_extract(metadata, '$.type') = 'reflection';

CREATE INDEX IF NOT EXISTS idx_memories_memory_kind ON memories(memory_kind);
