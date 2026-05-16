-- v0.7.0 fix campaign R1-M2 (#690) — defense-in-depth CHECK
-- constraints on the substrate.
--
-- Before this migration, every value-domain check on `memories.tier`,
-- `memories.priority`, `memories.confidence`, `memory_links.relation`,
-- and `memory_links.attest_level` lived in `crate::validate` — a Rust-
-- side gate. Direct-SQL writers (debugging, sidecar tooling, the
-- substrate's own `db::insert_if_newer`) could land any string or
-- numeric value the column type accepts. R1-M2 closes that gap by
-- adding SQL-side enforcement so the storage layer refuses bad rows
-- regardless of which language the writer is in.
--
-- IMPLEMENTATION NOTE: SQLite has no `ALTER TABLE ADD CONSTRAINT` for
-- CHECK clauses on existing columns. Two options exist —
--
--   (a) Recreate the table (CREATE NEW + INSERT + DROP OLD + RENAME).
--       This is invasive, requires re-pointing every dependent
--       trigger / FTS index / FK, and is high blast radius on a live
--       deployment.
--
--   (b) Enforce via `CREATE TRIGGER` with `RAISE(ABORT, ...)` on
--       INSERT / UPDATE. Equivalent INSERT-time semantics with zero
--       table churn. Idempotent under `CREATE TRIGGER IF NOT EXISTS`.
--
-- We pick (b). The trigger emits an error message that mirrors the
-- Rust-side validator's vocabulary so a operator who hits a constraint
-- failure sees the same diagnostic shape regardless of whether the
-- write went through the validator first.
--
-- Trigger naming convention: `<table>_ck_<column>_{ins,upd}`.
-- Idempotent: a re-run is a no-op.
--
-- Pre-existing data: triggers fire only on new INSERT / UPDATE writes.
-- Rows that pre-date the trigger and violate the constraint stay where
-- they are — they will surface on the next UPDATE that touches them
-- (caller responsibility to clean up). The `connection::open` helper
-- emits a `tracing::warn!` count of pre-existing violators when it
-- applies this migration so operators are notified loudly.

-- ---------- memories.tier ----------
CREATE TRIGGER IF NOT EXISTS memories_ck_tier_ins
BEFORE INSERT ON memories
FOR EACH ROW
WHEN NEW.tier NOT IN ('short', 'mid', 'long')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memories.tier must be one of short/mid/long');
END;

CREATE TRIGGER IF NOT EXISTS memories_ck_tier_upd
BEFORE UPDATE OF tier ON memories
FOR EACH ROW
WHEN NEW.tier NOT IN ('short', 'mid', 'long')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memories.tier must be one of short/mid/long');
END;

-- ---------- memories.priority ----------
CREATE TRIGGER IF NOT EXISTS memories_ck_priority_ins
BEFORE INSERT ON memories
FOR EACH ROW
WHEN NEW.priority < 1 OR NEW.priority > 10
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memories.priority must be between 1 and 10');
END;

CREATE TRIGGER IF NOT EXISTS memories_ck_priority_upd
BEFORE UPDATE OF priority ON memories
FOR EACH ROW
WHEN NEW.priority < 1 OR NEW.priority > 10
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memories.priority must be between 1 and 10');
END;

-- ---------- memories.confidence ----------
CREATE TRIGGER IF NOT EXISTS memories_ck_confidence_ins
BEFORE INSERT ON memories
FOR EACH ROW
WHEN NEW.confidence < 0.0 OR NEW.confidence > 1.0
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memories.confidence must be between 0.0 and 1.0');
END;

CREATE TRIGGER IF NOT EXISTS memories_ck_confidence_upd
BEFORE UPDATE OF confidence ON memories
FOR EACH ROW
WHEN NEW.confidence < 0.0 OR NEW.confidence > 1.0
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memories.confidence must be between 0.0 and 1.0');
END;

-- ---------- memory_links.relation ----------
-- Closed set must stay byte-identical to
-- `crate::models::MemoryLinkRelation::as_str` and to the Rust
-- validator (`crate::validate::validate_relation`).
--
-- v0.7.0 WT-1-A (schema v35) extended the closed set with
-- `derives_from` (atomisation provenance edges, atom -> parent). The
-- v33 (migration 0027) full-table-rebuild dropped these triggers and
-- promoted the constraint to a column-level CHECK clause, but
-- `connection::open` re-applies this SQL on every fresh open as
-- defense-in-depth (see `apply_check_constraint_triggers`). Keep the
-- closed set here in sync with the column-level CHECK and the
-- Rust-side enum so taxonomy drift surfaces loudly on either side.
CREATE TRIGGER IF NOT EXISTS memory_links_ck_relation_ins
BEFORE INSERT ON memory_links
FOR EACH ROW
WHEN NEW.relation NOT IN ('related_to', 'supersedes', 'contradicts', 'derived_from', 'reflects_on', 'derives_from')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memory_links.relation must be one of related_to/supersedes/contradicts/derived_from/reflects_on/derives_from');
END;

CREATE TRIGGER IF NOT EXISTS memory_links_ck_relation_upd
BEFORE UPDATE OF relation ON memory_links
FOR EACH ROW
WHEN NEW.relation NOT IN ('related_to', 'supersedes', 'contradicts', 'derived_from', 'reflects_on', 'derives_from')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memory_links.relation must be one of related_to/supersedes/contradicts/derived_from/reflects_on/derives_from');
END;

-- ---------- memory_links.attest_level ----------
-- Mirrors `crate::models::AttestLevel::as_str`. NULL is permitted
-- because v0.7 pre-H2 rows landed without the column and the v23
-- migration backfilled "unsigned" via a separate UPDATE that may not
-- have fired on every old row; tolerating NULL keeps the read path
-- compatible without forcing a wide UPDATE on every host.
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
