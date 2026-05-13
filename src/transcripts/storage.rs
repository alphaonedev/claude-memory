// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! CRUD, zstd helpers, lifecycle sweep, and data types for `memory_transcripts`.
//!
//! All I1/I2/I3 logic lives here. See [`super`] for the module-level
//! overview of the v0.7.0 I-track substrate.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::{Read, Write};

use crate::config::{ResolvedTranscriptLifecycle, TranscriptsConfig};

/// Default zstd compression level. Matches `cli::logs::zstd_compress`
/// for cross-codebase consistency.
const ZSTD_LEVEL: i32 = 3;

/// v0.7.0 I1 hardening — hard cap on the size of a single decompressed
/// transcript. A pathological zstd blob (e.g. 1 KB compressed → 1 GB
/// decompressed) would otherwise OOM the daemon when [`fetch`] runs.
/// 16 MiB is large enough that no legitimate transcript stored via
/// [`store`] is rejected (the store path itself ingests `&str`, so
/// rows above this ceiling could only have been hand-crafted by a
/// hostile writer with direct DB access). Surfaced as a constant so a
/// downstream operator can audit the boundary in a code review without
/// chasing magic numbers across modules.
pub const MAX_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;

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

/// v0.7.0 I4 — fetch the lightweight metadata for a transcript without
/// pulling the (potentially multi-MB) decompressed content blob.
/// Returns `Ok(None)` when no row matches, mirroring [`fetch`]. The
/// `Transcript` handle carries `created_at`, `compressed_size`, and
/// `original_size`, which I4's `memory_replay` joins with the I2
/// link spans to assemble a per-transcript metadata block.
///
/// # Errors
///
/// Returns an error when the SELECT fails (disk I/O, schema drift).
pub fn fetch_metadata(conn: &Connection, id: &str) -> Result<Option<Transcript>> {
    let row = conn
        .query_row(
            "SELECT id, namespace, created_at, expires_at,
                    compressed_size, original_size
               FROM memory_transcripts WHERE id = ?1",
            params![id],
            |r| {
                Ok(Transcript {
                    id: r.get(0)?,
                    namespace: r.get(1)?,
                    created_at: r.get(2)?,
                    expires_at: r.get(3)?,
                    compressed_size: r.get(4)?,
                    original_size: r.get(5)?,
                })
            },
        )
        .optional()
        .context("SELECT memory_transcripts metadata failed")?;
    Ok(row)
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

/// v0.7.0 I3 — outcome of one [`sweep_transcript_lifecycle`] pass.
///
/// `archived` and `pruned` count distinct rows touched in each phase
/// of the same sweep tick; a row archived this tick will not be
/// pruned until at least the next tick (and only after its grace
/// window expires). `errors` is best-effort observability — the
/// sweeper logs and continues past per-row failures so a single
/// poison row cannot stall the loop, but the count is surfaced for
/// the daemon's structured logs and the future doctor overlay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Number of rows transitioned to `archived` this tick.
    pub archived: usize,
    /// Number of rows hard-deleted (prune phase) this tick.
    pub pruned: usize,
    /// Per-row errors swallowed during the sweep (e.g. a single
    /// transcript with a corrupt namespace string). The aggregate
    /// sweep call still returns `Ok` so the background loop keeps
    /// running.
    pub errors: usize,
}

/// v0.7.0 I3 — drive the transcript archive→prune lifecycle once.
///
/// Two phases run in order against the supplied connection. Per the
/// I3 contract the connection is held for the full sweep so the two
/// phases see a consistent `now`:
///
/// * **Phase 1 — ARCHIVE.** For every live transcript (non-NULL
///   `archived_at` is skipped), resolve the per-namespace lifecycle
///   from `cfg`, then archive the row when:
///   1. `created_at + default_ttl_secs < now` (transcript itself is
///      old enough to retire), and
///   2. every memory linked to the transcript via the I2 join table
///      has either expired or been deleted (a transcript with no
///      linked memories trivially satisfies this — there is nothing
///      keeping it live).
///
/// * **Phase 2 — PRUNE.** Hard-DELETE every archived row whose
///   `archived_at + archive_grace_secs < now`. The
///   `ON DELETE CASCADE` declared on `memory_transcript_links`
///   cleans up the join table without an explicit second statement.
///
/// Phase 2 runs even if Phase 1 had errors so a single poisonous row
/// in the archive scan does not block the prune side. `SweepReport`
/// is the wire-shape returned to the daemon's metrics emitter.
///
/// # Errors
///
/// Returns an error only on infrastructure-level SQLite failures
/// (connection lost, disk full). Per-row failures are folded into
/// `SweepReport::errors`.
pub fn sweep_transcript_lifecycle(
    conn: &Connection,
    cfg: &TranscriptsConfig,
) -> Result<SweepReport> {
    let now = Utc::now();
    let mut report = SweepReport::default();

    // Phase 1 — ARCHIVE.
    archive_phase(conn, cfg, now, &mut report)?;

    // Phase 2 — PRUNE. Each row carries its own `archived_at` so we
    // re-resolve the per-namespace grace window for each candidate.
    prune_phase(conn, cfg, now, &mut report)?;

    Ok(report)
}

/// Phase-1 helper extracted for readability — see
/// [`sweep_transcript_lifecycle`] for the full contract.
fn archive_phase(
    conn: &Connection,
    cfg: &TranscriptsConfig,
    now: DateTime<Utc>,
    report: &mut SweepReport,
) -> Result<()> {
    // Pull every live row up front — the per-namespace TTL means we
    // cannot push the age cutoff into SQL without committing to the
    // global default, and the per-row aliveness check (all linked
    // memories expired) needs another query anyway.
    let live_candidates: Vec<(String, String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, namespace, created_at
                 FROM memory_transcripts
                 WHERE archived_at IS NULL",
            )
            .context("PREPARE archive_phase scan failed")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .context("QUERY archive_phase scan failed")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("decode archive_phase rows")?
    };

    for (id, namespace, created_at) in live_candidates {
        let resolved = cfg.resolve(&namespace);
        match should_archive(conn, &id, &created_at, now, resolved) {
            Ok(true) => {
                let stamp = now.to_rfc3339();
                if let Err(e) = conn.execute(
                    "UPDATE memory_transcripts
                        SET archived_at = ?1
                      WHERE id = ?2 AND archived_at IS NULL",
                    params![stamp, id],
                ) {
                    tracing::warn!(
                        target: "transcripts.lifecycle",
                        "archive UPDATE failed for transcript {id}: {e}"
                    );
                    report.errors += 1;
                } else {
                    report.archived += 1;
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    target: "transcripts.lifecycle",
                    "archive eligibility check failed for transcript {id}: {e}"
                );
                report.errors += 1;
            }
        }
    }
    Ok(())
}

/// Phase-2 helper extracted for readability — see
/// [`sweep_transcript_lifecycle`].
fn prune_phase(
    conn: &Connection,
    cfg: &TranscriptsConfig,
    now: DateTime<Utc>,
    report: &mut SweepReport,
) -> Result<()> {
    let candidates: Vec<(String, String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, namespace, archived_at
                 FROM memory_transcripts
                 WHERE archived_at IS NOT NULL",
            )
            .context("PREPARE prune_phase scan failed")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .context("QUERY prune_phase scan failed")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("decode prune_phase rows")?
    };

    for (id, namespace, archived_at) in candidates {
        let resolved = cfg.resolve(&namespace);
        let archived_at = match DateTime::parse_from_rfc3339(&archived_at) {
            Ok(t) => t.with_timezone(&Utc),
            Err(e) => {
                tracing::warn!(
                    target: "transcripts.lifecycle",
                    "transcript {id} has unparseable archived_at {archived_at:?}: {e}"
                );
                report.errors += 1;
                continue;
            }
        };
        let prune_at = archived_at + Duration::seconds(resolved.archive_grace_secs);
        if prune_at >= now {
            continue;
        }
        match conn.execute("DELETE FROM memory_transcripts WHERE id = ?1", params![id]) {
            Ok(n) => report.pruned += n,
            Err(e) => {
                tracing::warn!(
                    target: "transcripts.lifecycle",
                    "prune DELETE failed for transcript {id}: {e}"
                );
                report.errors += 1;
            }
        }
    }
    Ok(())
}

/// Decide whether a single transcript is archive-eligible at `now`
/// given the resolved [`ResolvedTranscriptLifecycle`].
///
/// Returns `Ok(true)` only when BOTH conditions hold:
///   1. `created_at + default_ttl_secs < now`
///   2. every memory linked via `memory_transcript_links` has
///      `expires_at` in the past, OR no memories link the transcript
///      at all.
///
/// A memory whose `expires_at` is NULL counts as "live forever" and
/// keeps the transcript live too — same as the substrate's
/// [`purge_expired`] semantics for transcript rows themselves.
fn should_archive(
    conn: &Connection,
    transcript_id: &str,
    created_at: &str,
    now: DateTime<Utc>,
    resolved: ResolvedTranscriptLifecycle,
) -> Result<bool> {
    // Age cutoff first — cheaper than the join.
    let created = DateTime::parse_from_rfc3339(created_at)
        .with_context(|| format!("transcript {transcript_id} has unparseable created_at"))?
        .with_timezone(&Utc);
    let archive_at = created + Duration::seconds(resolved.default_ttl_secs);
    if archive_at >= now {
        return Ok(false);
    }

    // Aliveness check: count linked memories with NULL or future
    // `expires_at`. SQLite returns 0 for the COUNT when the join is
    // empty, so a transcript with no links is trivially eligible.
    let now_str = now.to_rfc3339();
    let alive: i64 = conn
        .query_row(
            "SELECT COUNT(*)
               FROM memory_transcript_links l
               JOIN memories m ON m.id = l.memory_id
              WHERE l.transcript_id = ?1
                AND (m.expires_at IS NULL OR m.expires_at > ?2)",
            params![transcript_id, now_str],
            |r| r.get(0),
        )
        .with_context(|| format!("alive-memory count failed for transcript {transcript_id}"))?;
    Ok(alive == 0)
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

/// v0.7.0 I1 hardening — bounded zstd decoder.
///
/// Streams the decoder one fixed-size chunk at a time and bails the
/// moment the accumulated decompressed length would exceed
/// [`MAX_DECOMPRESSED_BYTES`]. Without this cap a hostile writer with
/// direct DB access could ship a small (~1 KB) zstd blob that decodes
/// into gigabytes and OOMs the daemon (a classic decompression bomb).
///
/// On overflow the function returns an error AND emits a structured
/// `tracing::warn!` line under the `transcripts.bomb` target so a
/// downstream audit log captures the rejection without the SQLite
/// row id (the caller does not pass it in here — the surrounding
/// [`fetch`] caller logs the id alongside the bubbled error).
fn zstd_decompress(input: &[u8]) -> Result<Vec<u8>> {
    // Cap the initial allocation too — a blob whose compressed size
    // alone is enormous is itself a smell, but `with_capacity` on a
    // hostile input shouldn't reserve gigabytes upfront either.
    let init_cap = std::cmp::min(input.len() * 4, MAX_DECOMPRESSED_BYTES);
    let mut out = Vec::with_capacity(init_cap);
    let mut decoder = zstd::stream::read::Decoder::new(input)?;
    // 64 KiB read window — large enough to amortise syscall overhead
    // on a normal-sized transcript, small enough to bound the
    // post-overflow drain to a single buffer.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > MAX_DECOMPRESSED_BYTES {
            tracing::warn!(
                target: "transcripts.bomb",
                cap_bytes = MAX_DECOMPRESSED_BYTES,
                so_far = out.len(),
                "rejecting transcript: decompressed size would exceed cap"
            );
            return Err(anyhow!(
                "transcript decompression exceeded {} byte cap (decompression bomb defence)",
                MAX_DECOMPRESSED_BYTES
            ));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

// -----------------------------------------------------------------
// L0.7-2 Tier A — transcripts/storage tests
// All paths exercised over `:memory:` SQLite via `crate::db::open` so
// the daemon's schema is applied. No /tmp writes.
// -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        crate::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn insert_memory(conn: &Connection, id: &str, expires_at: Option<&str>) {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memories (
                id, tier, namespace, title, content, expires_at, created_at, updated_at
             ) VALUES (?1, 'short_term', 'ns', ?2, 'body', ?3, ?4, ?4)",
            rusqlite::params![id, format!("title-{id}"), expires_at, now],
        )
        .expect("insert test memory");
    }

    #[test]
    fn store_and_fetch_round_trips_content() {
        let conn = fresh_db();
        let body = "hello transcripts";
        let t = store(&conn, "ns-x", body, None).expect("store ok");
        assert_eq!(t.namespace, "ns-x");
        assert!(t.compressed_size > 0);
        assert_eq!(t.original_size, body.len() as i64);
        let back = fetch(&conn, &t.id).expect("fetch ok").expect("present");
        assert_eq!(back, body);
    }

    #[test]
    fn store_with_ttl_sets_expires_at() {
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "body", Some(Duration::seconds(120))).expect("store ok");
        assert!(t.expires_at.is_some());
    }

    #[test]
    fn fetch_missing_id_returns_none() {
        let conn = fresh_db();
        let r = fetch(&conn, "no-such-id").expect("query ok");
        assert!(r.is_none());
    }

    #[test]
    fn fetch_metadata_returns_handle_without_blob() {
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "body", None).expect("store ok");
        let meta = fetch_metadata(&conn, &t.id)
            .expect("query ok")
            .expect("present");
        assert_eq!(meta.id, t.id);
        assert_eq!(meta.namespace, "ns-x");
        assert_eq!(meta.original_size, t.original_size);
    }

    #[test]
    fn fetch_metadata_missing_returns_none() {
        let conn = fresh_db();
        let r = fetch_metadata(&conn, "no-such-id").expect("query ok");
        assert!(r.is_none());
    }

    #[test]
    fn purge_expired_removes_only_past_due_rows() {
        let conn = fresh_db();
        // Past: 1 hour ago
        let _live = store(&conn, "ns-x", "live", None).expect("store live");
        // Manually set an expires_at in the past on a second row.
        let past = store(&conn, "ns-x", "past", None).expect("store past");
        conn.execute(
            "UPDATE memory_transcripts SET expires_at = '2000-01-01T00:00:00+00:00' WHERE id = ?1",
            rusqlite::params![past.id],
        )
        .unwrap();
        let n = purge_expired(&conn).expect("purge ok");
        assert_eq!(n, 1, "exactly one past-expiry row");
        assert!(fetch(&conn, &past.id).unwrap().is_none());
    }

    #[test]
    fn link_and_transcripts_for_memory_round_trip() {
        let conn = fresh_db();
        insert_memory(&conn, "m1", None);
        let t = store(&conn, "ns-x", "body", None).expect("store ok");
        link_transcript(&conn, "m1", &t.id, Some(0), Some(4)).expect("link ok");
        let links = transcripts_for_memory(&conn, "m1").expect("query ok");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].memory_id, "m1");
        assert_eq!(links[0].transcript_id, t.id);
        assert_eq!(links[0].span_start, Some(0));
        assert_eq!(links[0].span_end, Some(4));
    }

    #[test]
    fn memories_for_transcript_round_trip() {
        let conn = fresh_db();
        insert_memory(&conn, "m1", None);
        insert_memory(&conn, "m2", None);
        let t = store(&conn, "ns-x", "body", None).expect("store ok");
        link_transcript(&conn, "m1", &t.id, None, None).expect("link ok");
        link_transcript(&conn, "m2", &t.id, None, None).expect("link ok");
        let mems = memories_for_transcript(&conn, &t.id).expect("query ok");
        assert_eq!(mems.len(), 2);
        // Ordered by memory_id alphabetically per the SQL spec
        assert_eq!(mems[0].memory_id, "m1");
        assert_eq!(mems[1].memory_id, "m2");
    }

    #[test]
    fn link_transcript_replaces_on_duplicate_pair() {
        let conn = fresh_db();
        insert_memory(&conn, "m1", None);
        let t = store(&conn, "ns-x", "body", None).expect("store ok");
        link_transcript(&conn, "m1", &t.id, Some(0), Some(4)).expect("link ok");
        // Re-link the same pair with different span — INSERT OR REPLACE.
        link_transcript(&conn, "m1", &t.id, Some(2), Some(10)).expect("relink ok");
        let links = transcripts_for_memory(&conn, "m1").expect("query ok");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].span_start, Some(2));
        assert_eq!(links[0].span_end, Some(10));
    }

    #[test]
    fn sweep_archives_aged_rows_with_no_links() {
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "old", None).expect("store ok");
        // Backdate created_at far enough that the default TTL fires.
        conn.execute(
            "UPDATE memory_transcripts SET created_at = '2000-01-01T00:00:00+00:00' WHERE id = ?1",
            rusqlite::params![t.id],
        )
        .unwrap();
        let cfg = TranscriptsConfig::default();
        let report = sweep_transcript_lifecycle(&conn, &cfg).expect("sweep ok");
        assert!(report.archived >= 1, "expected archive: {report:?}");
    }

    #[test]
    fn sweep_prunes_archived_rows_past_grace() {
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "old", None).expect("store ok");
        // Mark archived a long time ago so the grace window has elapsed.
        conn.execute(
            "UPDATE memory_transcripts SET archived_at = '2000-01-01T00:00:00+00:00' WHERE id = ?1",
            rusqlite::params![t.id],
        )
        .unwrap();
        let cfg = TranscriptsConfig::default();
        let report = sweep_transcript_lifecycle(&conn, &cfg).expect("sweep ok");
        assert_eq!(report.pruned, 1, "expected prune: {report:?}");
        assert!(fetch_metadata(&conn, &t.id).unwrap().is_none());
    }

    #[test]
    fn sweep_skips_live_rows() {
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "fresh body", None).expect("store ok");
        let cfg = TranscriptsConfig::default();
        let report = sweep_transcript_lifecycle(&conn, &cfg).expect("sweep ok");
        // Just created — nothing to archive or prune.
        assert_eq!(report.archived, 0);
        assert_eq!(report.pruned, 0);
        assert!(fetch_metadata(&conn, &t.id).unwrap().is_some());
    }

    #[test]
    fn sweep_skips_archive_when_memory_still_alive() {
        // Phase 1 archive requires every linked memory to have expired.
        // A live memory keeps the transcript alive.
        let conn = fresh_db();
        insert_memory(&conn, "m1", None); // expires_at NULL ⇒ live forever
        let t = store(&conn, "ns-x", "body", None).expect("store ok");
        link_transcript(&conn, "m1", &t.id, None, None).expect("link ok");
        // Age the transcript out.
        conn.execute(
            "UPDATE memory_transcripts SET created_at = '2000-01-01T00:00:00+00:00' WHERE id = ?1",
            rusqlite::params![t.id],
        )
        .unwrap();
        let cfg = TranscriptsConfig::default();
        let report = sweep_transcript_lifecycle(&conn, &cfg).expect("sweep ok");
        // Memory is still live → should_archive returns false → archived 0.
        assert_eq!(
            report.archived, 0,
            "live memory keeps transcript: {report:?}"
        );
    }

    #[test]
    fn sweep_handles_unparseable_archived_at() {
        // Prune phase walks archived rows and tolerates an unparseable
        // archived_at by incrementing the errors counter and skipping.
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "body", None).expect("store ok");
        conn.execute(
            "UPDATE memory_transcripts SET archived_at = 'not-a-date' WHERE id = ?1",
            rusqlite::params![t.id],
        )
        .unwrap();
        let cfg = TranscriptsConfig::default();
        let report = sweep_transcript_lifecycle(&conn, &cfg).expect("sweep ok");
        assert!(report.errors >= 1, "expected error tally: {report:?}");
        assert_eq!(report.pruned, 0, "unparseable row must not be pruned");
    }

    #[test]
    fn should_archive_returns_false_when_within_ttl() {
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "fresh", None).expect("store ok");
        // No backdating — created_at is "now". should_archive must
        // return false on the age cutoff.
        let cfg = TranscriptsConfig::default();
        let resolved = cfg.resolve("ns-x");
        let res = super::should_archive(&conn, &t.id, &t.created_at, Utc::now(), resolved)
            .expect("should_archive ok");
        assert!(!res, "fresh row must not be archive-eligible");
    }

    #[test]
    fn sweep_archive_phase_tallies_should_archive_failure() {
        // Lines 430-435: archive_phase increments errors when
        // should_archive itself returns Err (unparseable created_at on
        // the row).
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "body", None).expect("store ok");
        conn.execute(
            "UPDATE memory_transcripts SET created_at = 'not-a-date' WHERE id = ?1",
            rusqlite::params![t.id],
        )
        .unwrap();
        let cfg = TranscriptsConfig::default();
        let report = sweep_transcript_lifecycle(&conn, &cfg).expect("sweep ok");
        assert!(report.errors >= 1, "expected error tally: {report:?}");
        assert_eq!(report.archived, 0);
    }

    #[test]
    fn should_archive_propagates_unparseable_created_at() {
        let conn = fresh_db();
        let cfg = TranscriptsConfig::default();
        let resolved = cfg.resolve("ns-x");
        let err =
            super::should_archive(&conn, "id", "not-a-date", Utc::now(), resolved).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unparseable created_at"), "got: {msg}");
    }

    #[test]
    fn zstd_round_trip_decodes_to_original() {
        let original = b"some non-trivial bytes \x00\x01\x02 with binary";
        let blob = super::zstd_compress(original).expect("compress");
        let back = super::zstd_decompress(&blob).expect("decompress");
        assert_eq!(back, original);
    }

    #[test]
    fn zstd_decompress_rejects_oversized_blob() {
        // Build a blob that decompresses to > MAX_DECOMPRESSED_BYTES.
        // Cheapest path: compress 17 MiB of zeros — zstd compresses
        // this down to a small blob but decompression must trip the cap.
        let big = vec![0u8; super::MAX_DECOMPRESSED_BYTES + 1024];
        let blob = super::zstd_compress(&big).expect("compress");
        let err = super::zstd_decompress(&blob).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("decompression bomb"), "got: {msg}");
    }

    #[test]
    fn fetch_invalid_utf8_blob_returns_error() {
        // Surgically replace a transcript's content_blob with a valid
        // zstd-of-invalid-utf8 sequence; fetch must surface a
        // "did not decode to valid UTF-8" error.
        let conn = fresh_db();
        let t = store(&conn, "ns-x", "placeholder", None).expect("store");
        let bad_blob = super::zstd_compress(&[0xFF, 0xFE, 0xFD]).expect("compress bad utf8");
        conn.execute(
            "UPDATE memory_transcripts SET content_blob = ?1 WHERE id = ?2",
            rusqlite::params![bad_blob, t.id],
        )
        .unwrap();
        let err = fetch(&conn, &t.id).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("UTF-8") || msg.contains("utf"), "got: {msg}");
    }
}
