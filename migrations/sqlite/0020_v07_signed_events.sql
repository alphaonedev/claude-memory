-- v0.7.0 — Append-only `signed_events` event ledger (Track H, Task H5
-- — schema v25).
--
-- Substrate for the row-level append-only event ledger over
-- identity-bearing writes. Every `memory_link` write (signed or
-- unsigned) appends one row here so a downstream auditor can replay
-- the exact sequence of attestation events the daemon emitted,
-- without having to scan the mutable `memory_links` table for "what
-- did this row look like at write time" — by construction, the
-- canonical-CBOR `payload_hash` captured here is the byte-for-byte
-- input the H2 signer committed to.
--
-- # Append-only invariant (row-level)
--
-- The application layer exposes ONE writer (`append_signed_event`)
-- and ZERO mutators — there are no `UPDATE signed_events` or `DELETE
-- FROM signed_events` statements anywhere in the production code
-- path. Operators that need to prune (compliance retention, disk
-- pressure) MUST do so via direct SQL with explicit awareness that
-- they are breaking the audit chain. This file does NOT add any
-- triggers enforcing append-only at the SQLite layer — SQLite's
-- trigger-based enforcement would also fire against operator-driven
-- pruning, defeating the escape hatch. The contract is enforced at
-- the Rust API surface; the H5 test suite asserts no `UPDATE
-- signed_events` / `DELETE FROM signed_events` strings appear in
-- src/ outside doc comments.
--
-- # Cross-row hash chain (schema v34, #698 V-4 closeout)
--
-- Rows carry two chain columns on top of the per-row signature:
--
-- * `prev_hash BLOB` — SHA-256 (32 bytes) over the canonical-bytes
--   encoding of the PRECEDING row, or 32 zero bytes for the first
--   row. The encoding (`canonical_chain_bytes` in
--   `src/signed_events.rs`) commits to every column that uniquely
--   identifies the row's content. An UPDATE or DELETE of any prior
--   row therefore propagates as a `prev_hash` mismatch at row N+1.
-- * `sequence INTEGER` — monotonically-increasing rank starting at 1,
--   pinned by a UNIQUE index. A tampered or duplicated `sequence`
--   breaks the contiguity check.
--
-- The chain is the LOAD-BEARING tamper-evidence property in the SQL
-- substrate. Per-row Ed25519 signatures (the existing `signature`
-- column) remain as defense-in-depth.
--
-- The JSONL audit log emitted by `src/audit.rs` (`<audit_dir>/
-- audit.log`) remains as the cross-host portable evidence format with
-- its own (1) `prev_hash` chain, (2) restart-stable monotonic
-- sequence (F2, v0.7.0 round-2), and (3) best-effort append-only OS
-- hint. The SQL chain is the daemon-local property; the two are
-- complementary.
--
-- # Columns
--
-- * `id`            — TEXT PRIMARY KEY, UUIDv4 issued by the writer.
-- * `agent_id`      — TEXT, the writer's resolved agent_id at insert
--                     time (NHI provenance — same shape as
--                     `memories.metadata.agent_id`).
-- * `event_type`    — TEXT, dotted event name. v0.7 H5 ships only
--                     `memory_link.created`; future tracks can
--                     extend (`memory_link.invalidated`,
--                     `memory.signed_store`, ...).
-- * `payload_hash`  — BLOB, SHA-256 (32 bytes) over the canonical-
--                     CBOR encoding of the signed event body. For
--                     `memory_link.created` this is the same bytes
--                     H2 hands to Ed25519 — the audit row therefore
--                     binds to exactly what was signed.
-- * `signature`     — BLOB, the Ed25519 signature when the source
--                     write was self-signed; NULL for unsigned
--                     writes. Mirrors `memory_links.signature` so
--                     auditors don't have to join two tables to
--                     reconstruct the signing surface.
-- * `attest_level`  — TEXT, one of `unsigned` / `self_signed` /
--                     `peer_attested` (same enum as H2's
--                     `memory_links.attest_level`). DEFAULT
--                     `'unsigned'` so a row written without an
--                     explicit level is conservative-by-default.
-- * `timestamp`     — TEXT, RFC3339 UTC instant the audit row was
--                     appended (NOT the source row's `created_at` —
--                     they are usually identical but we record the
--                     audit-append time for chain integrity).
--
-- The supporting indexes cover the three documented audit query
-- shapes: "events by this agent", "events of this type", and "events
-- in this time window". A composite index isn't worth the write
-- amplification — append-only tables with three single-column
-- indexes is the same shape as v0.6.4's `audit_log` from G14.

CREATE TABLE IF NOT EXISTS signed_events (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload_hash BLOB NOT NULL,
    signature BLOB,
    attest_level TEXT NOT NULL DEFAULT 'unsigned',
    timestamp TEXT NOT NULL,
    -- v34 (v0.7.0 V-4 closeout, #698) — cross-row hash chain columns.
    -- Nullable here because the migration ALTER cannot retroactively
    -- enforce NOT NULL on pre-existing rows; backfill stamps them in
    -- `migrate_v34_backfill_chain`, and `append_signed_event`
    -- populates both on every new write. See doc block above + the
    -- `signed_events::canonical_chain_bytes` helper for the encoding.
    prev_hash BLOB,
    sequence  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_signed_events_agent
    ON signed_events(agent_id);
CREATE INDEX IF NOT EXISTS idx_signed_events_type
    ON signed_events(event_type);
CREATE INDEX IF NOT EXISTS idx_signed_events_timestamp
    ON signed_events(timestamp);
CREATE UNIQUE INDEX IF NOT EXISTS idx_signed_events_sequence
    ON signed_events(sequence);
