-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Gap 3 (issue #886) — postgres v44 mirror of SQLite schema v47
-- (`migrations/sqlite/0038_v07_recall_observations.sql`).
--
-- The Batman 6-form audit closeout flagged a missing "what did the
-- caller actually use after we ranked it" feedback channel: ranking
-- telemetry stops at the recall response, so the substrate cannot tell
-- which candidates the caller subsequently cited in a memory_store or
-- memory_link payload. Issue #886 tracks the closeout; this is the
-- postgres half — full table + indexes + retention sweep helper.
--
-- # Table
--
-- `recall_observations` is an append-only ledger keyed by a recall_id
-- (returned in the memory_recall response) plus a memory_id. One row
-- per (recall_id, memory_id) pair carries the candidate's retriever
-- ('fts5' | 'hnsw' | 'hybrid'), in-result rank, score, observation
-- timestamp, a `consumed` boolean defaulting to FALSE, and (once the
-- caller cites the candidate in a downstream store/link request) the
-- consumed_at timestamp and the consumed_by_memory_id FK that did the
-- citing.
--
-- Schema shape mirrors SQLite v47 with two Postgres-native swaps:
--   * `consumed` is BOOLEAN (vs SQLite INTEGER 0/1) — both adapters
--     surface a Rust-side `bool`.
--   * `observed_at` / `consumed_at` are TIMESTAMPTZ (vs SQLite TEXT
--     RFC3339) — both adapters surface a Rust-side RFC3339 string.
--
-- # FKs / cascade
--
-- `memory_id` and `consumed_by_memory_id` both FK-reference
-- `memories(id)` with `ON DELETE CASCADE`. The observation ledger is
-- subordinate to the underlying memory row's lifecycle — when an
-- operator deletes a memory, the ledger entries that referenced it
-- evaporate alongside the row.
--
-- # Indexes
--
-- The recall_id, memory_id, and observed_at indexes cover the three
-- predicates the `memory_recall_observations` read-side tool exposes:
-- "show me everything we observed under recall X", "show me every
-- citation of memory M", and the TTL-gated time-window prune.
--
-- # Idempotency
--
-- Pure additive: `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT
-- EXISTS` are Postgres-native replay-safe primitives. Re-running this
-- migration on an already-populated DB is a no-op.

CREATE TABLE IF NOT EXISTS recall_observations (
    recall_id              TEXT        NOT NULL,
    memory_id              TEXT        NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    retriever              TEXT        NOT NULL,
    rank                   BIGINT      NOT NULL,
    score                  DOUBLE PRECISION NOT NULL,
    consumed               BOOLEAN     NOT NULL DEFAULT FALSE,
    observed_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    consumed_at            TIMESTAMPTZ NULL,
    consumed_by_memory_id  TEXT        NULL REFERENCES memories(id) ON DELETE CASCADE,
    PRIMARY KEY (recall_id, memory_id)
);

CREATE INDEX IF NOT EXISTS idx_recall_observations_recall_id
    ON recall_observations(recall_id);
CREATE INDEX IF NOT EXISTS idx_recall_observations_memory_id
    ON recall_observations(memory_id);
CREATE INDEX IF NOT EXISTS idx_recall_observations_observed_at
    ON recall_observations(observed_at);
