-- v0.7.0 K2 — pending_actions timeout sweeper.
--
-- Adds two columns to back the v0.7.0 K2 background sweeper that
-- transitions stale pending_actions rows to status='expired':
--
--   default_timeout_seconds  per-row TTL (NULL → use cluster default)
--   expired_at               RFC3339 timestamp set when the sweeper fires
--
-- The ALTER TABLE statements live in db.rs because SQLite has no
-- `ADD COLUMN IF NOT EXISTS`; this file holds the idempotent indexes.

-- Index speeds the hot path:
--   SELECT id FROM pending_actions
--   WHERE status='pending'
--     AND (julianday('now') - julianday(requested_at)) * 86400
--         > COALESCE(default_timeout_seconds, ?global_default)
-- A composite (status, requested_at) index keeps the planner from
-- scanning the whole table on every 60-second tick.
CREATE INDEX IF NOT EXISTS idx_pending_status_requested
    ON pending_actions (status, requested_at);
