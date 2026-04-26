-- v0.6.3 Stream B — Temporal-Validity KG schema additions (Postgres dialect).
-- Mirror of migrations/sqlite/0010_v063_hierarchy_kg.sql with Postgres-specific
-- types: TIMESTAMPTZ for temporal columns, BYTEA for signatures.
--
-- Charter §"Critical Schema Reference":
-- four temporal columns on `memory_links`, three temporal indexes for KG
-- traversal queries, and an `entity_aliases` side table for the upcoming
-- entity registry. Pure additive.

-- ALTER TABLE memory_links — add temporal columns (Postgres allows IF NOT EXISTS)
ALTER TABLE memory_links
    ADD COLUMN IF NOT EXISTS valid_from TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS valid_until TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS observed_by TEXT,
    ADD COLUMN IF NOT EXISTS signature BYTEA;

-- Backfill valid_from with source memory's created_at
-- Idempotent: only touches NULL rows
UPDATE memory_links
SET valid_from = memories.created_at
FROM memories
WHERE memory_links.source_id = memories.id
  AND memory_links.valid_from IS NULL;

-- Create temporal indexes for KG traversal queries (idempotent)
CREATE INDEX IF NOT EXISTS idx_links_temporal_src
    ON memory_links (source_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt
    ON memory_links (target_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_relation
    ON memory_links (relation, valid_from);

-- entity_aliases — alias→entity_id resolution (v0.6.3 Stream B/C)
CREATE TABLE IF NOT EXISTS entity_aliases (
    entity_id  TEXT NOT NULL,
    alias      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (entity_id, alias)
);
CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
    ON entity_aliases (alias);
