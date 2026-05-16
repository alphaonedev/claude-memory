-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Form 4 — fact-provenance closeout (postgres v37).
--
-- Mirror of SQLite migration 0032_v07_form4_provenance.sql. See that
-- file for the full design rationale. The Postgres flavor uses
-- `ADD COLUMN IF NOT EXISTS` so the migration is idempotent on
-- Postgres 14+; fresh installs pick the columns up inline from
-- `postgres_schema.sql`.

ALTER TABLE memories ADD COLUMN IF NOT EXISTS citations TEXT NOT NULL DEFAULT '[]';
ALTER TABLE memories ADD COLUMN IF NOT EXISTS source_uri TEXT;
ALTER TABLE memories ADD COLUMN IF NOT EXISTS source_span TEXT;

CREATE INDEX IF NOT EXISTS idx_memories_source_uri
    ON memories(source_uri) WHERE source_uri IS NOT NULL;
