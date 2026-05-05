-- v0.7.0 — Attested-cortex transcript links join table (schema v24).
--
-- I2 of the I-track. Establishes the m:n relationship between
-- `memories` (stored insights) and `memory_transcripts` (compressed
-- conversation source material from I1, schema v22). One memory can be
-- derived from a single transcript span (or several), and one transcript
-- can be the source for many memories. The optional `span_start` /
-- `span_end` byte offsets address a sub-region of the decompressed
-- transcript so I4's `memory_replay` can return the precise excerpt.
--
-- Substrate only — no MCP tool wiring lands here. Subsequent tasks layer
-- on:
--   I3 — archive->prune lifecycle for transcripts (rows in this table
--        are removed transitively via ON DELETE CASCADE).
--   I4 — memory_replay MCP tool that joins memories <-> transcripts via
--        this table to return the source span.
--   I5/R5 — pre_store hook that populates this table at extraction time.
--
-- Notes:
--   * PRIMARY KEY (memory_id, transcript_id) — a memory can only be
--     linked to a given transcript once. If callers need multiple spans
--     from the same transcript, they should concatenate / merge into a
--     single (start, end) pair upstream. Keeping the PK narrow keeps
--     the join cardinality bounded for I4's replay path.
--   * BOTH foreign keys are ON DELETE CASCADE: deleting a memory wipes
--     its provenance edges, and pruning a transcript (I3) wipes the
--     dangling links so `transcripts_for_memory` never returns ids that
--     can no longer be fetched.
--   * Two separate indexes cover the two access patterns: lookup by
--     memory (provenance fan-out) and lookup by transcript (replay
--     fan-in). The composite PK already covers (memory_id, *) so the
--     `idx_mtl_memory` single-column index is technically redundant
--     with the PK prefix — kept explicit for clarity and to make the
--     `PRAGMA index_list` test stable across SQLite versions that may
--     not expose the auto-PK index by name.

CREATE TABLE IF NOT EXISTS memory_transcript_links (
    memory_id     TEXT NOT NULL,
    transcript_id TEXT NOT NULL,
    span_start    INTEGER,
    span_end      INTEGER,
    PRIMARY KEY (memory_id, transcript_id),
    FOREIGN KEY (memory_id)     REFERENCES memories(id)           ON DELETE CASCADE,
    FOREIGN KEY (transcript_id) REFERENCES memory_transcripts(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_mtl_transcript ON memory_transcript_links(transcript_id);
CREATE INDEX IF NOT EXISTS idx_mtl_memory     ON memory_transcript_links(memory_id);
