-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 issue #860 — postgres v46 mirror of the SQLite Gap-7 surface.
--
-- # Background
--
-- The `memory_get_links` MCP tool's docstring promises that every link
-- row returned to the caller surfaces the four temporal-validity +
-- attestation columns:
--
--   * `valid_from   TIMESTAMPTZ NULL` — start of the link's validity window
--   * `valid_until  TIMESTAMPTZ NULL` — end of the link's validity window
--   * `observed_by  TEXT NULL`        — agent_id that minted the assertion
--   * `attest_level TEXT NULL`        — 'unsigned' | 'self_signed' | 'peer_attested'
--
-- All four columns are already in `postgres_schema.sql` (memory_links
-- declaration, lines 283-291) for greenfield deploys. This migration
-- exists as a defensive belt-and-braces for ANY pre-v0.7.0 Postgres
-- install whose `memory_links` table somehow missed one of these
-- columns during an interrupted earlier upgrade — `ADD COLUMN IF NOT
-- EXISTS` makes the migration safe to replay on a fully-conformant
-- table (it's a no-op when all four columns are present).
--
-- # Indexes
--
-- The composite `(source_id, valid_from, valid_until)` /
-- `(target_id, valid_from, valid_until)` indexes were already added
-- by `postgres_schema.sql:308-311` for greenfield deploys; this
-- migration re-asserts them via `CREATE INDEX IF NOT EXISTS` so a
-- legacy DB that was created before those declarations picks them up.
--
-- The `idx_memory_links_attest_level (attest_level, created_at)`
-- compound index supports the `memory_verify --by-attest-level` read
-- path that the H4 tool exposes.
--
-- # Idempotency
--
-- Pure additive: `ADD COLUMN IF NOT EXISTS` + `CREATE INDEX IF NOT
-- EXISTS` are Postgres-native replay-safe primitives. Re-running on a
-- fully-conformant table is a no-op.

ALTER TABLE memory_links
    ADD COLUMN IF NOT EXISTS valid_from   TIMESTAMPTZ;
ALTER TABLE memory_links
    ADD COLUMN IF NOT EXISTS valid_until  TIMESTAMPTZ;
ALTER TABLE memory_links
    ADD COLUMN IF NOT EXISTS observed_by  TEXT;
ALTER TABLE memory_links
    ADD COLUMN IF NOT EXISTS attest_level TEXT;

CREATE INDEX IF NOT EXISTS idx_links_temporal_src
    ON memory_links (source_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt
    ON memory_links (target_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_relation
    ON memory_links (relation, valid_from);
CREATE INDEX IF NOT EXISTS idx_memory_links_attest_level
    ON memory_links (attest_level, created_at);
