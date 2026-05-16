-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 QW-2 — Persona-as-artifact substrate primitive (Postgres v36).
--
-- Mirror of SQLite migration 0031_v07_persona.sql. See that file for the
-- full design rationale. The Postgres flavor uses `ADD COLUMN IF NOT
-- EXISTS` so the migration is idempotent on Postgres 14+; fresh installs
-- pick the columns up inline from `postgres_schema.sql`.

-- v0.7.0 WT-1 ship-readiness — backfill missing memory_kind column on
-- legacy postgres DBs. The base postgres_schema.sql carries this column
-- inline for fresh installs, but earlier postgres migrations omitted the
-- ADD COLUMN step. Idempotent — `IF NOT EXISTS` is a no-op on installs
-- that already have it (e.g., fresh schemas).
ALTER TABLE memories ADD COLUMN IF NOT EXISTS memory_kind TEXT NOT NULL DEFAULT 'observation';

ALTER TABLE memories ADD COLUMN IF NOT EXISTS entity_id TEXT;
ALTER TABLE memories ADD COLUMN IF NOT EXISTS persona_version INTEGER;

CREATE INDEX IF NOT EXISTS idx_personas_by_entity
    ON memories(entity_id, namespace)
    WHERE memory_kind = 'persona';
