-- v0.7.0 — Outbound link signing (Track H, Task H2 — schema v23).
--
-- Adds the `attest_level` TEXT column to `memory_links` so callers can
-- distinguish three states without poking at the `signature` BLOB:
--
--   "unsigned"     — no active keypair on the writer; signature is NULL.
--   "self_signed"  — writer signed with its own Ed25519 keypair.
--   "peer_attested" — H3 will set this on inbound links whose signature
--                     verified against a known peer's public key.
--
-- The `signature` BLOB column itself shipped in v0.6.3 (schema v15)
-- but stayed dead until H2; this migration only adds the level tag.
--
-- Backward compat: existing rows are backfilled to `attest_level =
-- 'unsigned'` because they were written before any keypair plumbing
-- existed. Callers MUST treat NULL as `unsigned` defensively in case a
-- post-migration row writer fails to populate the column.
--
-- The ALTER TABLE itself runs from Rust (SQLite has no `ADD COLUMN
-- IF NOT EXISTS`); this file only carries the idempotent backfill
-- and index. The (attest_level, created_at) index supports the H4
-- `memory_verify` listing path planned next.

UPDATE memory_links
   SET attest_level = 'unsigned'
 WHERE attest_level IS NULL;

CREATE INDEX IF NOT EXISTS idx_memory_links_attest_level
    ON memory_links (attest_level, created_at);
