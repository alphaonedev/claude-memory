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

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::Write;

/// Default zstd compression level. Matches `cli::logs::zstd_compress`
/// for cross-codebase consistency.
const ZSTD_LEVEL: i32 = 3;

/// Lightweight handle for a stored transcript. Does NOT carry the blob
/// itself — callers fetch the decompressed content on demand via
/// [`fetch`]. The [`Transcript`] handle is what insert/list operations
/// return so the (potentially multi-MB) payload doesn't need to flow
/// through every API surface.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub id: String,
    pub namespace: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub compressed_size: i64,
    pub original_size: i64,
}

/// Compress `content` with zstd-3 and write a row to `memory_transcripts`.
///
/// `ttl` is interpreted as a duration from "now"; `None` means no
/// expiry (the row is retained until explicitly deleted by I3's
/// archive-prune sweeper).
///
/// The returned [`Transcript`] handle lets callers persist the id +
/// metadata (e.g. into the I2 join table) without re-reading the row.
///
/// # Errors
///
/// Returns an error if zstd encoding fails (out-of-memory) or the
/// SQLite INSERT fails (disk full, schema mismatch, etc.).
pub fn store(
    conn: &Connection,
    namespace: &str,
    content: &str,
    ttl: Option<Duration>,
) -> Result<Transcript> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();
    let created_at = now.to_rfc3339();
    let expires_at = ttl.map(|d| (now + d).to_rfc3339());

    let original_size =
        i64::try_from(content.len()).context("transcript content length overflows i64")?;
    let blob = zstd_compress(content.as_bytes())
        .context("zstd compression failed for transcript content")?;
    let compressed_size =
        i64::try_from(blob.len()).context("compressed transcript length overflows i64")?;

    conn.execute(
        "INSERT INTO memory_transcripts (
            id, namespace, created_at, expires_at,
            compressed_size, original_size, zstd_level, content_blob
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            namespace,
            created_at,
            expires_at,
            compressed_size,
            original_size,
            ZSTD_LEVEL,
            blob,
        ],
    )
    .context("INSERT into memory_transcripts failed")?;

    Ok(Transcript {
        id,
        namespace: namespace.to_string(),
        created_at,
        expires_at,
        compressed_size,
        original_size,
    })
}

/// Fetch + decompress the transcript identified by `id`. Returns
/// `Ok(None)` when no row matches; callers treat that as "transcript
/// expired or never existed" and surface a structured error upstream.
///
/// # Errors
///
/// Returns an error when the row exists but the blob cannot be
/// decoded (corrupt blob, OOM during decompression) or when the
/// decompressed bytes are not valid UTF-8.
pub fn fetch(conn: &Connection, id: &str) -> Result<Option<String>> {
    let row: Option<Vec<u8>> = conn
        .query_row(
            "SELECT content_blob FROM memory_transcripts WHERE id = ?1",
            params![id],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()
        .context("SELECT memory_transcripts failed")?;

    let Some(blob) = row else {
        return Ok(None);
    };

    let bytes = zstd_decompress(&blob).context("zstd decompression failed")?;
    let text = String::from_utf8(bytes).context("transcript blob did not decode to valid UTF-8")?;
    Ok(Some(text))
}

/// Delete every row whose `expires_at` is in the past (relative to
/// "now"). Returns the number of rows removed.
///
/// I1 only deletes past-expiry rows. The full archive-then-prune
/// lifecycle (separate `archived_transcripts` mirror table, two-stage
/// retention) lands in I3.
///
/// # Errors
///
/// Returns an error when the DELETE fails (e.g. disk write error).
pub fn purge_expired(conn: &Connection) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let n = conn
        .execute(
            "DELETE FROM memory_transcripts
             WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now],
        )
        .context("DELETE expired memory_transcripts failed")?;
    Ok(n)
}

/// v0.7.0 I2 — provenance edge between a memory and a transcript span.
///
/// Establishes that `memory_id` was extracted (or otherwise derived)
/// from the transcript identified by `transcript_id`. The optional
/// (`span_start`, `span_end`) byte offsets address a sub-region of the
/// decompressed transcript; both `None` means "the whole transcript".
/// Offsets are 0-based byte positions into the UTF-8 decompressed
/// bytes, half-open `[start, end)` per the usual Rust slicing
/// convention.
///
/// The PRIMARY KEY on the join table is `(memory_id, transcript_id)`,
/// so a memory can only be linked to a given transcript once. Callers
/// that need to record multiple disjoint spans from the same transcript
/// should merge them into a single bounding pair upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLink {
    pub memory_id: String,
    pub transcript_id: String,
    pub span_start: Option<i64>,
    pub span_end: Option<i64>,
}

/// Insert (or replace) a provenance edge between a memory and a
/// transcript. Both ids must already exist in their respective tables —
/// the foreign keys are enforced (`PRAGMA foreign_keys = ON` is set on
/// every connection opened by [`crate::db::open`]).
///
/// Uses `INSERT OR REPLACE` so re-linking the same `(memory_id,
/// transcript_id)` pair with a different span is a no-fuss update; the
/// I-track currently has no caller that needs to detect the duplicate.
///
/// # Errors
///
/// Returns an error when the INSERT fails — most commonly a foreign-key
/// violation (one of the ids is unknown or has been deleted), or a
/// disk-write failure.
pub fn link_transcript(
    conn: &Connection,
    memory_id: &str,
    transcript_id: &str,
    span_start: Option<i64>,
    span_end: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO memory_transcript_links (
            memory_id, transcript_id, span_start, span_end
         ) VALUES (?1, ?2, ?3, ?4)",
        params![memory_id, transcript_id, span_start, span_end],
    )
    .context("INSERT into memory_transcript_links failed")?;
    Ok(())
}

/// Return every transcript provenance edge for a given memory.
///
/// Order is stable on `transcript_id` so callers (notably I4's
/// `memory_replay`) get a deterministic replay sequence.
///
/// # Errors
///
/// Returns an error when the SELECT or row decoding fails.
pub fn transcripts_for_memory(conn: &Connection, memory_id: &str) -> Result<Vec<TranscriptLink>> {
    let mut stmt = conn
        .prepare(
            "SELECT memory_id, transcript_id, span_start, span_end
             FROM memory_transcript_links
             WHERE memory_id = ?1
             ORDER BY transcript_id",
        )
        .context("PREPARE transcripts_for_memory failed")?;
    let rows = stmt
        .query_map(params![memory_id], row_to_link)
        .context("QUERY transcripts_for_memory failed")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("decode transcripts_for_memory row")?);
    }
    Ok(out)
}

/// Return every memory derived from a given transcript.
///
/// Order is stable on `memory_id` so the fan-in is deterministic for
/// downstream tooling (e.g. archive sweepers in I3).
///
/// # Errors
///
/// Returns an error when the SELECT or row decoding fails.
pub fn memories_for_transcript(
    conn: &Connection,
    transcript_id: &str,
) -> Result<Vec<TranscriptLink>> {
    let mut stmt = conn
        .prepare(
            "SELECT memory_id, transcript_id, span_start, span_end
             FROM memory_transcript_links
             WHERE transcript_id = ?1
             ORDER BY memory_id",
        )
        .context("PREPARE memories_for_transcript failed")?;
    let rows = stmt
        .query_map(params![transcript_id], row_to_link)
        .context("QUERY memories_for_transcript failed")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("decode memories_for_transcript row")?);
    }
    Ok(out)
}

fn row_to_link(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranscriptLink> {
    Ok(TranscriptLink {
        memory_id: row.get(0)?,
        transcript_id: row.get(1)?,
        span_start: row.get(2)?,
        span_end: row.get(3)?,
    })
}

fn zstd_compress(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 + 64);
    {
        let mut encoder = zstd::stream::write::Encoder::new(&mut out, ZSTD_LEVEL)?;
        encoder.write_all(input)?;
        encoder.finish()?;
    }
    Ok(out)
}

fn zstd_decompress(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 4);
    let mut decoder = zstd::stream::read::Decoder::new(input)?;
    std::io::copy(&mut decoder, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

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
            Some(Duration::seconds(-3600)),
        )
        .unwrap();
        // One live row with future TTL.
        let live = store(
            &conn,
            "team/eng",
            "live body",
            Some(Duration::seconds(3600)),
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
}
