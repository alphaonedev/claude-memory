-- v0.6.3.1 Phase P2 — Data-integrity hardening (schema v17).
-- Closes audit findings G4 (mixed embedding dims silently tolerated),
-- G5 (archive lossy + restore resets tier/expires_at), and G13 (f32
-- endianness — magic byte header on embeddings). G6 (silent merge on
-- UNIQUE(title,namespace) conflict) is closed at the handler layer
-- behind capability negotiation; no schema change is required for it.
--
-- NOTE: ALTER TABLE ADD COLUMN statements are omitted here and performed
-- at the Rust layer (db.rs::migrate) with column-existence checks, since
-- SQLite cannot use IF NOT EXISTS for column additions. This file holds
-- the index DDL plus the backfill statements.
--
-- Round-trip safety: the Postgres dialect is mirrored at
-- migrations/postgres/0011_v0631_data_integrity.sql. Both adapters
-- end up with `embedding_dim INTEGER` on `memories` and on
-- `archived_memories`, plus `embedding BLOB`, `original_tier TEXT`,
-- `original_expires_at TEXT` on `archived_memories`.

-- G4 backfill — infer embedding_dim from BLOB length / 4 for any row
-- written before v17. Idempotent: only updates rows where the column
-- is NULL but an embedding exists. Rows without embeddings keep
-- embedding_dim NULL (no violation).
UPDATE memories
SET embedding_dim = length(embedding) / 4
WHERE embedding_dim IS NULL
  AND embedding IS NOT NULL
  AND length(embedding) >= 4;

-- G5 backfill — pre-existing archive rows lost embedding/tier metadata
-- before v17. The live rows are gone (archive is a delete-and-copy
-- destination), so the original tier and expiry are irrecoverable.
-- Default `original_tier='long'` so restore_archived will not delete
-- the row again on its first restoration pass; default
-- `original_expires_at=NULL` so the restored row stays permanent until
-- the operator chooses to retier it. This loss is acknowledged in the
-- charter (REMEDIATIONv0631 §P2 G5).
UPDATE archived_memories
SET original_tier = 'long'
WHERE original_tier IS NULL;

-- G4 indexes — operators querying for dim-violation hot-spots and the
-- doctor command's `dim_violations` stat both filter by embedding_dim.
CREATE INDEX IF NOT EXISTS idx_memories_embedding_dim
    ON memories (embedding_dim)
    WHERE embedding_dim IS NOT NULL;

-- Per-namespace dim survey — supports the "first write establishes
-- dim" check (db.rs::namespace_embedding_dim). Without an index, the
-- check would scan the whole namespace on every write.
CREATE INDEX IF NOT EXISTS idx_memories_ns_dim
    ON memories (namespace, embedding_dim)
    WHERE embedding_dim IS NOT NULL;
