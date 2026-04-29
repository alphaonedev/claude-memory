-- v0.6.3 Stream B — Temporal-Validity KG schema additions.
-- Charter §"Critical Schema Reference" (lines 686–723):
-- four temporal columns on `memory_links`, three temporal
-- indexes for KG traversal queries, and an `entity_aliases`
-- side table for the upcoming entity registry. Pure additive.
--
-- NOTE: ALTER TABLE ADD COLUMN statements are omitted here and
-- performed at the Rust layer (db.rs::migrate) with column-existence
-- checks, since SQLite cannot use IF NOT EXISTS for column additions.

-- Backfill valid_from with source memory's created_at
-- Idempotent: only touches NULL rows
UPDATE memory_links
SET valid_from = (SELECT created_at FROM memories WHERE id = memory_links.source_id)
WHERE valid_from IS NULL;

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
    created_at TEXT NOT NULL,
    PRIMARY KEY (entity_id, alias)
);
CREATE INDEX IF NOT EXISTS idx_entity_aliases_alias
    ON entity_aliases (alias);
