-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- Postgres + pgvector schema for the PostgresStore SAL adapter.
-- Idempotent — this script runs on every PostgresStore::connect() and
-- must tolerate being re-executed against an already-populated DB.
--
-- Schema parity with the SQLite backend (src/db.rs). Tables: memories,
-- memory_links, archived_memories, namespace_meta, pending_actions,
-- sync_state, subscriptions. All v0.6.0 features (agent_id immutability,
-- (title, namespace) upsert, tier-downgrade protection, scope_idx) are
-- expressed at the SQL layer so the two adapters have identical
-- semantics.

-- pgvector extension. Supported version range: 0.7.x–0.8.x. pgvector
-- had breaking HNSW behaviour changes between 0.5 and 0.7 — if we
-- widen the range we MUST re-test HNSW recall on the fixture corpus.
-- Pin the minimum at adapter connect time rather than here; the
-- schema is run against whatever version is available, and the
-- adapter checks `SELECT extversion FROM pg_extension WHERE
-- extname='vector'` afterwards (see PostgresStore::connect).
CREATE EXTENSION IF NOT EXISTS vector;

-- Tier precedence — matches SQLite's Tier::rank() in src/models.rs.
-- Used to enforce "tier is never downgraded" on UPSERT and UPDATE
-- (blocker #296). Marked IMMUTABLE so query planner + generated column
-- can embed it without recomputation per row.
CREATE OR REPLACE FUNCTION tier_rank(t TEXT) RETURNS INTEGER
    LANGUAGE SQL IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE t
        WHEN 'short' THEN 0
        WHEN 'mid' THEN 1
        WHEN 'long' THEN 2
        ELSE 0
    END
$$;

-- ─────────────────────────────────────────────────────────────────────
-- schema_version — migration tracking (v0.7 in-place migration support).
-- 
-- Tracks the highest schema version applied to this Postgres instance.
-- Mirrors the SQLite CURRENT_SCHEMA_VERSION constant and schema_version
-- table in src/db.rs. The migration runner (PostgresStore::migrate)
-- reads MAX(version) here to determine which steps to apply.
-- Idempotent: if the table exists, the migration runner skips schema
-- setup steps already applied.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─────────────────────────────────────────────────────────────────────
-- memories — the core memory table.
-- ─────────────────────────────────────────────────────────────────────
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
    embedding         vector(384),
    -- v0.6.0 GA: generated column indexing metadata.scope for
    -- visibility queries. Mirrors SQLite's scope_idx migration (v10).
    scope_idx         TEXT GENERATED ALWAYS AS (
        COALESCE(metadata ->> 'scope', 'private')
    ) STORED
);

-- v0.6.0 blocker #294 fix: upsert contract is `(title, namespace)`.
-- SQLite enforces this with `CREATE UNIQUE INDEX idx_memories_title_ns`
-- (src/db.rs:132); Postgres matches here so both adapters agree on
-- upsert semantics.
CREATE UNIQUE INDEX IF NOT EXISTS memories_title_ns_uidx
    ON memories (title, namespace);

CREATE INDEX IF NOT EXISTS memories_namespace_idx ON memories (namespace);
CREATE INDEX IF NOT EXISTS memories_tier_idx      ON memories (tier);
CREATE INDEX IF NOT EXISTS memories_priority_idx  ON memories (priority DESC);
CREATE INDEX IF NOT EXISTS memories_updated_at_idx ON memories (updated_at DESC);
CREATE INDEX IF NOT EXISTS memories_expires_at_idx ON memories (expires_at)
    WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS memories_tags_gin      ON memories USING gin (tags);
CREATE INDEX IF NOT EXISTS memories_metadata_gin  ON memories USING gin (metadata);
CREATE INDEX IF NOT EXISTS memories_scope_idx_idx ON memories (scope_idx);

-- Full-text search. English stemming; matches the SQLite FTS5 setup.
CREATE INDEX IF NOT EXISTS memories_content_fts ON memories
    USING gin (to_tsvector('english', title || ' ' || content));

-- HNSW vector index for cosine-distance nearest-neighbor queries.
-- NOTE: this operator family returns cosine DISTANCE (smaller=closer),
-- while the SQLite HNSW path returns cosine SIMILARITY (larger=closer).
-- Adapter-level code must normalise via `1 - distance` before blending
-- with reranker scores. Tracked in #302.
CREATE INDEX IF NOT EXISTS memories_embedding_hnsw ON memories
    USING hnsw (embedding vector_cosine_ops);

-- ─────────────────────────────────────────────────────────────────────
-- memory_links — directional typed links between memories.
--
-- v0.6.3 Stream B: temporal columns + entity_aliases side table
-- mirror SQLite schema v15 (see src/db.rs::migrate). Forward-compatible
-- with v0.7 Apache AGE acceleration: same columns get projected as
-- AGE graph edges. Existing PG installs at v0.6.2 will not gain the
-- new columns automatically — the Postgres path is currently a fresh-
-- init only target (see src/store/postgres.rs notes). An explicit ALTER
-- migration lands when the link() implementation is wired up.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS memory_links (
    source_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation    TEXT NOT NULL DEFAULT 'related_to',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_from  TIMESTAMPTZ,
    valid_until TIMESTAMPTZ,
    observed_by TEXT,
    signature   BYTEA,
    PRIMARY KEY (source_id, target_id, relation)
);

CREATE INDEX IF NOT EXISTS memory_links_source_idx ON memory_links (source_id);
CREATE INDEX IF NOT EXISTS memory_links_target_idx ON memory_links (target_id);
CREATE INDEX IF NOT EXISTS idx_links_temporal_src
    ON memory_links (source_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt
    ON memory_links (target_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_relation
    ON memory_links (relation, valid_from);

-- ─────────────────────────────────────────────────────────────────────
-- entity_aliases — alias→entity_id resolution (v0.6.3 Stream B/C).
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS entity_aliases (
    entity_id  TEXT NOT NULL,
    alias      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (entity_id, alias)
);

CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
    ON entity_aliases (alias);

-- ─────────────────────────────────────────────────────────────────────
-- archived_memories — GC archive for restoration.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS archived_memories (
    id               TEXT PRIMARY KEY,
    tier             TEXT NOT NULL,
    namespace        TEXT NOT NULL DEFAULT 'global',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    tags             JSONB NOT NULL DEFAULT '[]'::jsonb,
    priority         INTEGER NOT NULL DEFAULT 5,
    confidence       DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    source           TEXT NOT NULL DEFAULT 'api',
    access_count     BIGINT NOT NULL DEFAULT 0,
    created_at       TIMESTAMPTZ NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL,
    last_accessed_at TIMESTAMPTZ,
    expires_at       TIMESTAMPTZ,
    archived_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    archive_reason   TEXT NOT NULL DEFAULT 'ttl_expired',
    metadata         JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS archived_memories_namespace_idx  ON archived_memories (namespace);
CREATE INDEX IF NOT EXISTS archived_memories_archived_at_idx ON archived_memories (archived_at);

-- ─────────────────────────────────────────────────────────────────────
-- namespace_meta — namespace standard / policy (Tasks 1.6–1.8).
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS namespace_meta (
    namespace         TEXT PRIMARY KEY,
    standard_id       TEXT,
    parent_namespace  TEXT,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─────────────────────────────────────────────────────────────────────
-- pending_actions — governance approval queue (Task 1.9–1.10).
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS pending_actions (
    id            TEXT PRIMARY KEY,
    action_type   TEXT NOT NULL,
    memory_id     TEXT,
    namespace     TEXT NOT NULL,
    payload       JSONB NOT NULL DEFAULT '{}'::jsonb,
    requested_by  TEXT NOT NULL,
    requested_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status        TEXT NOT NULL DEFAULT 'pending',
    decided_by    TEXT,
    decided_at    TIMESTAMPTZ,
    approvals     JSONB NOT NULL DEFAULT '[]'::jsonb
);

CREATE INDEX IF NOT EXISTS pending_actions_status_idx    ON pending_actions (status);
CREATE INDEX IF NOT EXISTS pending_actions_namespace_idx ON pending_actions (namespace);

-- ─────────────────────────────────────────────────────────────────────
-- sync_state — per-peer vector-clock high-watermarks.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS sync_state (
    agent_id         TEXT NOT NULL,
    peer_id          TEXT NOT NULL,
    last_seen_at     TIMESTAMPTZ NOT NULL,
    last_pulled_at   TIMESTAMPTZ NOT NULL,
    last_pushed_at   TIMESTAMPTZ,
    PRIMARY KEY (agent_id, peer_id)
);

CREATE INDEX IF NOT EXISTS sync_state_agent_idx ON sync_state (agent_id);

-- ─────────────────────────────────────────────────────────────────────
-- subscriptions — webhook registrations (v0.6.0).
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS subscriptions (
    id                 TEXT PRIMARY KEY,
    url                TEXT NOT NULL,
    events             TEXT NOT NULL DEFAULT '*',
    secret_hash        TEXT,
    namespace_filter   TEXT,
    agent_filter       TEXT,
    created_by         TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_dispatched_at TIMESTAMPTZ,
    dispatch_count     BIGINT NOT NULL DEFAULT 0,
    failure_count      BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS subscriptions_url_idx ON subscriptions (url);
