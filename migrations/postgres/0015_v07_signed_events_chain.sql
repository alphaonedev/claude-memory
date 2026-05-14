-- v0.7.0 V-4 closeout (#698) — add SQL-side cross-row hash chain to
-- `signed_events` on the Postgres backend. Mirrors SQLite schema v34
-- (0028_v07_signed_events_chain.sql).
--
-- Postgres supports `ADD COLUMN IF NOT EXISTS` so the migration is a
-- single DDL batch — fresh installs inherit the columns inline from
-- `postgres_schema.sql`; pre-v33 deployments pick them up here.
-- Postgres-native `BYTEA` mirrors SQLite's `BLOB` for the prev_hash
-- column and `BIGINT` mirrors `INTEGER` for the sequence column
-- (rowid is 64-bit in SQLite; Postgres `BIGINT` is the explicit-
-- width parity).
--
-- The backfill loop runs in `PostgresStore::migrate_v33` (Rust)
-- because the row-by-row prev_hash computation needs the application-
-- layer canonical-bytes encoding from `signed_events::
-- canonical_chain_bytes` — pure SQL cannot reproduce that across
-- both backends.

ALTER TABLE signed_events ADD COLUMN IF NOT EXISTS prev_hash BYTEA;
ALTER TABLE signed_events ADD COLUMN IF NOT EXISTS sequence  BIGINT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_signed_events_sequence
    ON signed_events(sequence);
