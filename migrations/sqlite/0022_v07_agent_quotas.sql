-- v0.7.0 — per-agent rate limits + storage caps (Track K, Task K8 —
-- schema v28).
--
-- Substrate for the K8 governance work: every registered agent gets a
-- single quota row tracking three rolling-window counters (memories
-- written today, storage bytes consumed lifetime, links written today)
-- against three limits (max_memories_per_day, max_storage_bytes,
-- max_links_per_day). The `store_memory` + `memory_link` write paths
-- consult the row before committing; on exceeded limit the call returns
-- a `QUOTA_EXCEEDED` diagnostic naming the limit that was hit.
--
-- Daily counters reset at UTC midnight via the K8 sweep loop wired into
-- `daemon_runtime::bootstrap_serve` — same lifecycle shape as the K2
-- pending-actions sweeper and the I3 transcript-lifecycle sweeper.
--
-- Columns rationale:
--
--   * `agent_id` PRIMARY KEY — one row per agent. The agent_id is the
--     same NHI marker resolved by `crate::identity::resolve_agent_id`
--     (see CLAUDE.md §"Agent Identity"). Default rows are auto-inserted
--     on first quota check rather than at agent registration time so the
--     substrate works against existing pre-v28 deployments without a
--     backfill.
--
--   * `max_memories_per_day` / `max_storage_bytes` / `max_links_per_day`
--     — operator-tunable hard limits. Compiled defaults: 1000 / 100MiB /
--     5000. Deliberately generous so the K8 substrate is invisible to
--     small-scale operations; tuning down is a per-deployment choice.
--
--   * `current_*_today` — running counters. The two `*_today` counters
--     reset to 0 at UTC midnight; `current_storage_bytes` is lifetime
--     (the storage cap is total persisted bytes, not per-day).
--
--   * `day_started_at` — RFC3339 timestamp of the start-of-day for the
--     `*_today` counter window. The sweeper compares against the
--     current UTC date and zeroes the `*_today` columns when the day
--     rolls over. Storing the boundary explicitly (rather than deriving
--     it from `updated_at`) keeps the reset idempotent under partial
--     write failure.
--
--   * `created_at` / `updated_at` — RFC3339 lifecycle timestamps.
--
-- Idempotency: pure CREATE TABLE IF NOT EXISTS + CREATE INDEX
-- statements. Re-applying the file is a no-op.

CREATE TABLE IF NOT EXISTS agent_quotas (
    agent_id                TEXT PRIMARY KEY,
    max_memories_per_day    INTEGER NOT NULL DEFAULT 1000,
    max_storage_bytes       INTEGER NOT NULL DEFAULT 104857600,
    max_links_per_day       INTEGER NOT NULL DEFAULT 5000,
    current_memories_today  INTEGER NOT NULL DEFAULT 0,
    current_storage_bytes   INTEGER NOT NULL DEFAULT 0,
    current_links_today     INTEGER NOT NULL DEFAULT 0,
    day_started_at          TEXT NOT NULL,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL
);

-- agent_id is already the PRIMARY KEY (and thus indexed), but an
-- explicit index keeps the K8 status-tool query plan stable across
-- SQLite versions that treat PK indexes differently in EXPLAIN output.
CREATE INDEX IF NOT EXISTS idx_agent_quotas_agent_id
    ON agent_quotas(agent_id);
