-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Form 5 — auto-confidence + shadow-mode + calibration tooling
-- (postgres v38). Mirror of SQLite migration
-- 0033_v07_form5_confidence_calibration.sql. See that file for the
-- full design rationale. The Postgres flavor uses
-- `ADD COLUMN IF NOT EXISTS` so the migration is idempotent on
-- Postgres 14+; fresh installs pick the columns up inline from
-- `postgres_schema.sql`.

ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS confidence_source TEXT NOT NULL DEFAULT 'caller_provided';
ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS confidence_signals TEXT;
ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS confidence_decayed_at TEXT;

CREATE TABLE IF NOT EXISTS confidence_shadow_observations (
    id BIGSERIAL PRIMARY KEY,
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    namespace TEXT NOT NULL,
    caller_confidence DOUBLE PRECISION NOT NULL,
    derived_confidence DOUBLE PRECISION NOT NULL,
    signals TEXT NOT NULL,
    recall_outcome TEXT,
    observed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_shadow_obs_namespace
    ON confidence_shadow_observations(namespace);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_observed_at
    ON confidence_shadow_observations(observed_at);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_memory
    ON confidence_shadow_observations(memory_id);

CREATE INDEX IF NOT EXISTS idx_memories_confidence_source
    ON memories(confidence_source)
    WHERE confidence_source != 'caller_provided';
