-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 issue #810 / #813 — postgres v42:
-- atomic `(attest_level, signature)` invariant on `memory_links`.
-- Mirror of sqlite migration
-- `0037_v07_persona_signing_atomicity.sql`; see that file for the
-- full rationale.
--
-- Postgres supports CHECK constraints inline so we use a single
-- `ADD CONSTRAINT … CHECK` instead of trigger pairs. The check
-- refuses any row that claims `attest_level IN ('self_signed',
-- 'peer_attested')` while `signature` is NULL or wrong-length
-- (Ed25519 signatures are exactly 64 bytes — `bytea` of length
-- != 64 fails).
--
-- # Backfill
--
-- One UPDATE flips any pre-existing phantom row back to 'unsigned'
-- BEFORE the CHECK constraint is added — otherwise the constraint
-- creation would fail with "check constraint is violated by some row"
-- on a legacy DB carrying phantom-signed links.
--
-- # Idempotency
--
-- - `ALTER TABLE … DROP CONSTRAINT IF EXISTS` + `ADD CONSTRAINT` makes
--   the migration replay safe (Postgres has no `ADD CONSTRAINT IF NOT
--   EXISTS` short-form).
-- - The backfill UPDATE matches no rows once already-applied, so
--   re-running is a no-op.

UPDATE memory_links
SET attest_level = 'unsigned'
WHERE attest_level IN ('self_signed', 'peer_attested')
  AND (signature IS NULL OR octet_length(signature) != 64);

ALTER TABLE memory_links
    DROP CONSTRAINT IF EXISTS memory_links_attest_signature_atomic_ck;

ALTER TABLE memory_links
    ADD CONSTRAINT memory_links_attest_signature_atomic_ck
    CHECK (
        attest_level NOT IN ('self_signed', 'peer_attested')
        OR (signature IS NOT NULL AND octet_length(signature) = 64)
    );
