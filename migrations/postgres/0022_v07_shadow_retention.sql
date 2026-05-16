-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Cluster G — shadow-mode retention + denormalised source column
-- + compound index supporting the calibration scan (postgres v40).
-- Mirror of SQLite migration 0035_v07_shadow_retention.sql. See that
-- file for full design rationale. Postgres supports `ADD COLUMN IF NOT
-- EXISTS` so the ALTER + backfill + index can all live in this SQL file
-- as one idempotent batch.
--
-- Backfill: legacy rows (written under postgres v39) backfill `source`
-- from the joined `memories` row; rows whose source memory has been
-- deleted land with `source = 'unknown'` (defense in depth; the v39
-- CASCADE FK should have already removed them).

ALTER TABLE confidence_shadow_observations
    ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'unknown';

UPDATE confidence_shadow_observations o
SET source = COALESCE(m.source, 'unknown')
FROM memories m
WHERE m.id = o.memory_id
  AND o.source = 'unknown';

CREATE INDEX IF NOT EXISTS idx_shadow_obs_namespace_source_observed
    ON confidence_shadow_observations(namespace, source, observed_at);
