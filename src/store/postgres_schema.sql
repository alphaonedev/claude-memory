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
CREATE INDEX IF NOT EXISTS idx_memories_namespace_path
    ON memories (namespace text_pattern_ops);
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
    source_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation     TEXT NOT NULL DEFAULT 'related_to',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_from   TIMESTAMPTZ,
    valid_until  TIMESTAMPTZ,
    observed_by  TEXT,
    signature    BYTEA,
    -- v0.7.0 H2 attestation tag — mirrors SQLite migration 0017
    -- (`attest_level` TEXT). Allowed values: "unsigned", "self_signed",
    -- "peer_attested". NULL is treated as "unsigned" by readers for
    -- back-compat with v0.6.3 rows written before this column existed.
    attest_level TEXT,
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
-- v0.7.0 H4 — `memory_verify` listing path probes by attest_level.
CREATE INDEX IF NOT EXISTS idx_memory_links_attest_level
    ON memory_links (attest_level, created_at);

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
    id                       TEXT PRIMARY KEY,
    action_type              TEXT NOT NULL,
    memory_id                TEXT,
    namespace                TEXT NOT NULL,
    payload                  JSONB NOT NULL DEFAULT '{}'::jsonb,
    requested_by             TEXT NOT NULL,
    requested_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status                   TEXT NOT NULL DEFAULT 'pending',
    decided_by               TEXT,
    decided_at               TIMESTAMPTZ,
    approvals                JSONB NOT NULL DEFAULT '[]'::jsonb,
    -- v0.7.0 K2 — pending_actions timeout sweeper. Per-row TTL
    -- (NULL → cluster default) and the stamp set when the sweep
    -- transitions a stale row to status='expired'.
    default_timeout_seconds  BIGINT,
    expired_at               TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS pending_actions_status_idx    ON pending_actions (status);
CREATE INDEX IF NOT EXISTS pending_actions_namespace_idx ON pending_actions (namespace);
-- v0.7.0 K2 — composite index for the 60-second sweep query
-- (`WHERE status='pending' AND ...julianday math`).
CREATE INDEX IF NOT EXISTS pending_actions_status_requested_idx
    ON pending_actions (status, requested_at);

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

-- ─────────────────────────────────────────────────────────────────────
-- F6 Gap 1 (v0.7.0) — SAL knowledge-graph SQL views.
--
-- These three views surface the shapes that the SAL trait's KG ops
-- ([`PostgresStore::kg_query`], [`kg_timeline`], [`find_paths`])
-- return, but expressed as pure recursive-CTE SQL so they work whether
-- Apache AGE is loaded or not. Operators can `SELECT * FROM
-- kg_query_view WHERE source_id = ...` from psql, BI tools, or
-- federated queries without going through the Rust SAL — handy for
-- ad-hoc auditing and for clients that haven't picked up the SAL crate.
--
-- Idempotent via `CREATE OR REPLACE` so the schema bootstrap stays
-- re-runnable. The views read directly from `memory_links` / `memories`
-- so they never lag behind a write.
-- ─────────────────────────────────────────────────────────────────────

-- kg_query_view — recursive traversal projection.
--
-- Mirrors [`PostgresStore::kg_query_cte`]: per-source row, walks edges
-- up to depth 5 (the published SAL ceiling — see
-- `KG_QUERY_MAX_SUPPORTED_DEPTH` in src/store/postgres.rs). Cycle
-- prevention via path substring containment, identical to the live
-- query body, so the view's depth/cycle semantics are byte-equivalent
-- to what the SAL returns.
--
-- Callers that want a different depth ceiling should query
-- `PostgresStore::kg_query` directly — the view is fixed at 5 so it can
-- be a plain `SELECT *` from psql without extra parameters.
CREATE OR REPLACE VIEW kg_query_view AS
WITH RECURSIVE traversal(source_id, target_id, relation, depth, path) AS (
    SELECT ml.source_id, ml.target_id, ml.relation, 1,
           ml.source_id || '->' || ml.target_id
    FROM memory_links ml
    UNION ALL
    SELECT t.source_id, ml.target_id, ml.relation, t.depth + 1,
           t.path || '->' || ml.target_id
    FROM memory_links ml
    JOIN traversal t ON ml.source_id = t.target_id
    WHERE t.depth < 5
      AND position(('->' || ml.target_id) IN t.path) = 0
      AND position((ml.target_id || '->') IN t.path) = 0
)
SELECT source_id, target_id, relation, depth, path
FROM traversal;

-- kg_timeline_view — temporal-validity projection.
--
-- Mirrors [`PostgresStore::kg_timeline_cte`]: rows ordered by
-- `valid_from DESC` (the SAL's authoritative ordering key for
-- timeline scans), filtering NULL `valid_from` to match the
-- contract documented on `db::kg_timeline`. The signature column
-- is surfaced as a hex string so consumers don't need to handle
-- BYTEA — `signature_hex` is `NULL` when the row is unsigned.
CREATE OR REPLACE VIEW kg_timeline_view AS
SELECT
    ml.source_id,
    ml.target_id,
    ml.relation,
    ml.valid_from,
    ml.valid_until,
    ml.observed_by,
    encode(ml.signature, 'hex') AS signature_hex
FROM memory_links ml
WHERE ml.valid_from IS NOT NULL
ORDER BY ml.valid_from DESC, ml.created_at DESC;

-- kg_find_paths(start_id text, max_depth int) — path enumeration.
--
-- Views can't accept parameters in Postgres, so this surfaces as a SQL
-- function instead. Mirrors [`PostgresStore::find_paths_cte`]: undirected
-- traversal (edges unioned with their reverse), TEXT[] visited prefix
-- for cycle prevention, ordered shortest-first.
--
-- `max_depth` is clamped to the SAL ceiling (FIND_PATHS_MAX_DEPTH_SAL =
-- 7) so a crafted call cannot fan out an unbounded scan from psql. The
-- function is `STABLE` because `memory_links` is the only data it
-- reads; PARALLEL SAFE because it has no side effects.
CREATE OR REPLACE FUNCTION kg_find_paths(start_id TEXT, max_depth INTEGER)
RETURNS TABLE (path_id INTEGER, length INTEGER, nodes TEXT[], relations TEXT[])
LANGUAGE SQL STABLE PARALLEL SAFE AS $$
    WITH RECURSIVE walk(current_id, depth, nodes, relations) AS (
        SELECT start_id, 0, ARRAY[start_id], ARRAY[]::TEXT[]
        UNION ALL
        SELECT edges.next_id,
               w.depth + 1,
               w.nodes || edges.next_id,
               w.relations || edges.relation
        FROM walk w
        JOIN (
            SELECT source_id AS from_id, target_id AS next_id, relation FROM memory_links
            UNION
            SELECT target_id AS from_id, source_id AS next_id, relation FROM memory_links
        ) edges ON edges.from_id = w.current_id
        WHERE w.depth < LEAST(max_depth, 7)
          AND NOT (edges.next_id = ANY(w.nodes))
    )
    SELECT
        ROW_NUMBER() OVER (ORDER BY depth ASC, nodes ASC)::INTEGER AS path_id,
        depth                                                      AS length,
        nodes,
        relations
    FROM walk
    WHERE depth >= 1
$$;
