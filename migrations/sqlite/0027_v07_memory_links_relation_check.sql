-- v0.7.0 v0.7.1-fold (#687/#688) — promote `memory_links.relation`
-- validation from RAISE triggers (v23, migration 0023) to a SQL-side
-- CHECK constraint baked into the column definition.
--
-- The v23 triggers (memory_links_ck_relation_ins / _upd) catch INSERT
-- + UPDATE writes and emit a RAISE(ABORT, ...). They are equivalent in
-- INSERT-time semantics to a CHECK clause, but they are NOT visible in
-- `.schema memory_links` output, so dashboards and operators inspecting
-- the substrate's storage shape cannot tell the constraint exists. The
-- v0.7.1 hardening backlog (memory `7b279df3`, decision `65ba07f6`)
-- called for a real CHECK clause so the constraint surfaces in the
-- declared schema. The 2026-05-13 operator directive folds the v0.7.1
-- carry-forward into v0.7.0, which this migration delivers.
--
-- SQLite has no `ALTER TABLE ADD CONSTRAINT CHECK` for an existing
-- column, so we full-table-rebuild:
--   1. CREATE TABLE memory_links_new with the canonical column list +
--      the CHECK clause.
--   2. INSERT INTO memory_links_new SELECT * FROM memory_links.
--   3. DROP the old triggers + indexes.
--   4. DROP TABLE memory_links.
--   5. ALTER TABLE memory_links_new RENAME TO memory_links.
--   6. Recreate indexes (the temporal trio from v15 + the attest_level
--      index from v23).
--   7. The CHECK triggers from v23 (memory_links_ck_relation_*) are
--      dropped in step 3 and NOT recreated — the column-level CHECK
--      clause now enforces the same invariant byte-for-byte (closed
--      taxonomy: related_to / supersedes / contradicts / derived_from
--      / reflects_on). The attest_level triggers are recreated below
--      because we are NOT promoting that column's check this round;
--      that lives at a different storage substrate epoch.
--
-- Closed taxonomy mirrors `crate::validate::VALID_RELATIONS` and
-- `crate::models::MemoryLinkRelation::as_str` exactly. Any drift
-- between those constants and the CHECK clause MUST trigger a
-- coordinated bump (this migration + a new SQL migration to ALTER
-- the table again).
--
-- Pre-existing rows are preserved verbatim — the INSERT SELECT in
-- step 2 will fail on rows that already violate the new CHECK clause.
-- The v23 triggers have been blocking bad writes since v0.7.0 went
-- live, so the only way a violating row exists is direct-SQL hand-
-- editing pre-v23 (extremely rare). If the INSERT fails, operators
-- must clean up offending rows manually before re-running migration.
--
-- NOTE: this migration runs inside the migrate() EXCLUSIVE transaction
-- (see `src/storage/migrations.rs::migrate`). SQLite forbids changing
-- `PRAGMA foreign_keys` inside an open transaction, so we cannot
-- toggle FK enforcement here. That is fine because the rebuild is
-- safe under FK = ON:
--   - No other table FK-references `memory_links` (it has no rows
--     pointing INTO it from elsewhere), so DROP TABLE is allowed.
--   - The `memory_links_new` table declares the same FK clauses to
--     `memories(id)` as the original, so newly-inserted rows are
--     validated identically.
--   - `INSERT INTO memory_links_new SELECT * FROM memory_links` only
--     reads from the old table; FK semantics are unchanged.

-- Step 1: New table with column list verbatim from the current shape
-- (SCHEMA base v0 + v15 temporal columns + v23 attest_level) plus the
-- CHECK clause on `relation`.
CREATE TABLE memory_links_new (
    source_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation     TEXT NOT NULL DEFAULT 'related_to',
    created_at   TEXT NOT NULL,
    valid_from   TEXT,
    valid_until  TEXT,
    observed_by  TEXT,
    signature    BLOB,
    attest_level TEXT,
    PRIMARY KEY (source_id, target_id, relation),
    CHECK (relation IN ('related_to', 'supersedes', 'contradicts', 'derived_from', 'reflects_on'))
);

-- Step 2: Copy rows. INSERT SELECT preserves PRIMARY KEY uniqueness
-- (would fail on duplicate (source_id, target_id, relation) — but the
-- old table already had the same PK so duplicates are impossible) and
-- enforces the new CHECK clause against existing rows.
INSERT INTO memory_links_new (
    source_id, target_id, relation, created_at,
    valid_from, valid_until, observed_by, signature, attest_level
)
SELECT
    source_id, target_id, relation, created_at,
    valid_from, valid_until, observed_by, signature, attest_level
FROM memory_links;

-- Step 3: Drop the old triggers + indexes BEFORE dropping the old
-- table. SQLite drops triggers attached to a table when the table is
-- dropped, but explicit DROP IF EXISTS keeps the intent visible and
-- the migration replayable. Drop only the relation triggers — we
-- recreate the attest_level ones below since the column-level CHECK
-- only covers `relation`.
DROP TRIGGER IF EXISTS memory_links_ck_relation_ins;
DROP TRIGGER IF EXISTS memory_links_ck_relation_upd;
DROP TRIGGER IF EXISTS memory_links_ck_attest_level_ins;
DROP TRIGGER IF EXISTS memory_links_ck_attest_level_upd;

DROP INDEX IF EXISTS idx_links_temporal_src;
DROP INDEX IF EXISTS idx_links_temporal_tgt;
DROP INDEX IF EXISTS idx_links_relation;
DROP INDEX IF EXISTS idx_memory_links_attest_level;

-- Step 4: Drop the old table.
DROP TABLE memory_links;

-- Step 5: Rename the new table into place.
ALTER TABLE memory_links_new RENAME TO memory_links;

-- Step 6: Recreate indexes (idempotent guard preserves replay safety).
CREATE INDEX IF NOT EXISTS idx_links_temporal_src
    ON memory_links (source_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt
    ON memory_links (target_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_relation
    ON memory_links (relation, valid_from);
CREATE INDEX IF NOT EXISTS idx_memory_links_attest_level
    ON memory_links (attest_level, created_at);

-- Step 7: Recreate the attest_level enforcement triggers (column-
-- level CHECK only covers `relation` this round).
CREATE TRIGGER IF NOT EXISTS memory_links_ck_attest_level_ins
BEFORE INSERT ON memory_links
FOR EACH ROW
WHEN NEW.attest_level IS NOT NULL
  AND NEW.attest_level NOT IN ('unsigned', 'self_signed', 'peer_attested')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memory_links.attest_level must be one of unsigned/self_signed/peer_attested (or NULL for legacy rows)');
END;

CREATE TRIGGER IF NOT EXISTS memory_links_ck_attest_level_upd
BEFORE UPDATE OF attest_level ON memory_links
FOR EACH ROW
WHEN NEW.attest_level IS NOT NULL
  AND NEW.attest_level NOT IN ('unsigned', 'self_signed', 'peer_attested')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memory_links.attest_level must be one of unsigned/self_signed/peer_attested (or NULL for legacy rows)');
END;
