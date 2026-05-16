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
    -- v0.7.0 L3 — `vector({EMBEDDING_DIM})` is a templated literal
    -- substituted at adapter init time. Defaults to dim 384
    -- (MiniLM-L6-v2) when the caller doesn't specify; operators using
    -- `nomic_embed_v15` pass `--embedding-dim 768` via `ai-memory
    -- schema-init`. The substitution is a single
    -- `str::replace("{EMBEDDING_DIM}", ...)` in `PostgresStore::connect`.
    embedding         vector({EMBEDDING_DIM}),
    -- v0.6.3.1 P2 / G4 — declared embedding dimension. Lets the
    -- daemon refuse a write whose vector dimension disagrees with
    -- the column declaration without falling back to byte-length
    -- arithmetic. Mirrors SQLite migration 0011_v0631_data_integrity.
    embedding_dim     INTEGER,
    -- v0.6.0 GA: generated column indexing metadata.scope for
    -- visibility queries. Mirrors SQLite's scope_idx migration (v10).
    scope_idx         TEXT GENERATED ALWAYS AS (
        COALESCE(metadata ->> 'scope', 'private')
    ) STORED,
    -- v0.6.0 GA / Ultrareview #342 — generated column indexing
    -- metadata.agent_id so list / search / recall predicates that
    -- filter by agent_id become real index lookups rather than
    -- json-extract scans. Mirrors SQLite migration v14.
    agent_id_idx      TEXT GENERATED ALWAYS AS (
        metadata ->> 'agent_id'
    ) STORED,
    -- v0.7.0 M15 — schema v30: enforce that metadata is a JSON object.
    -- The two generated columns above silently project NULL when
    -- metadata is anything else (array / scalar / NULL), which masks
    -- governance / scope-routing misconfiguration. The CHECK rejects
    -- the malformed row at the write boundary instead. Fresh schemas
    -- carry this inline; existing schemas pick it up via migrate_v30().
    CONSTRAINT memories_metadata_is_object
        CHECK (jsonb_typeof(metadata) = 'object'),
    -- v0.7.0 Task 1/8 (schema v31, recursive learning) — depth in the
    -- substrate-native reflection recursion tree. `0` for caller-minted
    -- (or pre-v0.7.0) rows; positive for memories synthesised by the
    -- reflection pass over lower-depth peers. Fresh schemas carry this
    -- inline; existing schemas pick it up via migrate_v31().
    reflection_depth  INTEGER NOT NULL DEFAULT 0,
    -- v0.7.0 WT-1-A (schema v35 postgres / v36 sqlite) — substrate-level
    -- atomisation foundation. `atomised_into` is NULL on legacy rows;
    -- positive integer on rows that have been split by the WT-1-B
    -- atomisation pass. `atom_of` is NULL on non-atom rows; on atom
    -- rows it FK-points back to the parent memory.
    atomised_into     INTEGER,
    atom_of           TEXT REFERENCES memories(id),
    -- v0.7.0 typed kind discriminator (mirrors SQLite migration v30+).
    -- Originally added by sqlite migration as the Memory.memory_kind
    -- enum landed; the postgres base schema was missing this column
    -- (the persona idx at the end of this file references it). v0.7.0
    -- Form 6 extends the vocabulary to claim/relation/event/etc. Fresh
    -- schemas carry this inline; existing schemas pick it up via the
    -- WT-1-A-era migration ladder arm.
    memory_kind       TEXT NOT NULL DEFAULT 'observation',
    -- v0.7.0 QW-2 (schema v36 postgres / v37 sqlite) — Persona-as-artifact
    -- discriminator columns. Populated only for `memory_kind = 'persona'`
    -- rows; every observation/reflection keeps NULL. Fresh schemas carry
    -- these inline; existing schemas pick them up via migrate_v36().
    entity_id         TEXT,
    persona_version   INTEGER,
    -- v0.7.0 Form 4 (schema v37 postgres / v38 sqlite, issue #757) —
    -- fact-provenance closeout. `citations` is a JSON-encoded array
    -- (TEXT, not JSONB, to mirror SQLite's TEXT column) of Citation
    -- objects ({uri, accessed_at, hash?, span?}) — first-class
    -- per-memory provenance pointers. `source_uri` is a URI-form
    -- pointer (uri:/doc:/file:) to the cited source body, distinct
    -- from the role-label `source` column. `source_span` is a JSON
    -- {start, end} byte-range into the parent source body, populated
    -- by the WT-1-B atomisation writer for atom-grain provenance.
    -- Fresh schemas carry these inline; existing schemas pick them up
    -- via migrate_v37().
    citations         TEXT NOT NULL DEFAULT '[]',
    source_uri        TEXT,
    source_span       TEXT,
    -- v0.7.0 Form 5 (schema v38 postgres / v39 sqlite, issue #758) —
    -- auto-confidence + shadow-mode + calibration tooling closeout.
    -- `confidence_source` is a typed discriminator for the provenance
    -- of the `confidence` column value (caller_provided | auto_derived
    -- | calibrated | decayed). `confidence_signals` is a JSON snapshot
    -- of the signals that produced the derivation. `confidence_decayed_at`
    -- is an RFC3339 stamp of the last decay computation. Fresh schemas
    -- carry these inline; existing schemas pick them up via migrate_v38().
    confidence_source     TEXT NOT NULL DEFAULT 'caller_provided',
    confidence_signals    TEXT,
    confidence_decayed_at TEXT,
    -- v0.7.0 polish PERF-8 (schema v41 postgres / v42 sqlite, issue
    -- #781) — auto-persona indexed entity-id column. Carries the
    -- canonical entity descriptor a memory MENTIONS (extracted at
    -- write time from `metadata.entity_id` or a `[entity:X]` title
    -- marker) so the SQLite-side auto-persona matcher resolves with
    -- `WHERE mentioned_entity_id = ?` instead of the previous
    -- full-table `content LIKE '%X%'` scan. Schema parity column on
    -- Postgres — the auto-persona executor itself is SQLite-only.
    -- Deliberately distinct from the QW-2 `entity_id` above (which is
    -- reserved for Persona-row attribution): PERF-8 reads the OPPOSITE
    -- direction (the entity a non-persona row mentions). Fresh
    -- schemas carry this inline; existing schemas pick it up via
    -- migrate_v41().
    mentioned_entity_id   TEXT
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
-- v0.6.0 / Ultrareview #342 — agent_id_idx (generated column) + created_at.
CREATE INDEX IF NOT EXISTS idx_memories_agent_id ON memories (agent_id_idx);
CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories (created_at);
-- v0.6.3.1 P2 / G4 — partial index on embedding_dim for hot-spot doctor
-- queries and the per-namespace "first write establishes dim" check.
CREATE INDEX IF NOT EXISTS idx_memories_embedding_dim
    ON memories (embedding_dim)
    WHERE embedding_dim IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_memories_ns_dim
    ON memories (namespace, embedding_dim)
    WHERE embedding_dim IS NOT NULL;
-- Partial indexes that reference columns ALTER-added by the migrate
-- ladder (`atom_of` / `atomised_into` v35 postgres ↔ v36 sqlite;
-- `entity_id` v36 ↔ v37; `source_uri` v37 ↔ v38; `confidence_source`
-- v38 ↔ v39; `mentioned_entity_id` v41 ↔ v42) and the v40 compound
-- shadow index referencing `confidence_shadow_observations.source` are
-- NOT in this bootstrap (issue #797). They live exclusively in their
-- migration .sql files (`migrations/postgres/0017_v07_atomisation.sql`,
-- `0018_v07_persona.sql`, `0019_v07_form4_provenance.sql`,
-- `0020_v07_form5_confidence_calibration.sql`,
-- `0022_v07_shadow_retention.sql`,
-- `0023_v07_auto_persona_entity_id.sql`) and run from the matching
-- `migrate_vN` arms of `src/store/postgres.rs::migrate` AFTER the
-- `ALTER TABLE memories ADD COLUMN IF NOT EXISTS` that adds the
-- referenced column.
--
-- `PostgresStore::connect` applies this bootstrap before `migrate`, so
-- any `CREATE INDEX` here referencing a v35+ column would crash on a
-- legacy DB whose pre-existing `memories` table makes the `CREATE TABLE
-- IF NOT EXISTS` above a no-op (the new columns never land). Fresh
-- installs are unaffected: the `CREATE TABLE` above carries every
-- v41-era column, then every `if current_version < N` arm runs its
-- (idempotent) .sql file to attach the partial index.

-- v0.7.0 Form 5 — per-recall shadow-mode telemetry. Populated when
-- AI_MEMORY_CONFIDENCE_SHADOW=1 and sampled at
-- AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE. The calibration CLI reads
-- this table to compute per-(namespace, source) baselines.
-- v0.7.0 Cluster G — added the denormalised `source` column + compound
-- `(namespace, source, observed_at)` index so the calibration sweep
-- streams a single-table SQL aggregation instead of materialising the
-- full window into Rust memory (PERF-12).
CREATE TABLE IF NOT EXISTS confidence_shadow_observations (
    id BIGSERIAL PRIMARY KEY,
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    namespace TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'unknown',
    caller_confidence DOUBLE PRECISION NOT NULL,
    derived_confidence DOUBLE PRECISION NOT NULL,
    signals TEXT NOT NULL,
    recall_outcome TEXT,
    observed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_shadow_obs_namespace
    ON confidence_shadow_observations(namespace);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_observed_at
    ON confidence_shadow_observations(observed_at);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_memory
    ON confidence_shadow_observations(memory_id);
-- `idx_shadow_obs_namespace_source_observed` references the v40
-- `confidence_shadow_observations.source` column and lives in
-- `migrations/postgres/0022_v07_shadow_retention.sql` (see #797
-- comment block above).

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
    PRIMARY KEY (source_id, target_id, relation),
    -- v0.7.0 v0.7.1-fold (#687/#688, schema v32 postgres / v33 sqlite) —
    -- closed taxonomy CHECK constraint on `relation`. Mirrors
    -- `crate::validate::VALID_RELATIONS` exactly. The constraint name is
    -- pinned so the migration (`migrations/postgres/0014_v07_memory_links_relation_check.sql`)
    -- can probe pg_constraint for it via `IF NOT EXISTS` semantics.
    -- v0.7.0 WT-1-A (schema v35 postgres / v36 sqlite) extended the
    -- closed set with `derives_from` for atomisation provenance edges
    -- and re-pinned the constraint under the `_wt1a` suffix so the
    -- 0017 migration can detect the upgraded form via pg_constraint.
    CONSTRAINT memory_links_relation_check_wt1a
        CHECK (relation IN ('related_to', 'supersedes', 'contradicts', 'derived_from', 'reflects_on', 'derives_from'))
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
    id                  TEXT PRIMARY KEY,
    tier                TEXT NOT NULL,
    namespace           TEXT NOT NULL DEFAULT 'global',
    title               TEXT NOT NULL,
    content             TEXT NOT NULL,
    tags                JSONB NOT NULL DEFAULT '[]'::jsonb,
    priority            INTEGER NOT NULL DEFAULT 5,
    confidence          DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    source              TEXT NOT NULL DEFAULT 'api',
    access_count        BIGINT NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL,
    last_accessed_at    TIMESTAMPTZ,
    expires_at          TIMESTAMPTZ,
    archived_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    archive_reason      TEXT NOT NULL DEFAULT 'ttl_expired',
    metadata            JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- v0.6.3.1 P2 / G5 — preserve embedding + original tier/expiry
    -- across archive→restore so a restored row is byte-identical to
    -- the live row that was archived. Mirrors SQLite migration
    -- 0011_v0631_data_integrity. Templated dim — see comment on
    -- `memories.embedding` above.
    embedding           vector({EMBEDDING_DIM}),
    embedding_dim       INTEGER,
    original_tier       TEXT,
    original_expires_at TIMESTAMPTZ
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
    failure_count      BIGINT NOT NULL DEFAULT 0,
    -- v0.6.3.1 P5 / G9 — structured event-type opt-in, JSON-encoded
    -- array stored as JSONB for direct GIN/path indexing if a future
    -- task needs it. NULL = legacy all-events ('*' on the `events`
    -- column). Mirrors SQLite migration 0013_webhook_event_types.
    event_types        JSONB
);

CREATE INDEX IF NOT EXISTS subscriptions_url_idx ON subscriptions (url);
CREATE INDEX IF NOT EXISTS idx_subscriptions_event_types
    ON subscriptions (event_types);

-- ─────────────────────────────────────────────────────────────────────
-- audit_log — capability-expansion audit (v0.6.4-009, NHI guardrails
-- phase 1, schema v20).
--
-- Mirrors `migrations/sqlite/0014_v064_audit_log.sql`. Records every
-- `memory_capabilities --include-schema family=<f>` call so operators
-- can audit which agents asked for which families and whether the
-- request was granted. Distinct from the SOC2/HIPAA hash-chained audit
-- trail in `audit/*.log` (file-based, tamper-evident); this table is
-- for runtime observability inside the database itself.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS audit_log (
    id                 TEXT PRIMARY KEY,
    agent_id           TEXT,
    event_type         TEXT NOT NULL,
    requested_family   TEXT,
    -- Postgres uses native BOOLEAN here vs SQLite's INTEGER 0/1 — both
    -- adapters present a Rust-side `bool` to callers. Migration code
    -- coerces legacy INTEGER columns where needed.
    granted            BOOLEAN NOT NULL,
    attestation_tier   TEXT,
    timestamp          TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_log_agent_id    ON audit_log (agent_id);
CREATE INDEX IF NOT EXISTS idx_audit_log_timestamp   ON audit_log (timestamp);
CREATE INDEX IF NOT EXISTS idx_audit_log_event_type  ON audit_log (event_type);

-- ─────────────────────────────────────────────────────────────────────
-- memory_transcripts — attested-cortex transcripts substrate
-- (v0.7.0 I1, schema v22).
--
-- Mirrors `migrations/sqlite/0016_v07_transcripts.sql`. Compressed
-- (zstd-3) storage of raw conversation transcripts so memories can
-- later be re-grounded against the verbatim source. The blob is
-- bytea on Postgres / BLOB on SQLite; both adapters store the same
-- zstd-encoded payload byte-for-byte.
--
-- Substrate only — Rust write/read paths (transcripts.rs) currently
-- bind to SQLite. Adapter-level wiring lands in a future SAL wave.
-- The schema is kept here so `schema-init` against Postgres lays
-- the same foundation a SQLite bootstrap would.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS memory_transcripts (
    id               TEXT PRIMARY KEY,
    namespace        TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL,
    expires_at       TIMESTAMPTZ,
    compressed_size  BIGINT NOT NULL,
    original_size    BIGINT NOT NULL,
    zstd_level       INTEGER NOT NULL DEFAULT 3,
    content_blob     BYTEA NOT NULL,
    -- v0.7.0 I3 — archive→prune lifecycle marker. NULL = live row.
    -- RFC3339-equivalent timestamp on Postgres (TIMESTAMPTZ) when the
    -- sweeper marked the row archived.
    archived_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_memory_transcripts_namespace_created
    ON memory_transcripts (namespace, created_at);
-- v0.7.0 I3 — partial index on archived rows so the prune-phase scan
-- stays O(archived) rather than O(total transcripts) on busy
-- namespaces. Mirrors SQLite migration 0019_v07_transcript_lifecycle.
CREATE INDEX IF NOT EXISTS idx_memory_transcripts_archived_at
    ON memory_transcripts (archived_at)
    WHERE archived_at IS NOT NULL;

-- ─────────────────────────────────────────────────────────────────────
-- memory_transcript_links — m:n join between memories and transcripts
-- (v0.7.0 I2, schema v24).
--
-- Mirrors `migrations/sqlite/0018_v07_transcript_links.sql`. One
-- memory can be derived from several transcript spans; one transcript
-- can be the source for many memories. Optional (span_start, span_end)
-- byte offsets address a sub-region of the decompressed transcript.
-- ON DELETE CASCADE on both foreign keys keeps the join free of
-- dangling rows when memories are deleted or I3's archive→prune
-- lifecycle removes transcripts.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS memory_transcript_links (
    memory_id     TEXT NOT NULL REFERENCES memories(id)           ON DELETE CASCADE,
    transcript_id TEXT NOT NULL REFERENCES memory_transcripts(id) ON DELETE CASCADE,
    span_start    BIGINT,
    span_end      BIGINT,
    PRIMARY KEY (memory_id, transcript_id)
);

CREATE INDEX IF NOT EXISTS idx_mtl_transcript ON memory_transcript_links (transcript_id);
CREATE INDEX IF NOT EXISTS idx_mtl_memory     ON memory_transcript_links (memory_id);

-- ─────────────────────────────────────────────────────────────────────
-- signed_events — append-only audit chain over identity-bearing
-- writes (v0.7.0 H5, schema v26).
--
-- Mirrors `migrations/sqlite/0020_v07_signed_events.sql`. Every
-- `memory_link` write (signed or unsigned) appends one row so a
-- downstream auditor can replay the exact sequence of attestation
-- events the daemon emitted. The append-only invariant is enforced
-- at the Rust API surface (`signed_events::append_signed_event` is
-- the only writer; no UPDATE/DELETE call sites exist) — no triggers
-- are added at the SQL layer because they would also fire against
-- operator-driven retention pruning, defeating the escape hatch.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS signed_events (
    id           TEXT PRIMARY KEY,
    agent_id     TEXT NOT NULL,
    event_type   TEXT NOT NULL,
    payload_hash BYTEA NOT NULL,
    signature    BYTEA,
    attest_level TEXT NOT NULL DEFAULT 'unsigned',
    timestamp    TIMESTAMPTZ NOT NULL,
    -- v33 (v0.7.0 V-4 closeout, #698) — cross-row hash chain columns
    -- mirroring SQLite schema v34. Nullable so the connect-time ALTER
    -- can land without rewriting pre-existing rows; backfill stamps
    -- them in `PostgresStore::migrate_v33`, and the application-layer
    -- `append_signed_event` populates both on every new write.
    prev_hash    BYTEA,
    sequence     BIGINT
);

CREATE INDEX IF NOT EXISTS idx_signed_events_agent     ON signed_events (agent_id);
CREATE INDEX IF NOT EXISTS idx_signed_events_type      ON signed_events (event_type);
CREATE INDEX IF NOT EXISTS idx_signed_events_timestamp ON signed_events (timestamp);
CREATE UNIQUE INDEX IF NOT EXISTS idx_signed_events_sequence ON signed_events (sequence);

-- ─────────────────────────────────────────────────────────────────────
-- signed_events_dlq — deferred-audit drainer dead-letter queue
-- (v0.7.0 Cluster-C SEC-3, issue #767, schema v39 Postgres / v40 SQLite).
--
-- Mirrors `migrations/postgres/0021_v07_signed_events_dlq.sql`. See
-- the SQLite migration in `migrations/sqlite/0034_v07_signed_events_dlq.sql`
-- for the design rationale (failure-split between race-on-UNIQUE
-- requeue and DLQ-land; non-append-only invariant carve-out).

CREATE TABLE IF NOT EXISTS signed_events_dlq (
    dlq_id          BIGSERIAL PRIMARY KEY,
    id              TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    payload_hash    BYTEA NOT NULL,
    signature       BYTEA,
    attest_level    TEXT NOT NULL DEFAULT 'unsigned',
    timestamp       TEXT NOT NULL,
    failure_reason  TEXT NOT NULL,
    failed_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_failed_at
    ON signed_events_dlq(failed_at);
CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_agent
    ON signed_events_dlq(agent_id);

-- ─────────────────────────────────────────────────────────────────────
-- subscription_events / subscription_dlq — A2A correlation IDs, ACK
-- semantics, retry, and dead-letter queue (v0.7.0 K6, schema v27).
--
-- Mirrors `migrations/sqlite/0021_v07_a2a_correlation.sql`. Every
-- outgoing webhook payload is committed to `subscription_events`
-- BEFORE the network send so correlation_id round-trips and
-- replay-from-cursor queries (memory_subscription_replay, K7) have a
-- stable record. `subscription_dlq` holds deliveries that exhausted
-- the three-attempt retry ladder.
--
-- The Postgres adapter uses BIGSERIAL for the autoincrement primary
-- keys (vs SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT`). Both surface
-- as monotonically-increasing i64 values to Rust callers.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS subscription_events (
    id              BIGSERIAL PRIMARY KEY,
    subscription_id TEXT NOT NULL,
    correlation_id  TEXT NOT NULL DEFAULT '',
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    delivered_at    TIMESTAMPTZ NOT NULL,
    delivery_status TEXT NOT NULL DEFAULT 'pending'
);

CREATE INDEX IF NOT EXISTS idx_subscription_events_correlation
    ON subscription_events (correlation_id);
CREATE INDEX IF NOT EXISTS idx_subscription_events_subscription
    ON subscription_events (subscription_id, delivered_at);

CREATE TABLE IF NOT EXISTS subscription_dlq (
    id               BIGSERIAL PRIMARY KEY,
    subscription_id  TEXT NOT NULL,
    correlation_id   TEXT NOT NULL,
    event_type       TEXT NOT NULL,
    payload          JSONB NOT NULL,
    retry_count      INTEGER NOT NULL,
    last_error       TEXT NOT NULL,
    first_failed_at  TIMESTAMPTZ NOT NULL,
    last_failed_at   TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_subscription_dlq_subscription
    ON subscription_dlq (subscription_id, last_failed_at);
CREATE INDEX IF NOT EXISTS idx_subscription_dlq_correlation
    ON subscription_dlq (correlation_id);

-- ─────────────────────────────────────────────────────────────────────
-- agent_quotas — per-agent rate limits + storage caps (v0.7.0 K8,
-- schema v28).
--
-- Mirrors `migrations/sqlite/0022_v07_agent_quotas.sql`. Every
-- registered agent has at most one quota row tracking three rolling-
-- window counters (memories/day, storage bytes lifetime, links/day)
-- against three limits. Daily counters reset at UTC midnight via the
-- K8 sweep loop. The K8 application code currently binds to SQLite;
-- the table is provisioned here so a Postgres bootstrap is one wiring
-- change away from full SAL coverage.
-- ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS agent_quotas (
    agent_id                TEXT PRIMARY KEY,
    max_memories_per_day    BIGINT  NOT NULL DEFAULT 1000,
    max_storage_bytes       BIGINT  NOT NULL DEFAULT 104857600,
    max_links_per_day       BIGINT  NOT NULL DEFAULT 5000,
    current_memories_today  BIGINT  NOT NULL DEFAULT 0,
    current_storage_bytes   BIGINT  NOT NULL DEFAULT 0,
    current_links_today     BIGINT  NOT NULL DEFAULT 0,
    day_started_at          TIMESTAMPTZ NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL,
    updated_at              TIMESTAMPTZ NOT NULL
);

-- agent_id is already the PRIMARY KEY (and thus indexed); the explicit
-- index keeps the K8 status-tool query plan stable across Postgres
-- versions that may treat PK indexes differently in EXPLAIN output —
-- mirrors the SQLite rationale for the same redundant index.
CREATE INDEX IF NOT EXISTS idx_agent_quotas_agent_id
    ON agent_quotas (agent_id);

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
