-- v0.7.0 Cluster-C SEC-3 closeout (issue #767) — `signed_events_dlq`
-- dead-letter queue for the deferred-audit drainer.
--
-- # Why a DLQ at all
--
-- The deferred-audit drainer (`src/governance/deferred_audit.rs`) is
-- the only path that promotes storage-hook `governance.refusal`
-- events into the `signed_events` cryptographic chain. Pre-Cluster-C
-- the drainer dropped failed appends silently: a SQLITE_BUSY,
-- SQLITE_CONSTRAINT_UNIQUE (chain-head race), or any other rusqlite
-- error logged at `tracing::error!`, incremented the metric counter,
-- and proceeded to the next event. The audit chain ROW for that
-- refusal was permanently lost — the exact failure mode the
-- chain-log primitive exists to prevent.
--
-- The Cluster-C fix splits drainer failures into two buckets:
--
--   * `SQLITE_CONSTRAINT_UNIQUE` on the `idx_signed_events_sequence`
--     UNIQUE index: requeue. This is a race-only failure (two
--     concurrent writers raced to grab the same `sequence` value)
--     and the BEGIN IMMEDIATE wrap in `append_signed_event` now
--     serialises retries against the wal-write lock. The event
--     lands on the next drainer iteration.
--
--   * Everything else (disk full, schema corruption, FK violation,
--     malformed event): land in `signed_events_dlq` so an operator
--     can manually replay or post-mortem the loss. The DLQ row
--     mirrors the would-be `signed_events` row schema PLUS a
--     `failure_reason` column carrying the rusqlite error string at
--     time of DLQ landing.
--
-- # Why a SQL table (not a JSONL file)
--
-- - The DLQ is queryable from the existing `signed_events`-aware
--   tooling (capabilities `dlq.size`, future `ai-memory audit
--   dlq list`).
-- - The drainer already owns a SQLite Connection; landing a row in
--   the same DB is one INSERT, no extra I/O surface.
-- - JSONL would couple the DLQ to the audit-log path's append-only
--   chflags semantics — which is the wrong primitive (the DLQ is
--   recoverable / mutable; the audit log is not).
-- - A future `ai-memory audit dlq replay <row_id>` substrate command
--   can re-attempt the chain-log insert with one SQL DELETE +
--   `append_signed_event` from the row's captured columns.
--
-- # Columns
--
-- Mirrors `signed_events` 1:1 for the would-be-chain-log fields
-- (`id` / `agent_id` / `event_type` / `payload_hash` / `signature` /
-- `attest_level` / `timestamp`) so a future "replay one DLQ row"
-- tool can reconstruct the original `SignedEvent` losslessly. Adds:
--
-- * `dlq_id`         — INTEGER PRIMARY KEY AUTOINCREMENT. Distinct
--                      from `id` because two failed attempts to
--                      append the SAME logical event (rare but
--                      possible if the supervisor restarts mid-DLQ)
--                      must not collide on the UUIDv4.
-- * `failure_reason` — TEXT, rusqlite error string at time of DLQ
--                      landing. Operator-readable; not parsed.
-- * `failed_at`      — TEXT, RFC3339 UTC instant the DLQ row was
--                      written. Distinct from `timestamp` (which is
--                      the original event time) so the auditor can
--                      tell "event was at T0, failed to chain-log at
--                      T1".
--
-- # Append-only invariant — NOT enforced for the DLQ
--
-- Unlike `signed_events`, the DLQ IS mutable: an operator replaying
-- a DLQ row DELETEs it after a successful re-append. There is no
-- chain-hash on DLQ rows; the integrity property the DLQ provides
-- is "no audit drop is silent", not "DLQ rows are tamper-evident".

CREATE TABLE IF NOT EXISTS signed_events_dlq (
    dlq_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    id              TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    payload_hash    BLOB NOT NULL,
    signature       BLOB,
    attest_level    TEXT NOT NULL DEFAULT 'unsigned',
    timestamp       TEXT NOT NULL,
    failure_reason  TEXT NOT NULL,
    failed_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_failed_at
    ON signed_events_dlq(failed_at);
CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_agent
    ON signed_events_dlq(agent_id);
