-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Form 5 — auto-confidence + shadow-mode + calibration tooling
-- (schema v39, sqlite). Issue #758.
--
-- The Batman 6-form audit (PR #753) found Form 5 PARTIAL: the
-- `memories.confidence` REAL column had existed since schema v2 and
-- recall ranking consumed it (`+ confidence * 2.0` in the FTS5 score
-- expression at `src/storage/mod.rs:1174`), but
--
--   * No automatic confidence assignment (every caller value was taken
--     at face — no source-age decay, no atom-derivation bump, no
--     prior-corroboration boost).
--   * No shadow-mode telemetry (no way to compare caller-provided vs.
--     derived confidence on a live workload before flipping the
--     auto-derive switch).
--   * No calibration mechanism (no per-namespace / per-source-role
--     baseline computed from observed shadow-mode samples).
--   * No freshness-decay model (an old fact at confidence=0.9 was
--     ranked identically to a fresh fact at confidence=0.9, despite
--     human memory and downstream LLM reasoning both treating
--     recency as a confidence signal).
--
-- This migration is the schema half of the closeout. The Rust-side
-- engine lives in `src/confidence/` (`derive`, `shadow`, `decay`).
--
-- # Columns added on `memories`
--
-- - `confidence_source TEXT NOT NULL DEFAULT 'caller_provided'` — typed
--   discriminator for the provenance of the `confidence` value. One of
--   `caller_provided` (legacy / default), `auto_derived` (computed by
--   the v0.7 Form 5 engine from row signals), `calibrated` (replaced by
--   the calibration sweep using per-source baselines), `decayed` (the
--   freshness-decay updater overwrote the live value with a decayed
--   one). Lets the recall ranker and forensic bundle reason about the
--   confidence trust path without re-running the derivation.
--
-- - `confidence_signals TEXT` — JSON snapshot of the
--   `ConfidenceSignals` struct (`source_age_days`, `atom_derivation`,
--   `prior_corroboration_count`, `freshness_factor`,
--   `baseline_per_source`) emitted at the moment the value was
--   computed. NULL on legacy rows and on rows whose `confidence_source
--   = 'caller_provided'`. Lets an auditor reconstruct the derivation
--   after the fact without re-querying the substrate at the
--   then-current state.
--
-- - `confidence_decayed_at TEXT` — RFC3339 timestamp of the last decay
--   computation. NULL on legacy rows and rows that have never been
--   touched by the decay updater. Bumped by the recall path when
--   `AI_MEMORY_CONFIDENCE_DECAY=1` or the namespace policy carries
--   `confidence_decay_half_life_days`.
--
-- # `confidence_shadow_observations` table
--
-- Per-recall shadow-mode telemetry. Populated when
-- `AI_MEMORY_CONFIDENCE_SHADOW=1` and sampled at
-- `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE`. Each row carries the
-- caller's confidence (the value the system used downstream — shadow
-- mode never silently overrides), the derived confidence the Form 5
-- engine would have assigned, a JSON snapshot of the signals that
-- produced the derivation, and an optional `recall_outcome` discriminator
-- (`recalled` | `skipped` | NULL) the recall ranker stamps once the
-- candidate is either returned in the top-K or dropped. The downstream
-- calibration CLI reads this table to compute per-namespace +
-- per-source-role baselines.
--
-- # Idempotency
--
-- The Rust migrate ladder probes `PRAGMA table_info(memories)` for each
-- new column before emitting the ALTERs (SQLite has no
-- `ADD COLUMN IF NOT EXISTS`). This SQL file holds the
-- `confidence_shadow_observations` table + indexes plus the supporting
-- partial index on `confidence_source` so the recall ranker can
-- filter "rows with calibrated/decayed confidence" without a full
-- table scan.

CREATE TABLE IF NOT EXISTS confidence_shadow_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    caller_confidence REAL NOT NULL,
    derived_confidence REAL NOT NULL,
    signals TEXT NOT NULL,
    recall_outcome TEXT,
    observed_at TEXT NOT NULL,
    FOREIGN KEY(memory_id) REFERENCES memories(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_shadow_obs_namespace
    ON confidence_shadow_observations(namespace);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_observed_at
    ON confidence_shadow_observations(observed_at);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_memory
    ON confidence_shadow_observations(memory_id);

-- Partial index on `confidence_source` covering rows that are NOT in
-- the (overwhelming-majority) `caller_provided` bucket. The
-- calibration CLI scans this slice to enumerate derived/calibrated/
-- decayed rows; legacy and caller-provided rows are excluded so the
-- index footprint on legacy DBs stays at zero.
CREATE INDEX IF NOT EXISTS idx_memories_confidence_source
    ON memories(confidence_source)
    WHERE confidence_source != 'caller_provided';
