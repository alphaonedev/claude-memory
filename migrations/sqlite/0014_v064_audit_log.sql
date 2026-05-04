-- v0.6.4-009 — Capability-expansion audit log (schema v20).
--
-- Records every `memory_capabilities --include-schema family=<f>` call
-- so operators can audit which agents asked for which families and
-- whether the request was granted. Distinct from the SOC2/HIPAA
-- hash-chained audit trail in `audit/*.log` (file-based, tamper-
-- evident); this table is for runtime observability inside the SQLite
-- DB itself and is the simplest way to roll up per-agent expansion
-- patterns into a `memory_stats` overlay.
--
-- The table is created here so a fresh-DB bootstrap picks it up; the
-- `ALTER TABLE`-equivalent path (already-migrated v18/v19 DBs) is
-- handled idempotently in db.rs via `CREATE TABLE IF NOT EXISTS`.
-- Postgres dialect mirror: migrations/postgres/0014_v064_audit_log.sql

CREATE TABLE IF NOT EXISTS audit_log (
    id                 TEXT PRIMARY KEY,
    agent_id           TEXT,
    event_type         TEXT NOT NULL,    -- 'capability_expansion' for v0.6.4-009
    requested_family   TEXT,
    granted            INTEGER NOT NULL, -- 1 = allow, 0 = deny
    attestation_tier   TEXT,             -- reserved for v0.7 — null in v0.6.4
    timestamp          TEXT NOT NULL     -- RFC3339 (UTC)
);

CREATE INDEX IF NOT EXISTS idx_audit_log_agent_id
    ON audit_log (agent_id);

CREATE INDEX IF NOT EXISTS idx_audit_log_timestamp
    ON audit_log (timestamp);

CREATE INDEX IF NOT EXISTS idx_audit_log_event_type
    ON audit_log (event_type);
