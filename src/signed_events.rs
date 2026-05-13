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
//! # Append-only invariant
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEvent {
    pub id: String,
    pub agent_id: String,
    pub event_type: String,
    pub payload_hash: Vec<u8>,
    pub signature: Option<Vec<u8>>,
    pub attest_level: String,
    pub timestamp: String,
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
    conn.execute(
        "INSERT INTO signed_events \
            (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            event.id,
            event.agent_id,
            event.event_type,
            event.payload_hash,
            event.signature,
            event.attest_level,
            event.timestamp,
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
            "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, timestamp \
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
            "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, timestamp \
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
        assert_eq!(listed[0], event);
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
        let mut hits: Vec<String> = Vec::new();
        walk_rs_files(&src_root, &mut |path, contents| {
            let stripped = strip_rust_comments(contents);
            for needle in &forbidden {
                if stripped.contains(needle.as_str()) {
                    hits.push(format!("{}: {}", path.display(), needle));
                }
            }
        });
        assert!(
            hits.is_empty(),
            "found forbidden mutator(s) on signed_events: {hits:?} \
             — append-only invariant requires zero UPDATE/DELETE call sites in production code"
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
                timestamp       TEXT NOT NULL
            );",
        )
        .unwrap();
        // Insert one row with NULL in agent_id — the SELECT shape
        // matches list_signed_events but row.get(1)? fails on the
        // NULL→String decode.
        conn.execute(
            "INSERT INTO signed_events \
             (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp) \
             VALUES ('row1', NULL, 'memory_link.created', X'00', NULL, 'unsigned', \
             '2026-05-13T00:00:00+00:00')",
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
                timestamp       TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO signed_events \
             (id, agent_id, event_type, payload_hash, signature, attest_level, timestamp) \
             VALUES ('row2', 'alice', NULL, X'00', NULL, 'unsigned', \
             '2026-05-13T00:00:00+00:00')",
            [],
        )
        .unwrap();
        let res = list_signed_events(&conn, Some("alice"), 10, 0);
        assert!(res.is_err(), "row decode must fail when event_type is NULL");
    }
}
