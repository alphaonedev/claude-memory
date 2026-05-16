-- v0.7.0 Cluster-C SEC-3 closeout (issue #767) — `signed_events_dlq`
-- dead-letter queue for the deferred-audit drainer (Postgres
-- counterpart to SQLite schema v40 / 0034_v07_signed_events_dlq.sql).
--
-- See the SQLite migration for the design rationale (failure-split,
-- table-vs-JSONL trade-off, append-only invariant carve-out). This
-- file ships the Postgres-native column types (`BYTEA` for blobs,
-- `BIGSERIAL` for the synthetic primary key, `TEXT` for everything
-- else — same as `signed_events` itself).

CREATE TABLE IF NOT EXISTS signed_events_dlq (
    dlq_id          BIGSERIAL PRIMARY KEY,
    id              TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    payload_hash    BYTEA NOT NULL,
    signature       BYTEA,
    attest_level    TEXT NOT NULL DEFAULT 'unsigned',
    timestamp       TEXT NOT NULL,
    failure_reason  TEXT NOT NULL,
    failed_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_failed_at
    ON signed_events_dlq(failed_at);
CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_agent
    ON signed_events_dlq(agent_id);
