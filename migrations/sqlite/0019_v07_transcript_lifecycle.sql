-- v0.7.0 — Per-namespace TTL with archive→prune lifecycle (schema v25).
--
-- I3 of the attested-cortex epic. Builds on the I1 substrate
-- (`memory_transcripts`, schema v22) and the I2 join table
-- (`memory_transcript_links`, schema v24).
--
-- Two-phase lifecycle:
--   Phase 1 — ARCHIVE: a transcript whose age exceeds the resolved
--             default_ttl AND whose linked memories are all expired
--             (or absent) is marked archived by setting `archived_at`
--             to the current RFC3339 timestamp. The blob stays put
--             so a late-binding `memory_replay` (I4) call can still
--             reach it during the grace window.
--   Phase 2 — PRUNE:  an archived transcript whose
--             archived_at + archive_grace_secs has passed is DELETEd.
--             The I2 join table is cleaned up automatically by the
--             `ON DELETE CASCADE` declared on
--             memory_transcript_links.transcript_id.
--
-- The `archived_at` column is the load-bearing addition for I3; the
-- supporting partial index keeps the prune-phase scan O(archived rows)
-- rather than O(total transcripts) on busy namespaces.
--
-- ALTER TABLE … ADD COLUMN is emitted from Rust (SQLite has no
-- `ADD COLUMN IF NOT EXISTS`); this file only carries the idempotent
-- index DDL.

CREATE INDEX IF NOT EXISTS idx_memory_transcripts_archived_at
    ON memory_transcripts (archived_at)
    WHERE archived_at IS NOT NULL;
