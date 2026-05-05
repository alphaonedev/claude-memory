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
