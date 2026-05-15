-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 QW-2 — Persona-as-artifact substrate primitive (Postgres v36).
--
-- Mirror of SQLite migration 0031_v07_persona.sql. See that file for the
-- full design rationale. The Postgres flavor uses `ADD COLUMN IF NOT
-- EXISTS` so the migration is idempotent on Postgres 14+; fresh installs
-- pick the columns up inline from `postgres_schema.sql`.

ALTER TABLE memories ADD COLUMN IF NOT EXISTS entity_id TEXT;
ALTER TABLE memories ADD COLUMN IF NOT EXISTS persona_version INTEGER;

CREATE INDEX IF NOT EXISTS idx_personas_by_entity
    ON memories(entity_id, namespace)
    WHERE memory_kind = 'persona';
