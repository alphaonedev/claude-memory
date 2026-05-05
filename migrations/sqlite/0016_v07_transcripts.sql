-- v0.7.0 — Attested-cortex transcripts (schema v21).
--
-- Substrate for the I-track tasks (I1-I5 + R5): compressed storage of
-- raw conversation transcripts so memories can later be re-grounded
-- against the verbatim source. The blob is zstd-3 compressed text;
-- chat-shaped corpora typically achieve >=5x compression at level 3.
--
-- I1 (this migration) ships the table + sweep index ONLY. Subsequent
-- tasks layer on:
--   I2 — memory_transcript_links join table (memory_id <-> transcript_id)
--   I3 — per-namespace TTL with archive->prune lifecycle
--   I4 — memory_replay MCP tool (reads via fetch())
--   I5/R5 — pre_store extraction hook
--
-- Notes:
--   * `expires_at` is RFC3339 in UTC, mirroring the convention used by
--     `memories.expires_at`. NULL means no TTL.
--   * `compressed_size` / `original_size` are recorded at insert time so
--     `memory_stats` overlays (planned in I-track follow-ups) can compute
--     ratios without decompressing every blob.
--   * `zstd_level` defaults to 3 — keeping it as a column lets later
--     migrations bump the default without losing the original encoding
--     parameter for legacy rows.
--   * The (namespace, created_at) index is the access pattern for
--     archive sweeps (I3) and per-namespace listings (I4).

CREATE TABLE IF NOT EXISTS memory_transcripts (
    id               TEXT PRIMARY KEY,
    namespace        TEXT NOT NULL,
    created_at       TEXT NOT NULL,    -- RFC3339 (UTC)
    expires_at       TEXT,             -- RFC3339 (UTC); NULL = no TTL
    compressed_size  INTEGER NOT NULL,
    original_size    INTEGER NOT NULL,
    zstd_level       INTEGER NOT NULL DEFAULT 3,
    content_blob     BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_transcripts_namespace_created
    ON memory_transcripts (namespace, created_at);
