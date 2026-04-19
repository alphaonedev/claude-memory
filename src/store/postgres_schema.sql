-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- Postgres + pgvector schema for the PostgresStore SAL adapter (v0.7).
-- Idempotent — this script runs on every PostgresStore::connect() and
-- must tolerate being re-executed against an already-populated DB.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS memories (
    id                TEXT PRIMARY KEY,
    tier              TEXT NOT NULL,
    namespace         TEXT NOT NULL,
    title             TEXT NOT NULL,
    content           TEXT NOT NULL,
    tags              JSONB NOT NULL DEFAULT '[]'::jsonb,
    priority          INTEGER NOT NULL DEFAULT 5,
    confidence        DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    source            TEXT NOT NULL,
    access_count      BIGINT NOT NULL DEFAULT 0,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_accessed_at  TIMESTAMPTZ,
    expires_at        TIMESTAMPTZ,
    metadata          JSONB NOT NULL DEFAULT '{}'::jsonb,
    embedding         vector(384)
);

CREATE INDEX IF NOT EXISTS memories_namespace_idx ON memories (namespace);
CREATE INDEX IF NOT EXISTS memories_tier_idx ON memories (tier);
CREATE INDEX IF NOT EXISTS memories_priority_idx ON memories (priority DESC);
CREATE INDEX IF NOT EXISTS memories_updated_at_idx ON memories (updated_at DESC);
CREATE INDEX IF NOT EXISTS memories_expires_at_idx ON memories (expires_at)
    WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS memories_tags_gin ON memories USING gin (tags);
CREATE INDEX IF NOT EXISTS memories_metadata_gin ON memories USING gin (metadata);

-- Full-text search index. Uses English stemming; the ai-memory codebase
-- is English-primary today. Add per-namespace configurable stemming in
-- a follow-up when the i18n track opens.
CREATE INDEX IF NOT EXISTS memories_content_fts ON memories
    USING gin (to_tsvector('english', title || ' ' || content));

-- HNSW vector index for cosine-distance nearest-neighbor queries.
-- Default pgvector HNSW params are reasonable for up to ~1M rows;
-- tune `m` / `ef_construction` via ALTER INDEX ... SET for larger
-- corpora. Rebuild required on parameter change.
CREATE INDEX IF NOT EXISTS memories_embedding_hnsw ON memories
    USING hnsw (embedding vector_cosine_ops);
