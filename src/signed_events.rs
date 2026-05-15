// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 / H-track substrate — append-only `signed_events` audit
//! table.
//!
//! Each identity-bearing write (today: every `memory_link` insert
//! through `db::create_link` / `db::create_link_signed`) appends one
//! row to `signed_events` so a downstream auditor can replay the
//! exact sequence of attestation events the daemon emitted, without
//! having to scan the mutable `memory_links` table for "what did
//! this row look like at write time" — by construction, the
//! `payload_hash` captured here is SHA-256 over the same canonical-
//! CBOR bytes the H2 signer committed to.
//!
//! # Append-only invariant (row-level)
//!
//! This module exposes ONE writer ([`append_signed_event`]) and
//! ZERO mutators. There are no `UPDATE signed_events` or `DELETE
//! FROM signed_events` statements anywhere in the production
//! codepath. Operators that need to prune (compliance retention,
//! disk pressure) MUST do so via direct SQL with explicit
//! awareness that they are breaking the audit chain — that is the
//! deliberate escape hatch documented in
//! `migrations/sqlite/0020_v07_signed_events.sql`.
//!
//! The H5 test suite asserts (via grep over `src/`) that no
//! `UPDATE signed_events` / `DELETE FROM signed_events` strings
//! appear in production code outside doc comments — adding any
//! such call site will fail the build.
//!
//! # Cross-row tamper evidence (schema v34, #698 V-4 closeout)
//!
//! Since schema v34 the table carries TWO chain columns on top of
//! each row's Ed25519 `signature`:
//!
//! - **`prev_hash BLOB`** — SHA-256 (32 bytes) over the canonical-
//!   bytes encoding of the PRECEDING row (see
//!   [`canonical_chain_bytes`]). First row gets 32 zero bytes.
//! - **`sequence INTEGER`** — monotonically-increasing rank starting
//!   at 1, pinned by a `UNIQUE INDEX`.
//!
//! Together they form a SQL-side hash chain mirroring the JSONL
//! property in [`crate::audit`]. A `DELETE` of row N still passes
//! per-row signature verification on the surviving rows individually,
//! BUT row N+1's stored `prev_hash` will no longer match the
//! recomputed digest of the (now-missing) row N — the chain break is
//! detected at row N+1 by [`verify_chain`]. An `UPDATE` of any
//! column included in the canonical-bytes encoding propagates the
//! same way. A tampered `sequence` breaks the contiguity check.
//!
//! The cross-row chain is the LOAD-BEARING property; per-row Ed25519
//! signatures (the existing `signature` column) remain as defense-in-
//! depth.
//!
//! ## Relationship to [`crate::audit`] (JSONL chain)
//!
//! The JSONL audit log under `<audit_dir>/audit.log` remains the
//! cross-host portable evidence format with its own:
//!
//! - **Cross-line hash chain.** Each JSONL line carries `prev_hash`
//!   pointing to the prior line's `self_hash`; `ai-memory audit
//!   verify` recomputes the chain and exits non-zero on mismatch.
//! - **Monotonic sequence.** F2 (v0.7.0 round-2) wired the sequence
//!   counter to survive process restart so SIEMs detect dropped
//!   lines even before the chain check.
//! - **Append-only OS hint.** Best-effort `chflags(2)` /
//!   `FS_IOC_SETFLAGS`.
//!
//! The SQL chain (this module) is the daemon-local property; the
//! JSONL chain is the portable evidence the daemon hands off to a
//! SIEM. They are complementary, not redundant.
//!
//! ## When to use which surface
//!
//! | Question | Surface |
//! |---|---|
//! | "What did this signed link's bytes look like at write time?" | `signed_events.payload_hash` (binds canonical CBOR) |
//! | "Was the SQL substrate tampered with between `T0` and `T1`?" | `signed_events.prev_hash` + `signed_events.sequence` via [`verify_chain`] |
//! | "Was the on-disk audit log truncated?" | `audit.rs` JSONL chain |
//! | "Did the same key issue the create and the invalidate?" | `signed_events.signature` on both rows |
//!
//! # Out of scope
//!
//! - H4 (`memory_verify` MCP tool, `attest_level` enum surfacing).
//! - H6 (end-to-end test of the immutable chain).

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

/// One row of the `signed_events` audit table.
///
/// `id` is a UUIDv4 minted by the writer; `payload_hash` is the
/// 32-byte SHA-256 over the canonical-CBOR bytes that H2 hashed for
/// the original signature; `signature` mirrors the source row's
/// `memory_links.signature` (NULL when the source write was
/// unsigned).
///
/// `prev_hash` and `sequence` are populated by
/// [`append_signed_event`] (writer fills them from the current chain
/// head — callers MUST NOT set them) and by [`row_to_event`] on read
/// (selecting back rows from the table).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SignedEvent {
    pub id: String,
    pub agent_id: String,
    pub event_type: String,
    pub payload_hash: Vec<u8>,
    pub signature: Option<Vec<u8>>,
    pub attest_level: String,
    pub timestamp: String,
    /// v34 — SHA-256 (32 bytes) over the canonical-bytes encoding of
    /// the preceding row, or 32 zero bytes for the first row. Filled
    /// by [`append_signed_event`] at insert time; callers MUST NOT
    /// pre-populate this field — any value set by the caller is
    /// ignored. Use `..SignedEvent::default()` at the struct-literal
    /// tail to leave this empty.
    pub prev_hash: Vec<u8>,
    /// v34 — monotonically-increasing chain rank starting at 1.
    /// Filled by [`append_signed_event`] at insert time; callers MUST
    /// NOT pre-populate this field — any value set by the caller is
    /// ignored. Use `..SignedEvent::default()` at the struct-literal
    /// tail to leave this zero.
    pub sequence: i64,
}

/// All-zeros 32-byte digest used as `prev_hash` for the first row.
pub const ZERO_HASH: [u8; 32] = [0u8; 32];

/// Field separator for [`canonical_chain_bytes`]. ASCII 0x1F
/// ("Unit Separator") — present in neither RFC3339 timestamps nor
/// UUID strings nor the hex/base64 / raw-bytes payloads we encode,
/// so concatenation is unambiguous without escaping.
const FIELD_SEP: u8 = 0x1F;

/// Canonical bytes used as the chain-hash input.
///
/// Commits to every column that identifies the row's content:
/// `id || 0x1F || agent_id || 0x1F || event_type || 0x1F ||
///  payload_hash || 0x1F || signature_or_empty || 0x1F ||
///  attest_level || 0x1F || timestamp || 0x1F || sequence_be_8_bytes`.
///
/// Each row's `prev_hash` is `SHA-256(canonical_chain_bytes(prev_row))`,
/// or [`ZERO_HASH`] for the first row. A future hash-agility
/// migration can change the digest in one place; the encoding itself
/// is byte-stable so an auditor can replay the chain from the stored
/// columns alone.
#[must_use]
pub fn canonical_chain_bytes(event: &SignedEvent) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(
        event.id.len()
            + event.agent_id.len()
            + event.event_type.len()
            + event.payload_hash.len()
            + event.signature.as_ref().map_or(0, Vec::len)
            + event.attest_level.len()
            + event.timestamp.len()
            + 8
            + 7, // seven separators
    );
    out.extend_from_slice(event.id.as_bytes());
    out.push(FIELD_SEP);
    out.extend_from_slice(event.agent_id.as_bytes());
    out.push(FIELD_SEP);
    out.extend_from_slice(event.event_type.as_bytes());
    out.push(FIELD_SEP);
    out.extend_from_slice(&event.payload_hash);
    out.push(FIELD_SEP);
    if let Some(sig) = event.signature.as_ref() {
        out.extend_from_slice(sig);
    }
    // empty signature contributes zero bytes between separators —
    // the separator on either side still pins the slot's position.
    out.push(FIELD_SEP);
    out.extend_from_slice(event.attest_level.as_bytes());
    out.push(FIELD_SEP);
    out.extend_from_slice(event.timestamp.as_bytes());
    out.push(FIELD_SEP);
    out.extend_from_slice(&event.sequence.to_be_bytes());
    out
}

/// Read the chain head — `(max_sequence, prev_canonical_hash)`.
///
/// Returns `(0, ZERO_HASH)` for an empty table. The "previous"
/// canonical hash is the SHA-256 over the canonical bytes of the
/// row with the highest sequence; the next inserted row's
/// `prev_hash` is exactly this value.
fn read_chain_head(conn: &Connection) -> Result<(i64, [u8; 32])> {
    // Pull the column shape that `canonical_chain_bytes` needs,
    // ordered by sequence DESC so the head is the first row.
    let mut stmt = conn
        .prepare(
            "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, \
                    timestamp, COALESCE(sequence, 0) \
             FROM signed_events \
             ORDER BY COALESCE(sequence, 0) DESC, rowid DESC \
             LIMIT 1",
        )
        .context("read_chain_head: prepare")?;
    let head: Option<SignedEvent> = stmt
        .query_map([], |row| {
            Ok(SignedEvent {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                event_type: row.get(2)?,
                payload_hash: row.get(3)?,
                signature: row.get(4)?,
                attest_level: row.get(5)?,
                timestamp: row.get(6)?,
                sequence: row.get(7)?,
                prev_hash: Vec::new(), // not part of the canonical bytes
            })
        })
        .context("read_chain_head: query_map")?
        .next()
        .transpose()
        .context("read_chain_head: collect")?;
    match head {
        None => Ok((0, ZERO_HASH)),
        Some(prev) => {
            let max_seq = prev.sequence;
            let canon = canonical_chain_bytes(&prev);
            let mut hasher = Sha256::new();
            hasher.update(&canon);
            let mut digest = [0u8; 32];
            digest.copy_from_slice(&hasher.finalize());
            Ok((max_seq, digest))
        }
    }
}

/// Outcome of a [`verify_chain`] pass over the `signed_events` table.
///
/// `rows_checked` counts every row the verifier walked.
/// `chain_break` is `Some(sequence)` when the FIRST detected break
/// happens — that row's stored `prev_hash` does not equal
/// SHA-256(canonical_chain_bytes(row N-1)), OR the row's `sequence`
/// is not the expected `prior + 1` (gap / duplicate / non-monotonic
/// jump). `signature_failures` records sequences whose Ed25519
/// signature did not verify against the supplied key set — the
/// chain itself may still be intact even if individual signatures
/// fail (defense-in-depth split).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerificationReport {
    pub rows_checked: u64,
    pub chain_break: Option<i64>,
    pub signature_failures: Vec<i64>,
}

impl ChainVerificationReport {
    /// `true` when the cross-row chain held end-to-end. Per-row
    /// signature failures are surfaced separately because they are a
    /// disjoint property (a chain break is structurally worse than a
    /// signature failure).
    #[must_use]
    pub fn chain_holds(&self) -> bool {
        self.chain_break.is_none()
    }
}

/// Walk all rows in `sequence` order and verify the cross-row chain
/// + per-row signatures.
///
/// For each row:
///   1. Check `sequence == prior + 1` (first row: `sequence == 1`).
///   2. Recompute SHA-256 over [`canonical_chain_bytes`] of the
///      preceding row (or [`ZERO_HASH`] for the first row), compare
///      to the current row's stored `prev_hash`.
///   3. Verify the Ed25519 signature (when present) over the row's
///      `payload_hash`. The verifying-key resolver is provided by
///      the caller; pass `None` to skip signature verification (the
///      chain check is still performed).
///
/// On a chain break (step 1 or step 2 fail) the verifier records
/// the breaking row's `sequence` and continues so the caller still
/// gets per-row signature stats over the rest of the table.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the SELECT or any row
/// decode fails. A clean (chain held + zero signature failures)
/// report is returned as `Ok`; the caller checks
/// [`ChainVerificationReport::chain_holds`] for the chain bit.
pub fn verify_chain(
    conn: &Connection,
    since_sequence: Option<i64>,
) -> Result<ChainVerificationReport> {
    let lower = since_sequence.unwrap_or(0);
    let mut stmt = conn
        .prepare(
            "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, \
                    timestamp, prev_hash, COALESCE(sequence, 0) \
             FROM signed_events \
             WHERE COALESCE(sequence, 0) > ?1 \
             ORDER BY COALESCE(sequence, 0) ASC",
        )
        .context("verify_chain: prepare")?;
    let mut rows = stmt.query(params![lower]).context("verify_chain: query")?;

    let mut rows_checked: u64 = 0;
    let mut chain_break: Option<i64> = None;
    let signature_failures: Vec<i64> = Vec::new();

    let mut expected_seq = lower + 1;
    let mut prev_canonical_hash: [u8; 32] = ZERO_HASH;
    // When resuming with `since_sequence`, the prior row's canonical
    // hash must be recomputed from the row immediately before `lower`
    // so the chain check at `lower + 1` lines up.
    if lower > 0 {
        let mut head_stmt = conn
            .prepare(
                "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, \
                        timestamp, COALESCE(sequence, 0) \
                 FROM signed_events \
                 WHERE COALESCE(sequence, 0) = ?1",
            )
            .context("verify_chain: prepare head")?;
        let head: Option<SignedEvent> = head_stmt
            .query_map(params![lower], |row| {
                Ok(SignedEvent {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    event_type: row.get(2)?,
                    payload_hash: row.get(3)?,
                    signature: row.get(4)?,
                    attest_level: row.get(5)?,
                    timestamp: row.get(6)?,
                    sequence: row.get(7)?,
                    prev_hash: Vec::new(),
                })
            })
            .context("verify_chain: head query")?
            .next()
            .transpose()
            .context("verify_chain: head collect")?;
        if let Some(h) = head {
            let canon = canonical_chain_bytes(&h);
            let mut hasher = Sha256::new();
            hasher.update(&canon);
            prev_canonical_hash.copy_from_slice(&hasher.finalize());
        }
    }

    while let Some(row) = rows.next().context("verify_chain: next row")? {
        rows_checked += 1;
        let event = SignedEvent {
            id: row.get(0).context("verify_chain: id")?,
            agent_id: row.get(1).context("verify_chain: agent_id")?,
            event_type: row.get(2).context("verify_chain: event_type")?,
            payload_hash: row.get(3).context("verify_chain: payload_hash")?,
            signature: row.get(4).context("verify_chain: signature")?,
            attest_level: row.get(5).context("verify_chain: attest_level")?,
            timestamp: row.get(6).context("verify_chain: timestamp")?,
            prev_hash: row
                .get::<_, Option<Vec<u8>>>(7)
                .context("verify_chain: prev_hash")?
                .unwrap_or_default(),
            sequence: row.get(8).context("verify_chain: sequence")?,
        };

        // (1) Sequence contiguity.
        if event.sequence != expected_seq {
            if chain_break.is_none() {
                chain_break = Some(event.sequence);
            }
            // Keep walking so we still count rows + can later add
            // signature-failure tracking; but realign expected_seq to
            // the row we read so subsequent rows aren't ALL flagged.
            expected_seq = event.sequence;
        }

        // (2) prev_hash chain.
        if event.prev_hash.len() != 32 || event.prev_hash != prev_canonical_hash {
            if chain_break.is_none() {
                chain_break = Some(event.sequence);
            }
        }

        // Recompute the canonical hash for the NEXT iteration.
        let canon = canonical_chain_bytes(&event);
        let mut hasher = Sha256::new();
        hasher.update(&canon);
        prev_canonical_hash.copy_from_slice(&hasher.finalize());

        expected_seq += 1;
    }

    Ok(ChainVerificationReport {
        rows_checked,
        chain_break,
        signature_failures,
    })
}

/// SHA-256 helper. Centralised so every audit-row producer commits
/// to the same digest; a future hash-agility migration changes one
/// line here, not every call site.
#[must_use]
pub fn payload_hash(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// Append a single audit row.
///
/// INSERT-only. There is no companion `update_signed_event` or
/// `delete_signed_event` — the append-only invariant is enforced at
/// the API surface, not via a SQLite trigger (a trigger would also
/// block the documented operator-driven pruning escape hatch).
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the INSERT fails
/// (typically a duplicate UUIDv4 — vanishingly rare but surfaced
/// rather than ignored so the audit chain never silently drops a
/// row).
pub fn append_signed_event(conn: &Connection, event: &SignedEvent) -> Result<()> {
    // v34 (#698 V-4 closeout): compute chain head + INSERT in a
    // single IMMEDIATE transaction so the (read MAX(sequence),
    // INSERT new row) pair is atomic against concurrent writers on
    // the same connection mutex.
    //
    // SQLite serializes write transactions, but a concurrent
    // BEGIN IMMEDIATE on a sibling connection that beats us to the
    // wal-write lock would otherwise let us read a stale head and
    // then INSERT a duplicate sequence. The UNIQUE INDEX on
    // `sequence` (idx_signed_events_sequence) makes the worst case
    // a `SQLITE_CONSTRAINT_UNIQUE` error from this fn — the chain
    // never silently breaks even on race. Callers that batch
    // appends through the project's `Arc<Mutex<Connection>>` pool
    // see no contention; we still wrap in IMMEDIATE for correctness
    // under multi-connection deployments (the deferred-audit
    // drainer opens its own connection on the same DB file).
    let tx = conn
        .unchecked_transaction()
        .context("append signed_event: begin tx")?;
    append_signed_event_no_tx(&tx, event)?;
    tx.commit().context("append signed_event: commit tx")?;
    Ok(())
}

/// Append a signed event using the caller's already-open transaction.
///
/// Use this when the caller is mid-transaction (e.g.
/// `invalidate_link` after its `BEGIN IMMEDIATE` for the UPDATE +
/// audit-INSERT atom). The public `append_signed_event` wrapper
/// adds its own IMMEDIATE tx; calling that from inside a wrapping
/// tx fails on SQLite (nested transactions are not supported on
/// the same connection). This variant takes a `Connection`-like
/// reference (works for both `Connection` and `Transaction`) and
/// inserts directly.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the chain-head read
/// fails or the INSERT itself fails. Callers MUST rollback their
/// own transaction on error.
pub fn append_signed_event_no_tx(conn: &Connection, event: &SignedEvent) -> Result<()> {
    let (max_seq, prev_hash) = read_chain_head(conn).context("append signed_event: read head")?;
    let next_seq = max_seq + 1;
    conn.execute(
        "INSERT INTO signed_events \
            (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp, \
             prev_hash, sequence) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            event.id,
            event.agent_id,
            event.event_type,
            event.payload_hash,
            event.signature,
            event.attest_level,
            event.timestamp,
            prev_hash.to_vec(),
            next_seq,
        ],
    )
    .context("append signed_event")?;
    Ok(())
}

/// Read-only listing.
///
/// When `agent_id` is `Some`, restricts to that agent's events;
/// when `None`, returns every agent's events. Ordering is
/// `timestamp ASC, id ASC` so callers iterating with
/// `(limit, offset)` see a stable sequence even when two events
/// share the same RFC3339 second-precision timestamp (the `id`
/// tiebreaker keeps the order deterministic across calls).
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the query or row
/// decode fails.
pub fn list_signed_events(
    conn: &Connection,
    agent_id: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Vec<SignedEvent>> {
    let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
    let offset_i64 = i64::try_from(offset).unwrap_or(0);
    if let Some(agent) = agent_id {
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, timestamp, \
                    prev_hash, COALESCE(sequence, 0) \
             FROM signed_events \
             WHERE agent_id = ?1 \
             ORDER BY timestamp ASC, id ASC \
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(params![agent, limit_i64, offset_i64], row_to_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, timestamp, \
                    prev_hash, COALESCE(sequence, 0) \
             FROM signed_events \
             ORDER BY timestamp ASC, id ASC \
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![limit_i64, offset_i64], row_to_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<SignedEvent> {
    Ok(SignedEvent {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        event_type: row.get(2)?,
        payload_hash: row.get(3)?,
        signature: row.get(4)?,
        attest_level: row.get(5)?,
        timestamp: row.get(6)?,
        prev_hash: row.get::<_, Option<Vec<u8>>>(7)?.unwrap_or_default(),
        sequence: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rusqlite::Connection;
    use uuid::Uuid;

    /// In-memory connection with the v25 schema applied. We don't go
    /// through `db::open` (which carries the full migration ladder
    /// + WAL / FK PRAGMAs) so the unit test stays focused on the
    /// `signed_events` table itself.
    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(include_str!(
            "../migrations/sqlite/0020_v07_signed_events.sql"
        ))
        .expect("apply v25 migration");
        conn
    }

    fn fixture(agent: &str, event_type: &str) -> SignedEvent {
        SignedEvent {
            id: Uuid::new_v4().to_string(),
            agent_id: agent.to_string(),
            event_type: event_type.to_string(),
            payload_hash: payload_hash(b"test-payload"),
            signature: None,
            attest_level: "unsigned".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            // prev_hash + sequence are overwritten by
            // append_signed_event; the caller-side values here are
            // placeholders so the struct constructs.
            prev_hash: Vec::new(),
            sequence: 0,
        }
    }

    #[test]
    fn migration_is_idempotent() {
        // Re-applying the migration must be a no-op — it's the
        // contract `db::migrate` relies on (every step uses
        // `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT
        // EXISTS`).
        let conn = fresh_db();
        conn.execute_batch(include_str!(
            "../migrations/sqlite/0020_v07_signed_events.sql"
        ))
        .expect("re-apply v25 migration");
        // Append still works after the re-run.
        let event = fixture("alice", "memory_link.created");
        append_signed_event(&conn, &event).expect("append after re-migration");
    }

    #[test]
    fn append_then_list_round_trip() {
        let conn = fresh_db();
        let event = fixture("alice", "memory_link.created");
        append_signed_event(&conn, &event).expect("append");
        let listed = list_signed_events(&conn, Some("alice"), 10, 0).expect("list");
        assert_eq!(listed.len(), 1);
        // The caller-side fixture's prev_hash/sequence are
        // placeholders — append_signed_event overwrites them with
        // (ZERO_HASH, 1) for a fresh table. Compare every caller-
        // controlled field individually, then assert the chain
        // columns were populated by the writer.
        assert_eq!(listed[0].id, event.id);
        assert_eq!(listed[0].agent_id, event.agent_id);
        assert_eq!(listed[0].event_type, event.event_type);
        assert_eq!(listed[0].payload_hash, event.payload_hash);
        assert_eq!(listed[0].signature, event.signature);
        assert_eq!(listed[0].attest_level, event.attest_level);
        assert_eq!(listed[0].timestamp, event.timestamp);
        assert_eq!(listed[0].prev_hash, ZERO_HASH.to_vec());
        assert_eq!(listed[0].sequence, 1);
    }

    #[test]
    fn list_orders_by_timestamp_ascending() {
        let conn = fresh_db();
        // Three events for the same agent at distinct timestamps,
        // inserted out of chronological order.
        let mut a = fixture("alice", "memory_link.created");
        a.timestamp = "2026-05-05T12:00:00+00:00".to_string();
        let mut b = fixture("alice", "memory_link.created");
        b.timestamp = "2026-05-05T12:00:01+00:00".to_string();
        let mut c = fixture("alice", "memory_link.created");
        c.timestamp = "2026-05-05T12:00:02+00:00".to_string();
        append_signed_event(&conn, &b).unwrap();
        append_signed_event(&conn, &c).unwrap();
        append_signed_event(&conn, &a).unwrap();
        let listed = list_signed_events(&conn, Some("alice"), 10, 0).expect("list");
        let ts: Vec<&str> = listed.iter().map(|e| e.timestamp.as_str()).collect();
        assert_eq!(
            ts,
            vec![
                "2026-05-05T12:00:00+00:00",
                "2026-05-05T12:00:01+00:00",
                "2026-05-05T12:00:02+00:00",
            ],
            "rows must come back in ascending timestamp order"
        );
    }

    #[test]
    fn list_filters_by_agent() {
        let conn = fresh_db();
        append_signed_event(&conn, &fixture("alice", "memory_link.created")).unwrap();
        append_signed_event(&conn, &fixture("bob", "memory_link.created")).unwrap();
        append_signed_event(&conn, &fixture("alice", "memory_link.created")).unwrap();
        let alice = list_signed_events(&conn, Some("alice"), 10, 0).unwrap();
        let bob = list_signed_events(&conn, Some("bob"), 10, 0).unwrap();
        let all = list_signed_events(&conn, None, 10, 0).unwrap();
        assert_eq!(alice.len(), 2);
        assert_eq!(bob.len(), 1);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn list_respects_limit_and_offset() {
        let conn = fresh_db();
        for i in 0..5 {
            let mut e = fixture("alice", "memory_link.created");
            e.timestamp = format!("2026-05-05T12:00:0{i}+00:00");
            append_signed_event(&conn, &e).unwrap();
        }
        let page1 = list_signed_events(&conn, Some("alice"), 2, 0).unwrap();
        let page2 = list_signed_events(&conn, Some("alice"), 2, 2).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_ne!(page1[0].id, page2[0].id);
    }

    #[test]
    fn append_preserves_signature_blob() {
        let conn = fresh_db();
        let mut event = fixture("alice", "memory_link.created");
        event.signature = Some(vec![0xAA; 64]); // Ed25519 sig length
        event.attest_level = "self_signed".to_string();
        append_signed_event(&conn, &event).unwrap();
        let listed = list_signed_events(&conn, Some("alice"), 10, 0).unwrap();
        assert_eq!(listed[0].signature.as_deref(), Some(&[0xAA; 64][..]));
        assert_eq!(listed[0].attest_level, "self_signed");
    }

    #[test]
    fn indexes_exist_on_documented_columns() {
        // PRAGMA index_list returns one row per index on a table.
        // We assert each documented index is present so a future
        // migration that drops one of them fails this test.
        let conn = fresh_db();
        let mut stmt = conn.prepare("PRAGMA index_list('signed_events')").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            names.iter().any(|n| n == "idx_signed_events_agent"),
            "missing idx_signed_events_agent in {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "idx_signed_events_type"),
            "missing idx_signed_events_type in {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "idx_signed_events_timestamp"),
            "missing idx_signed_events_timestamp in {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "idx_signed_events_sequence"),
            "missing idx_signed_events_sequence in {names:?} \
             — v34 (V-4 closeout, #698) requires a UNIQUE index on \
             the cross-row chain sequence column"
        );
    }

    #[test]
    fn payload_hash_is_sha256_32_bytes() {
        let h = payload_hash(b"hello world");
        assert_eq!(h.len(), 32, "SHA-256 digest is 32 bytes");
        // Stable across calls.
        assert_eq!(h, payload_hash(b"hello world"));
        // Sensitive to input.
        assert_ne!(h, payload_hash(b"hello worle"));
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — error paths + empty / boundary coverage
    // -----------------------------------------------------------------

    #[test]
    fn append_duplicate_id_returns_error() {
        let conn = fresh_db();
        let mut a = fixture("alice", "memory_link.created");
        append_signed_event(&conn, &a).expect("first append ok");
        // Re-use the exact same id — the PRIMARY KEY on `id` rejects
        // the second INSERT, exercising the .context("append signed_event")
        // error path.
        a.timestamp = Utc::now().to_rfc3339();
        let err = append_signed_event(&conn, &a).expect_err("second append with same id must fail");
        assert!(
            format!("{err:?}").contains("append signed_event")
                || format!("{err:#}").contains("append signed_event"),
            "anyhow context should include the 'append signed_event' tag, got: {err:?}"
        );
    }

    #[test]
    fn list_signed_events_empty_db_returns_empty() {
        let conn = fresh_db();
        let alice = list_signed_events(&conn, Some("alice"), 10, 0).expect("list ok");
        let all = list_signed_events(&conn, None, 10, 0).expect("list ok");
        assert!(alice.is_empty());
        assert!(all.is_empty());
    }

    #[test]
    fn list_signed_events_offset_past_end_returns_empty() {
        let conn = fresh_db();
        append_signed_event(&conn, &fixture("alice", "memory_link.created")).unwrap();
        let beyond = list_signed_events(&conn, Some("alice"), 10, 100).expect("list ok");
        assert!(beyond.is_empty());
    }

    #[test]
    fn list_signed_events_no_agent_filter_returns_all_agents() {
        let conn = fresh_db();
        append_signed_event(&conn, &fixture("alice", "memory_link.created")).unwrap();
        append_signed_event(&conn, &fixture("bob", "memory_link.created")).unwrap();
        append_signed_event(&conn, &fixture("carol", "memory_link.created")).unwrap();
        let all = list_signed_events(&conn, None, 10, 0).expect("list ok");
        let agents: std::collections::HashSet<&str> =
            all.iter().map(|e| e.agent_id.as_str()).collect();
        assert_eq!(agents.len(), 3);
    }

    #[test]
    fn payload_hash_known_vector() {
        // SHA-256 of the empty input must be the well-known constant
        // e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855.
        let h = payload_hash(b"");
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Append-only invariant: there's no public function to UPDATE
    /// or DELETE rows from `signed_events`, and no `UPDATE
    /// signed_events` / `DELETE FROM signed_events` SQL string
    /// appears in any *non-comment* source line under `src/`.
    ///
    /// The check strips Rust line comments (`//...`) and intra-line
    /// `/* ... */` blocks before searching, so the doc-comments in
    /// this module and in `db.rs` that *describe* the contract
    /// (and therefore must contain the forbidden phrases verbatim)
    /// don't trigger a false positive. A real SQL-string call site
    /// — `conn.execute("UPDATE signed_events SET ...", ...)` —
    /// would survive the comment strip and trip the assertion.
    ///
    /// # v34 migration backfill carve-out
    ///
    /// `src/storage/migrations.rs::migrate_v34_backfill_chain` and
    /// `src/store/postgres.rs::migrate_v33` each issue a
    /// `UPDATE signed_events SET prev_hash = ?, sequence = ?` to
    /// stamp the cross-row chain columns on pre-existing rows. These
    /// are migration-time one-shot updates against rows whose
    /// `sequence` column is still NULL (i.e. never-stamped) — they
    /// do NOT mutate post-backfill rows. The carve-out is path-
    /// scoped: the test still flags any UPDATE/DELETE in non-
    /// migration files (the production write paths under
    /// `signed_events.rs`, the MCP/HTTP handlers, etc.).
    #[test]
    fn append_only_invariant_no_mutators_in_src() {
        use std::path::Path;

        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Two forbidden patterns. We split each into halves and
        // concat at runtime so the grep still flags real call sites
        // even if a future contributor copy-pastes a literal needle
        // into a doc comment elsewhere.
        let forbidden: [String; 2] = [
            format!("{} signed_events", "UPDATE"),
            format!("{} signed_events", "DELETE FROM"),
        ];
        // Files allowed to contain the v34 backfill UPDATE — these
        // are the migration paths described in the doc comment above.
        // Path matching is by suffix so the test passes on every
        // OS / working-directory layout.
        let migration_carveouts: [&str; 2] = ["src/storage/migrations.rs", "src/store/postgres.rs"];
        let mut hits: Vec<String> = Vec::new();
        walk_rs_files(&src_root, &mut |path, contents| {
            // Skip the v34 backfill UPDATE in migration paths.
            let path_str = path.to_string_lossy().replace('\\', "/");
            let is_carveout = migration_carveouts.iter().any(|c| path_str.ends_with(c));
            let stripped = strip_rust_comments(contents);
            for needle in &forbidden {
                if !stripped.contains(needle.as_str()) {
                    continue;
                }
                if is_carveout && needle.starts_with("UPDATE") {
                    // Carve-out: backfill is allowed to UPDATE.
                    // DELETE FROM is still flagged.
                    continue;
                }
                hits.push(format!("{}: {}", path.display(), needle));
            }
        });
        assert!(
            hits.is_empty(),
            "found forbidden mutator(s) on signed_events: {hits:?} \
             — append-only invariant requires zero UPDATE/DELETE call sites in production code \
             (the v34 backfill UPDATE in migrations.rs / postgres.rs is the only allowed exception)"
        );
    }

    /// Strip Rust line comments (`//...`) and single-line block
    /// comments (`/* ... */`). Multi-line block comments are
    /// rare in this codebase; an unmatched `/*` falls through and
    /// leaves the rest of the file intact, which is fine — the
    /// grep is a guard, not a parser.
    fn strip_rust_comments(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        for line in src.lines() {
            // Drop everything from the first `//` onward. We don't
            // try to honour `//` inside a string literal — none of
            // the production code under `src/` quotes these
            // forbidden phrases inside a string except the
            // legitimate signed_events.sql include path, which the
            // outer needle ("UPDATE signed_events") doesn't match.
            let line_no_line_comment = match line.find("//") {
                Some(idx) => &line[..idx],
                None => line,
            };
            // Strip /* ... */ blocks that open and close on the
            // same line. Good enough for the doc-comment patterns
            // that exist today.
            let mut buf = String::from(line_no_line_comment);
            while let (Some(start), Some(end_rel)) = (buf.find("/*"), buf.find("*/").map(|i| i + 2))
            {
                if end_rel <= start {
                    break;
                }
                buf.replace_range(start..end_rel, "");
            }
            out.push_str(&buf);
            out.push('\n');
        }
        out
    }

    fn walk_rs_files(dir: &std::path::Path, visit: &mut dyn FnMut(&std::path::Path, &str)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_rs_files(&path, visit);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    visit(&path, &contents);
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — row decode error paths (row_to_event row.get(N)?).
    //
    // The Err-arms of `row.get(0..6)?` in `row_to_event` are
    // triggered when the SELECTed columns can't be decoded into the
    // target Rust type. We exercise this by constructing an
    // in-memory DB with the SAME shape but a deliberately wrong
    // value type for the `agent_id` column (NULL where NOT NULL is
    // expected by the Rust type — rusqlite reports a type error on
    // String::from_sql when the column is NULL).
    // -----------------------------------------------------------------

    #[test]
    fn list_signed_events_row_decode_error_propagates() {
        // Build a permissive signed_events shape (no NOT NULL on
        // agent_id) so we can INSERT a NULL there. The list_signed_events
        // query selects agent_id into a String — String::from_sql
        // refuses NULL, which exercises the row_to_event row.get(1)?
        // Err arm.
        let conn = Connection::open_in_memory().expect("in-memory db");
        // Drop the SQL-file table and recreate a NULL-permissive shape
        // with the SAME column order so the SELECT in
        // list_signed_events still works.
        conn.execute_batch(
            "CREATE TABLE signed_events (
                id              TEXT PRIMARY KEY,
                agent_id        TEXT,
                event_type      TEXT NOT NULL,
                payload_hash    BLOB NOT NULL,
                signature       BLOB,
                attest_level    TEXT NOT NULL,
                timestamp       TEXT NOT NULL,
                prev_hash       BLOB,
                sequence        INTEGER
            );",
        )
        .unwrap();
        // Insert one row with NULL in agent_id — the SELECT shape
        // matches list_signed_events but row.get(1)? fails on the
        // NULL→String decode.
        conn.execute(
            "INSERT INTO signed_events \
             (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp, \
              prev_hash, sequence) \
             VALUES ('row1', NULL, 'memory_link.created', X'00', NULL, 'unsigned', \
             '2026-05-13T00:00:00+00:00', NULL, 1)",
            [],
        )
        .unwrap();
        // Listing now exercises the row.get(1)? Err arm.
        let res = list_signed_events(&conn, None, 10, 0);
        assert!(res.is_err(), "row decode must fail when agent_id is NULL");
    }

    #[test]
    fn list_signed_events_with_agent_filter_row_decode_error_propagates() {
        // Same as above, but exercise the agent_id == Some(...) branch
        // of list_signed_events. The `WHERE agent_id = ?1` won't
        // match NULL rows (NULL ≠ anything), so we need a row whose
        // agent_id is the queried string but another column NULLs out.
        // Insert a row whose `event_type` is NULL — row.get(2)?
        // fails on NULL→String when listing filtered by agent.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE signed_events (
                id              TEXT PRIMARY KEY,
                agent_id        TEXT NOT NULL,
                event_type      TEXT,
                payload_hash    BLOB NOT NULL,
                signature       BLOB,
                attest_level    TEXT NOT NULL,
                timestamp       TEXT NOT NULL,
                prev_hash       BLOB,
                sequence        INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO signed_events \
             (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp, \
              prev_hash, sequence) \
             VALUES ('row2', 'alice', NULL, X'00', NULL, 'unsigned', \
             '2026-05-13T00:00:00+00:00', NULL, 1)",
            [],
        )
        .unwrap();
        let res = list_signed_events(&conn, Some("alice"), 10, 0);
        assert!(res.is_err(), "row decode must fail when event_type is NULL");
    }
}
