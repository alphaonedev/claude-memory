-- v0.6.3.1 Phase P2 — Data-integrity hardening (schema v17, Postgres dialect).
-- Mirror of migrations/sqlite/0011_v0631_data_integrity.sql. Closes audit
-- findings G4 (mixed embedding dims silently tolerated), G5 (archive
-- lossy + restore resets tier/expires_at), and G13 (embedding magic-byte
-- header). G6 is a handler-layer fix.
--
-- Postgres allows ALTER TABLE ADD COLUMN IF NOT EXISTS, so the column
-- additions live here directly (the SQLite adapter handles them at the
-- Rust layer with PRAGMA table_info checks).

-- G4 — embedding dimensionality on `memories`.
ALTER TABLE memories
    ADD COLUMN IF NOT EXISTS embedding_dim INTEGER;

-- G5 — preserve embedding + original tier/expiry on archive.
ALTER TABLE archived_memories
    ADD COLUMN IF NOT EXISTS embedding         BYTEA,
    ADD COLUMN IF NOT EXISTS embedding_dim     INTEGER,
    ADD COLUMN IF NOT EXISTS original_tier     TEXT,
    ADD COLUMN IF NOT EXISTS original_expires_at TIMESTAMPTZ;

-- G4 backfill — infer embedding_dim from the pgvector embedding column
-- when present. The pgvector path stores `vector(384)` so the dim is
-- known at type level; we still record it explicitly so the SQLite and
-- Postgres adapters surface the same `dim_violations` stat.
UPDATE memories
SET embedding_dim = 384
WHERE embedding_dim IS NULL
  AND embedding IS NOT NULL;

-- G5 backfill — pre-existing archive rows have no recoverable original
-- tier (the live row is gone). Default to 'long' so restore_archived
-- treats them as permanent on first restoration.
UPDATE archived_memories
SET original_tier = 'long'
WHERE original_tier IS NULL;

-- G4 indexes — supports `dim_violations` stat and per-namespace dim
-- survey for the first-write-establishes-dim check.
CREATE INDEX IF NOT EXISTS idx_memories_embedding_dim
    ON memories (embedding_dim)
    WHERE embedding_dim IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_memories_ns_dim
    ON memories (namespace, embedding_dim)
    WHERE embedding_dim IS NOT NULL;
