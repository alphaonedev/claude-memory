-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Provenance Gap 1 (issue #884) — postgres v42 mirror of SQLite
-- schema v45 (`migrations/sqlite/0039_v07_provenance_version.sql`).
--
-- Adds `memories.version BIGINT NOT NULL DEFAULT 1` so every memory
-- row carries an optimistic-concurrency counter. The `version` column
-- is bumped on every mutation by `PostgresStore::update_with_expected_version`;
-- concurrent updates pass `expected_version` (via MCP `memory_update`
-- param or HTTP `If-Match: <version>` header) and receive a typed
-- `CONFLICT` envelope when the stored version has drifted.
--
-- # Idempotency
--
-- Postgres 14+ supports `ADD COLUMN IF NOT EXISTS`, so the migration is
-- replay-safe. Pure additive: every pre-v42 row inherits `version = 1`
-- via the SQL DEFAULT clause; subsequent updates monotonically bump
-- from there. Fresh installs pick the column up inline via the v42
-- bootstrap that runs after this migration ladder lands (see
-- `postgres_schema.sql` for the canonical column declaration).
--
-- See `tests/store_parity_gaps.rs::verify_gap_1_version` for the
-- regression-pin: two concurrent updates against the same memory must
-- produce exactly one winner; the loser receives a typed
-- `VersionConflict` envelope naming the current stored version.

ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1;
