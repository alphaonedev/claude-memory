-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 issue #810 / #813 — schema v43 sqlite:
-- atomic `(attest_level, signature)` invariant on `memory_links`.
--
-- # Why
--
-- The H2 link-signing path
-- (`crate::storage::create_link_signed`) is internally atomic: the
-- match arm produces either `(Some(sig), "self_signed",
-- Some(agent_id))` or `(None, "unsigned", None)`, never a mixed
-- tuple. The defect that motivated issue #810, however, is that a
-- caller (or a future code path, or a manual SQL fixup, or a peer
-- replay through `create_link_inbound`) can construct a row whose
-- `attest_level` claims `self_signed` / `peer_attested` but whose
-- `signature` column is NULL or the wrong length. Such rows are
-- "phantom-signed" — they pass the `attest_level == 'self_signed'`
-- filter on `memory_verify` but have no bytes to verify against —
-- and the operator sees a misleading `signature_verified=false`
-- result with no clear cause.
--
-- # The fix
--
-- A pair of SQLite triggers (one for INSERT, one for UPDATE) refuses
-- any row that asserts `attest_level IN ('self_signed', 'peer_attested')`
-- without a 64-byte signature blob (Ed25519 signatures are exactly
-- 64 bytes — see ed25519-dalek's `Signature::to_bytes`). The
-- defensive invariant runs at the substrate layer so EVERY writer
-- (the legitimate H2 signer, federation `create_link_inbound`,
-- direct SQL, future MCP tools, ad-hoc operator UPDATE) sees the
-- same rule.
--
-- # Backfill
--
-- One UPDATE flips any pre-existing phantom row (attest_level says
-- self_signed but signature is NULL or wrong-length) back to
-- 'unsigned'. We cannot reconstruct a signature retroactively; the
-- only correct posture for these rows is to be honest about the
-- absence of bytes.
--
-- # Idempotency
--
-- The triggers use `DROP TRIGGER IF EXISTS` followed by `CREATE
-- TRIGGER` so the migration replay is safe. The backfill UPDATE
-- is idempotent on its predicate (rows that already say `unsigned`
-- are filtered out).
--
-- # Out of scope
--
-- The triggers do NOT police `attest_level = 'unsigned'` against the
-- presence of a signature — a legacy row that carries a stale
-- signature byte string but `unsigned` attest_level is a no-op for
-- the verifier (which only re-derives bytes when the attest_level
-- claims a signature is present). The asymmetric check keeps the
-- migration narrow and surgical to the phantom-self-signed defect.

UPDATE memory_links
SET attest_level = 'unsigned'
WHERE attest_level IN ('self_signed', 'peer_attested')
  AND (signature IS NULL OR length(signature) != 64);

DROP TRIGGER IF EXISTS memory_links_ck_attest_signature_ins;
CREATE TRIGGER memory_links_ck_attest_signature_ins
BEFORE INSERT ON memory_links
FOR EACH ROW
WHEN NEW.attest_level IN ('self_signed', 'peer_attested')
  AND (NEW.signature IS NULL OR length(NEW.signature) != 64)
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: attest_level=self_signed/peer_attested requires 64-byte signature');
END;

DROP TRIGGER IF EXISTS memory_links_ck_attest_signature_upd;
CREATE TRIGGER memory_links_ck_attest_signature_upd
BEFORE UPDATE ON memory_links
FOR EACH ROW
WHEN NEW.attest_level IN ('self_signed', 'peer_attested')
  AND (NEW.signature IS NULL OR length(NEW.signature) != 64)
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: attest_level=self_signed/peer_attested requires 64-byte signature');
END;
