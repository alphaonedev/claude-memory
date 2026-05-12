-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Task 1/8 (recursive learning) — Postgres schema v31.
--
-- Adds `memories.reflection_depth INTEGER NOT NULL DEFAULT 0`, the
-- column that tracks each memory's depth in the substrate-native
-- reflection recursion tree. `0` for caller-minted (or pre-v0.7.0)
-- rows; positive for memories synthesised by the reflection pass over
-- lower-depth peers.
--
-- Mirrors the SQLite v29 migration in `src/db.rs`. Idempotent via
-- `ADD COLUMN IF NOT EXISTS` (Postgres 15+ syntax; supported on the
-- adapter's published target — 14+ with extension polyfill not
-- required for column-level IF NOT EXISTS, which landed in 9.6 for
-- ADD COLUMN). Fresh-init schemas in `postgres_schema.sql` carry the
-- column inline so this script is a no-op for new clusters.

ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS reflection_depth INTEGER NOT NULL DEFAULT 0;
