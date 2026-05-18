-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 Gap 3 (#886) — recall-consumption observation tier (schema v47, sqlite).
--
-- The Batman 6-form audit closeout flagged a missing "what did the
-- caller actually use after we ranked it" feedback channel: ranking
-- telemetry stops at the recall response, so the substrate cannot tell
-- which candidates the caller subsequently cited in a memory_store or
-- memory_link payload. Issue #886 tracks the closeout; this is the
-- substrate half.
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

CREATE TABLE IF NOT EXISTS recall_observations (
    recall_id              TEXT    NOT NULL,
    memory_id              TEXT    NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    retriever              TEXT    NOT NULL,
    rank                   INTEGER NOT NULL,
    score                  REAL    NOT NULL,
    consumed               INTEGER NOT NULL DEFAULT 0,
    observed_at            TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    consumed_at            TEXT    NULL,
    consumed_by_memory_id  TEXT    NULL REFERENCES memories(id) ON DELETE CASCADE,
    PRIMARY KEY (recall_id, memory_id)
);

CREATE INDEX IF NOT EXISTS idx_recall_observations_recall_id
    ON recall_observations(recall_id);
CREATE INDEX IF NOT EXISTS idx_recall_observations_memory_id
    ON recall_observations(memory_id);
CREATE INDEX IF NOT EXISTS idx_recall_observations_observed_at
    ON recall_observations(observed_at);
