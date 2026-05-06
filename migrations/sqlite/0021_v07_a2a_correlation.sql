-- v0.7.0 — A2A correlation IDs, ACK semantics, retry, and a
-- dead-letter queue for failed webhook deliveries (Track K, Task K6 —
-- schema v27).
--
-- This migration lays the substrate for K6's reliability work on the
-- subscription dispatch path:
--
--   * `subscription_events` is the per-delivery audit log. Every
--     outgoing webhook payload is committed here BEFORE the network
--     send, so correlation_id round-trips and replay-from-cursor
--     queries (memory_subscription_replay, gated behind K7) have a
--     stable record to read from. The row is keyed on UUIDv7
--     correlation_id (time-ordered, unique) so cursor scans by
--     `created_at` and `correlation_id` agree on order.
--
--   * `subscription_dlq` holds the deliveries that exhausted the
--     three-attempt retry ladder (200ms / 1s / 5s exponential
--     backoff). The row carries the full payload + the last error so
--     a downstream operator (or the K7 inspector tool) can replay or
--     discard without reaching back into the live subscriber side.
--
-- Columns rationale:
--
--   * `subscription_events.correlation_id` — UUIDv7 string. NOT NULL
--     with DEFAULT '' so the ALTER TABLE on existing v26 deployments
--     succeeds (legacy rows pre-K6 carry the empty correlation_id;
--     the application path always populates a fresh UUIDv7 going
--     forward). The companion idx_subscription_events_correlation
--     index keeps replay-from-cursor lookups O(log n).
--
--   * `subscription_dlq.retry_count` — number of retries that were
--     attempted (NOT including the initial delivery). For the K6
--     ladder this is always 3 by the time a row lands in the DLQ;
--     captured explicitly so future tuning of the retry ladder
--     leaves the historical record intact.
--
--   * `subscription_dlq.last_error` — short error string from the
--     final failed attempt. Free-form text; the K7 inspector tool
--     surfaces this verbatim so operators can diagnose without
--     reaching for daemon logs.
--
--   * `first_failed_at` / `last_failed_at` — RFC3339 timestamps
--     bracketing the retry window. The pair lets DLQ analytics
--     compute ladder duration without a join.
--
-- Idempotency:
--
--   The ALTER TABLE adding `correlation_id` to `subscription_events`
--   is emitted from Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`);
--   this file holds only the idempotent CREATE TABLE / CREATE INDEX
--   statements. Re-applying the file is a no-op.

-- subscription_events: per-delivery audit log. Created here (v27)
-- because no prior K-track migration introduced it. K7's
-- memory_subscription_replay reads from this table; the
-- correlation_id index supports both cursor-by-id and cursor-by-time
-- scans.
CREATE TABLE IF NOT EXISTS subscription_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    subscription_id TEXT NOT NULL,
    correlation_id  TEXT NOT NULL DEFAULT '',
    event_type      TEXT NOT NULL,
    payload         TEXT NOT NULL,
    delivered_at    TEXT NOT NULL,
    delivery_status TEXT NOT NULL DEFAULT 'pending'
);

CREATE INDEX IF NOT EXISTS idx_subscription_events_correlation
    ON subscription_events(correlation_id);

CREATE INDEX IF NOT EXISTS idx_subscription_events_subscription
    ON subscription_events(subscription_id, delivered_at);

-- subscription_dlq: dead-letter queue for deliveries that exhausted
-- the three-attempt retry ladder. Inspecting / replaying / purging
-- DLQ rows is K7's surface; K6 only ships the writer.
CREATE TABLE IF NOT EXISTS subscription_dlq (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    subscription_id  TEXT NOT NULL,
    correlation_id   TEXT NOT NULL,
    event_type       TEXT NOT NULL,
    payload          TEXT NOT NULL,
    retry_count      INTEGER NOT NULL,
    last_error       TEXT NOT NULL,
    first_failed_at  TEXT NOT NULL,
    last_failed_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_subscription_dlq_subscription
    ON subscription_dlq(subscription_id, last_failed_at);

CREATE INDEX IF NOT EXISTS idx_subscription_dlq_correlation
    ON subscription_dlq(correlation_id);
