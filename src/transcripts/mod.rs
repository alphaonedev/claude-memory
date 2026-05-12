// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 / I-track substrate — compressed transcript storage.
//!
//! `memory_transcripts` is the storage layer for raw conversation
//! transcripts that back the attested-cortex epic. A memory captured by
//! the agent can be linked (via the I2 join table) back to the verbatim
//! source it was extracted from, so re-grounding and replay (I4) become
//! possible without keeping the entire transcript inline on every
//! `memories` row.
//!
//! This module ships only the **storage primitives** for I1: write a
//! transcript, read it back, and purge expired rows. Higher-level
//! lifecycle (archive/prune in I3) and the replay tool (I4) build on
//! top of `store` / `fetch` / `purge_expired`.
//!
//! ## Encoding
//!
//! Content is compressed with `zstd` at level 3, the same level the
//! operational-log archiver in `cli::logs` uses. Level 3 is the zstd
//! default and gives a strong ratio/CPU tradeoff for chat-shaped text
//! (dialogue with repeated speaker tokens, system prompts, and tool
//! calls compresses to roughly 5-10x). The encoding parameter is
//! recorded per-row in `zstd_level` so a future migration that changes
//! the default can still decode legacy rows.
//!
//! ## Sizes
//!
//! `compressed_size` and `original_size` are recorded at write time so
//! `memory_stats` overlays can report compression ratios without
//! decompressing every blob. Both values are derived from the byte
//! length of the encoded / source content respectively.

pub mod replay;
pub mod storage;

pub use storage::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use rusqlite::{Connection, params};

    fn test_db() -> Connection {
        db::open(std::path::Path::new(":memory:")).unwrap()
    }

    /// ~5KB of plausible chat-shaped content. Heavy on repeated speaker
    /// tokens, system-prompt boilerplate, and tool-call envelopes — the
    /// shape that motivates the BLOB+zstd substrate in the first place.
    fn chat_corpus() -> String {
        let mut s = String::new();
        let header = "[system] You are an assistant operating on the alphaonedev/ai-memory-mcp codebase. Always cite tool ids in your replies. Always cite tool ids in your replies.\n";
        let user = "[user] What did we decide about the v0.7 attested-cortex epic last sprint? Please include the full transcript of the relevant meeting.\n";
        let assistant = "[assistant] Per the v0.7 epic doc, the attested-cortex track adds a memory_transcripts table backed by zstd-3 compressed BLOBs. The decision was logged in the meeting transcript on 2026-04-12 at 14:33 UTC.\n";
        let tool_call = "[tool_call name=\"memory_recall\" args={\"query\":\"v0.7 attested cortex\",\"limit\":10,\"namespace\":\"team/eng/memory\"}]\n";
        let tool_result = "[tool_result name=\"memory_recall\" ok=true count=10 latency_ms=42]\n";
        for _ in 0..16 {
            s.push_str(header);
            s.push_str(user);
            s.push_str(assistant);
            s.push_str(tool_call);
            s.push_str(tool_result);
        }
        debug_assert!(s.len() >= 5_000, "corpus too small: {}", s.len());
        s
    }

    #[test]
    fn migration_is_idempotent() {
        // Open the DB twice in succession; the CREATE TABLE IF NOT
        // EXISTS path for memory_transcripts must not error.
        let p = tempfile::NamedTempFile::new().unwrap();
        let path = p.path().to_path_buf();
        let _ = db::open(&path).unwrap();
        let conn = db::open(&path).unwrap();
        // Table is reachable.
        let cnt: i64 = conn
            .query_row("SELECT count(*) FROM memory_transcripts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 0);
    }

    #[test]
    fn round_trip_returns_original_content() {
        let conn = test_db();
        let body = chat_corpus();
        let handle = store(&conn, "team/eng", &body, None).unwrap();
        let got = fetch(&conn, &handle.id).unwrap();
        assert_eq!(got.as_deref(), Some(body.as_str()));
    }

    #[test]
    fn fetch_missing_id_returns_none() {
        let conn = test_db();
        let got = fetch(&conn, "not-a-real-uuid").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn compression_ratio_at_least_5x_on_chat_corpus() {
        let conn = test_db();
        let body = chat_corpus();
        let handle = store(&conn, "team/eng", &body, None).unwrap();
        let ratio = handle.original_size as f64 / handle.compressed_size as f64;
        assert!(
            ratio >= 5.0,
            "expected >=5x zstd-3 ratio on chat-shaped text, got {ratio:.2}x \
             (orig={} compressed={})",
            handle.original_size,
            handle.compressed_size,
        );
        // Sanity: the recorded sizes match the raw-byte facts.
        assert_eq!(handle.original_size, body.len() as i64);
    }

    #[test]
    fn namespace_created_index_exists() {
        let conn = test_db();
        let mut stmt = conn
            .prepare("PRAGMA index_list('memory_transcripts')")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n == "idx_memory_transcripts_namespace_created"),
            "expected idx_memory_transcripts_namespace_created in {names:?}"
        );
    }

    /// v0.7.0 I2 — insert a stub `memories` row so the join-table FK
    /// can be satisfied without dragging in the full `store::create`
    /// pipeline (capabilities, governance, embeddings...). The minimal
    /// column set mirrors the SCHEMA defaults at the top of `db.rs`.
    fn insert_test_memory(conn: &Connection, id: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memories (
                id, tier, namespace, title, content, created_at, updated_at
             ) VALUES (?1, 'short_term', 'team/eng', ?2, 'body', ?3, ?3)",
            params![id, format!("title-{id}"), now],
        )
        .unwrap();
    }

    /// v0.7.0 I2 — round-trip: `link_transcript` writes an edge,
    /// `transcripts_for_memory` reads it back with span fidelity.
    #[test]
    fn i2_link_then_transcripts_for_memory_round_trip() {
        let conn = test_db();
        insert_test_memory(&conn, "mem-1");
        let t = store(&conn, "team/eng", "abcdefghij", None).unwrap();

        link_transcript(&conn, "mem-1", &t.id, Some(2), Some(7)).unwrap();

        let got = transcripts_for_memory(&conn, "mem-1").unwrap();
        assert_eq!(
            got,
            vec![TranscriptLink {
                memory_id: "mem-1".into(),
                transcript_id: t.id.clone(),
                span_start: Some(2),
                span_end: Some(7),
            }],
        );
    }

    /// v0.7.0 I2 — fan-in: a single transcript referenced by multiple
    /// memories. `memories_for_transcript` returns every link, ordered
    /// by `memory_id` for deterministic downstream consumption.
    #[test]
    fn i2_memories_for_transcript_returns_all_linked_memories() {
        let conn = test_db();
        insert_test_memory(&conn, "mem-a");
        insert_test_memory(&conn, "mem-b");
        insert_test_memory(&conn, "mem-c");
        let t = store(&conn, "team/eng", "shared transcript body", None).unwrap();

        link_transcript(&conn, "mem-a", &t.id, None, None).unwrap();
        link_transcript(&conn, "mem-b", &t.id, Some(0), Some(10)).unwrap();
        link_transcript(&conn, "mem-c", &t.id, Some(11), Some(22)).unwrap();

        let got = memories_for_transcript(&conn, &t.id).unwrap();
        let ids: Vec<&str> = got.iter().map(|l| l.memory_id.as_str()).collect();
        assert_eq!(ids, vec!["mem-a", "mem-b", "mem-c"]);
    }

    /// v0.7.0 I2 — `NULL` spans (whole-transcript provenance) survive
    /// the round-trip cleanly. Guards against a future refactor that
    /// might silently coerce them to 0.
    #[test]
    fn i2_null_spans_round_trip_as_none() {
        let conn = test_db();
        insert_test_memory(&conn, "mem-null");
        let t = store(&conn, "team/eng", "body", None).unwrap();

        link_transcript(&conn, "mem-null", &t.id, None, None).unwrap();

        let got = transcripts_for_memory(&conn, "mem-null").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].span_start, None);
        assert_eq!(got[0].span_end, None);
    }

    /// v0.7.0 I2 — deleting a memory must cascade to its provenance
    /// edges. Without this, the join table would accumulate dangling
    /// rows that point at vanished memories and confuse I4's replay.
    #[test]
    fn i2_delete_memory_cascades_to_links() {
        let conn = test_db();
        insert_test_memory(&conn, "mem-doomed");
        insert_test_memory(&conn, "mem-survives");
        let t = store(&conn, "team/eng", "body", None).unwrap();

        link_transcript(&conn, "mem-doomed", &t.id, None, None).unwrap();
        link_transcript(&conn, "mem-survives", &t.id, None, None).unwrap();
        assert_eq!(memories_for_transcript(&conn, &t.id).unwrap().len(), 2);

        conn.execute("DELETE FROM memories WHERE id = ?1", params!["mem-doomed"])
            .unwrap();

        let remaining = memories_for_transcript(&conn, &t.id).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].memory_id, "mem-survives");
    }

    /// v0.7.0 I2 — deleting a transcript must cascade to its links.
    /// I3's archive->prune lifecycle relies on this so callers of
    /// `transcripts_for_memory` never see an id that can no longer be
    /// fetched.
    #[test]
    fn i2_delete_transcript_cascades_to_links() {
        let conn = test_db();
        insert_test_memory(&conn, "mem-x");
        let t = store(&conn, "team/eng", "ephemeral", None).unwrap();

        link_transcript(&conn, "mem-x", &t.id, None, None).unwrap();
        assert_eq!(transcripts_for_memory(&conn, "mem-x").unwrap().len(), 1);

        conn.execute(
            "DELETE FROM memory_transcripts WHERE id = ?1",
            params![t.id],
        )
        .unwrap();

        assert!(transcripts_for_memory(&conn, "mem-x").unwrap().is_empty());
    }

    /// v0.7.0 I2 — the migration is idempotent: opening the same DB
    /// path twice in succession must not error on the join table's
    /// CREATE TABLE / CREATE INDEX statements.
    #[test]
    fn i2_migration_is_idempotent() {
        let p = tempfile::NamedTempFile::new().unwrap();
        let path = p.path().to_path_buf();
        let _ = db::open(&path).unwrap();
        let conn = db::open(&path).unwrap();
        let cnt: i64 = conn
            .query_row("SELECT count(*) FROM memory_transcript_links", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cnt, 0);
    }

    /// v0.7.0 I2 — both supporting indexes are present. Guards against
    /// a future migration that drops the SQL file's `CREATE INDEX` and
    /// silently regresses I4's replay path to a table scan.
    #[test]
    fn i2_join_table_indexes_exist() {
        let conn = test_db();
        let mut stmt = conn
            .prepare("PRAGMA index_list('memory_transcript_links')")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        for expected in ["idx_mtl_transcript", "idx_mtl_memory"] {
            assert!(
                names.iter().any(|n| n == expected),
                "expected {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn purge_expired_only_removes_past_due_rows() {
        let conn = test_db();
        // One expired row.
        let expired = store(
            &conn,
            "team/eng",
            "expired body",
            Some(chrono::Duration::seconds(-3600)),
        )
        .unwrap();
        // One live row with future TTL.
        let live = store(
            &conn,
            "team/eng",
            "live body",
            Some(chrono::Duration::seconds(3600)),
        )
        .unwrap();
        // One row with no TTL — must NOT be purged.
        let immortal = store(&conn, "team/eng", "immortal body", None).unwrap();

        let n = purge_expired(&conn).unwrap();
        assert_eq!(n, 1, "exactly the past-due row should be deleted");

        assert!(fetch(&conn, &expired.id).unwrap().is_none());
        assert_eq!(
            fetch(&conn, &live.id).unwrap().as_deref(),
            Some("live body"),
        );
        assert_eq!(
            fetch(&conn, &immortal.id).unwrap().as_deref(),
            Some("immortal body"),
        );
    }

    // ---------------------------------------------------------------
    // I3 — per-namespace TTL with archive→prune lifecycle.
    // ---------------------------------------------------------------

    use crate::config::{TranscriptNamespaceConfig, TranscriptsConfig};
    use std::collections::HashMap;

    /// Backdate a transcript's `created_at` to `secs` ago. The
    /// `store()` API takes a TTL relative to `now()`, so the only
    /// way to fake an aged row in a test is to UPDATE the column
    /// directly. Returns the rewritten timestamp for assertions.
    fn backdate_created(conn: &Connection, id: &str, secs: i64) -> String {
        let stamp = (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339();
        conn.execute(
            "UPDATE memory_transcripts SET created_at = ?1 WHERE id = ?2",
            params![stamp, id],
        )
        .unwrap();
        stamp
    }

    /// Backdate `archived_at` directly so the prune phase sees a row
    /// that was archived `secs` ago.
    fn backdate_archived(conn: &Connection, id: &str, secs: i64) -> String {
        let stamp = (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339();
        conn.execute(
            "UPDATE memory_transcripts SET archived_at = ?1 WHERE id = ?2",
            params![stamp, id],
        )
        .unwrap();
        stamp
    }

    /// Build a [`TranscriptsConfig`] with a 1-hour global TTL and a
    /// 1-hour grace window — small enough that test-side backdating
    /// can cleanly straddle both thresholds without touching the
    /// system clock.
    fn fast_cfg() -> TranscriptsConfig {
        TranscriptsConfig {
            default_ttl_secs: Some(3600),
            archive_grace_secs: Some(3600),
            namespaces: None,
        }
    }

    /// Read the current `archived_at` for `id`. `None` means the row
    /// is still live (NULL column). Asserts the row exists.
    fn archived_at(conn: &Connection, id: &str) -> Option<String> {
        conn.query_row(
            "SELECT archived_at FROM memory_transcripts WHERE id = ?1",
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    /// Read the `memory_transcripts` row count for `id` — 0 means the
    /// prune phase fired.
    fn row_exists(conn: &Connection, id: &str) -> bool {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_transcripts WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        n > 0
    }

    /// I3 — a transcript with no linked memories AND age beyond the
    /// resolved TTL must be archived in phase 1.
    #[test]
    fn i3_unlinked_aged_transcript_is_archived() {
        let conn = test_db();
        let cfg = fast_cfg();

        let t = store(&conn, "team/eng", "old body", None).unwrap();
        backdate_created(&conn, &t.id, 7200); // 2 h old, TTL = 1 h

        let report = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(report.archived, 1, "phase 1 must archive the aged row");
        assert_eq!(
            report.pruned, 0,
            "phase 2 must not fire on a freshly archived row"
        );
        assert!(
            archived_at(&conn, &t.id).is_some(),
            "archived_at must be set after phase 1",
        );
    }

    /// I3 — phase 2 deletes an archived row whose grace window has
    /// passed. Cascades to the I2 join table for free.
    #[test]
    fn i3_archived_past_grace_is_pruned_with_cascade() {
        let conn = test_db();
        let cfg = fast_cfg();

        insert_test_memory(&conn, "mem-cascade");
        let t = store(&conn, "team/eng", "to be pruned", None).unwrap();
        link_transcript(&conn, "mem-cascade", &t.id, None, None).unwrap();

        // Mark it archived 2 h ago (grace = 1 h).
        backdate_archived(&conn, &t.id, 7200);

        let report = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(report.pruned, 1, "phase 2 must hard-DELETE the row");
        assert!(!row_exists(&conn, &t.id), "transcript row gone");

        // I2 cascade fires: the link row goes too.
        assert!(
            transcripts_for_memory(&conn, "mem-cascade")
                .unwrap()
                .is_empty(),
            "ON DELETE CASCADE must clear the I2 join row",
        );
    }

    /// I3 — a transcript with a still-live linked memory (NULL
    /// `expires_at` or future `expires_at`) is NOT archived even when
    /// the transcript itself is older than its TTL. The memory's
    /// liveness pins the transcript.
    #[test]
    fn i3_live_linked_memory_keeps_transcript_alive() {
        let conn = test_db();
        let cfg = fast_cfg();

        // Memory with no expiry — counts as "live forever".
        insert_test_memory(&conn, "mem-immortal");

        let t = store(&conn, "team/eng", "still wanted", None).unwrap();
        link_transcript(&conn, "mem-immortal", &t.id, None, None).unwrap();
        backdate_created(&conn, &t.id, 7200); // way past TTL

        let report = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(report.archived, 0);
        assert!(
            archived_at(&conn, &t.id).is_none(),
            "live linked memory must keep archived_at NULL",
        );
    }

    /// I3 — a transcript whose every linked memory has an `expires_at`
    /// in the past IS archived. Mirror image of the test above —
    /// guards against the SQL accidentally treating an empty-future
    /// memory as live.
    #[test]
    fn i3_all_linked_memories_expired_then_transcript_is_archived() {
        let conn = test_db();
        let cfg = fast_cfg();

        // Insert a memory then force its expires_at into the past.
        insert_test_memory(&conn, "mem-expired");
        let past = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET expires_at = ?1 WHERE id = 'mem-expired'",
            params![past],
        )
        .unwrap();

        let t = store(&conn, "team/eng", "no longer needed", None).unwrap();
        link_transcript(&conn, "mem-expired", &t.id, None, None).unwrap();
        backdate_created(&conn, &t.id, 7200);

        let report = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(report.archived, 1);
        assert!(archived_at(&conn, &t.id).is_some());
    }

    /// I3 — a per-namespace override (longer TTL) beats the global
    /// default. The global cfg would archive the row; the namespace
    /// override keeps it live.
    #[test]
    fn i3_per_namespace_override_extends_ttl_beyond_global_default() {
        let conn = test_db();

        // Global TTL = 1h, but the team/audit namespace gets 1 day.
        let mut ns_table = HashMap::new();
        ns_table.insert(
            "team/audit".to_string(),
            TranscriptNamespaceConfig {
                default_ttl_secs: Some(86_400),
                archive_grace_secs: None,
                auto_extract: None,
            },
        );
        let cfg = TranscriptsConfig {
            default_ttl_secs: Some(3600),
            archive_grace_secs: Some(3600),
            namespaces: Some(ns_table),
        };

        // Two rows, two namespaces, both 2 h old.
        let eng = store(&conn, "team/eng", "eng body", None).unwrap();
        backdate_created(&conn, &eng.id, 7200);
        let audit = store(&conn, "team/audit", "audit body", None).unwrap();
        backdate_created(&conn, &audit.id, 7200);

        let report = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(report.archived, 1, "only team/eng is past the resolved TTL");
        assert!(archived_at(&conn, &eng.id).is_some());
        assert!(
            archived_at(&conn, &audit.id).is_none(),
            "team/audit override (1d) keeps the audit row live",
        );
    }

    /// I3 — a `prefix/*` per-namespace pattern matches every child
    /// namespace (longest-prefix wins on multiple matches). Guards
    /// the resolver's prefix-walk path.
    #[test]
    fn i3_prefix_pattern_override_matches_child_namespaces() {
        let conn = test_db();

        let mut ns_table = HashMap::new();
        ns_table.insert(
            "ephemeral/*".to_string(),
            TranscriptNamespaceConfig {
                default_ttl_secs: Some(60),
                archive_grace_secs: Some(60),
                auto_extract: None,
            },
        );
        let cfg = TranscriptsConfig {
            default_ttl_secs: Some(86_400 * 30), // 30 days global
            archive_grace_secs: Some(86_400 * 7),
            namespaces: Some(ns_table),
        };

        // 5-min-old row in ephemeral/scratch — past the 60s pattern TTL,
        // well under the 30-day global default.
        let t = store(&conn, "ephemeral/scratch", "scratch", None).unwrap();
        backdate_created(&conn, &t.id, 300);

        let report = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(
            report.archived, 1,
            "prefix pattern must apply to ephemeral/scratch"
        );
    }

    /// I3 — a freshly-archived row (within the grace window) is NOT
    /// pruned. Together with `i3_archived_past_grace_is_pruned`, the
    /// pair brackets the prune-phase boundary condition.
    #[test]
    fn i3_archived_within_grace_is_not_pruned() {
        let conn = test_db();
        let cfg = fast_cfg();

        let t = store(&conn, "team/eng", "still in grace", None).unwrap();
        // Archived 30 minutes ago; grace = 1 h.
        backdate_archived(&conn, &t.id, 1800);

        let report = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(report.pruned, 0);
        assert!(row_exists(&conn, &t.id));
    }

    /// I3 — end-to-end: one sweep archives, a second sweep (after the
    /// grace window) prunes. Documents the two-tick lifecycle the
    /// daemon sweeper relies on.
    #[test]
    fn i3_archive_then_prune_in_two_sweeps() {
        let conn = test_db();
        let cfg = fast_cfg();

        let t = store(&conn, "team/eng", "lifecycle e2e", None).unwrap();
        backdate_created(&conn, &t.id, 7200);

        // Tick 1: archives.
        let r1 = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(r1.archived, 1);
        assert_eq!(r1.pruned, 0);

        // Backdate the archive stamp past the grace window.
        backdate_archived(&conn, &t.id, 7200);

        // Tick 2: prunes.
        let r2 = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(r2.archived, 0);
        assert_eq!(r2.pruned, 1);
        assert!(!row_exists(&conn, &t.id));
    }

    /// I3 — already-archived rows are not re-archived in subsequent
    /// sweeps. Phase-1 SQL filters on `archived_at IS NULL`; this
    /// test pins that filter so a future refactor can't silently
    /// re-stamp archived rows.
    #[test]
    fn i3_idempotent_phase1_does_not_restamp_archived_rows() {
        let conn = test_db();
        let cfg = fast_cfg();

        let t = store(&conn, "team/eng", "already archived", None).unwrap();
        backdate_created(&conn, &t.id, 7200);
        let _ = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        let first_stamp = archived_at(&conn, &t.id).unwrap();

        // Sleep far enough to perceive a clock tick, then re-sweep.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let r2 = sweep_transcript_lifecycle(&conn, &cfg).unwrap();
        assert_eq!(r2.archived, 0, "no row should be re-archived");

        let second_stamp = archived_at(&conn, &t.id).unwrap();
        assert_eq!(
            first_stamp, second_stamp,
            "archived_at must be preserved across sweeps",
        );
    }
}
