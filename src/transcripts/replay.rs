// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-4 — transcript replay extended to the reflection union.
//!
//! ## What this module owns
//!
//! `memory_replay` (v0.7.0 I4) originally returned the transcripts linked
//! to a *single* memory id via the I2 join table. L2-4 (issue #669)
//! generalises the read: when the memory is a `Reflection`
//! (`memory_kind = 'reflection'`, L1-1), the replay must reconstruct the
//! **union** of every transcript reachable from the reflection by
//! walking `reflects_on` edges backward to the source observations.
//!
//! The walk is BFS over the `reflects_on` adjacency (source_id ->
//! target_id). Each visited memory contributes its own
//! `transcripts_for_memory` rows; the final entry list is deduplicated
//! by transcript id (first-seen wins so the closest ancestor's span
//! metadata is preferred when the same transcript is reachable through
//! more than one path) and sorted by `created_at` ascending. Ties on
//! `created_at` fall back to `transcript_id` so two transcripts minted
//! in the same RFC3339 millisecond still produce a deterministic
//! ordering — same tie-break the I4 handler used.
//!
//! ## Depth contract
//!
//! Callers may cap the BFS at `depth` hops via the `depth` parameter
//! threaded through `memory_replay(depth=N)`. `None` (the default)
//! means "walk the full chain" — every transitively-reachable ancestor.
//! `Some(0)` means "self only" (skip the union; same shape as the
//! pre-L2-4 I4 read). `Some(N>=1)` means "self plus N hops of
//! ancestors". This matches the depth-counting convention used by
//! `reflection_depth` on the memory row.
//!
//! ## Non-Reflection passthrough
//!
//! When the input memory is `MemoryKind::Observation` (or the row
//! cannot be loaded — substrate may have GC'd it between the
//! permission check and now), the walk is skipped entirely and the
//! result is exactly what `transcripts_for_memory` returns for the
//! single memory id. This is the explicit "non-reflection
//! `memory_replay` MUST be unchanged" acceptance criterion from #669.
//!
//! ## Cycle safety
//!
//! L1-2 (#659) already refuses to add a `reflects_on` edge that
//! would close a cycle. The walk here still maintains a `visited`
//! set on `memory_id` so a stale cycle that slipped past the
//! anti-cycle guard (e.g. via direct SQL writes from a legacy
//! migration) cannot induce an infinite loop. Cycle detection is
//! a hard safety net, not a correctness shortcut.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::{HashSet, VecDeque};

use crate::transcripts::storage::{
    Transcript, TranscriptLink, fetch_metadata, transcripts_for_memory,
};

/// One row of the L2-4 union replay stream. Carries both the transcript
/// metadata (compressed/original size, namespace, created_at) and the
/// I2 link span — plus the `memory_id` the link was discovered through,
/// which the I4 handler returns to operators so they can see which
/// ancestor in the chain contributed each transcript.
#[derive(Debug, Clone)]
pub struct ReplayEntry {
    /// Memory id the transcript link is anchored to. For a non-
    /// reflection replay this always equals the input `memory_id`; for
    /// a reflection union it can be any ancestor reachable within
    /// `depth` hops.
    pub memory_id: String,
    /// I2 link row, including the span offsets (may be `None` for
    /// whole-transcript provenance).
    pub link: TranscriptLink,
    /// Transcript metadata. The blob is NOT loaded — the I4 handler
    /// decompresses on demand under the verbose / truncation rule.
    pub meta: Transcript,
}

/// Replay a memory's transcripts. When the memory is a reflection,
/// gather the union of every transcript reachable by walking
/// `reflects_on` edges to `depth` hops.
///
/// `depth = None` walks the full chain; `Some(N)` caps the BFS at N
/// hops from the input memory (a hop is one `reflects_on` edge).
///
/// # Errors
///
/// Returns an error when the underlying SQLite reads fail (disk I/O,
/// schema drift, lock contention). The walk itself is resilient to
/// missing rows — an unreachable id in the chain (pruned by GC, etc.)
/// is silently dropped, the same shape the I4 handler already used for
/// dangling transcripts.
pub fn replay_transcript_union(
    conn: &Connection,
    memory_id: &str,
    depth: Option<u32>,
) -> Result<Vec<ReplayEntry>> {
    // Resolve the root memory's kind. A failed lookup (row missing) is
    // not an error here — the I4 handler decides whether to surface
    // "no transcripts" vs "memory not found"; the substrate read just
    // returns an empty union in that case. `crate::db::get` returns
    // `Ok(None)` cleanly when the id does not exist.
    let kind = match crate::db::get(conn, memory_id)? {
        Some(m) => m.memory_kind,
        None => return Ok(Vec::new()),
    };

    let mut visited: HashSet<String> = HashSet::new();
    let mut frontier: VecDeque<(String, u32)> = VecDeque::new();
    let mut union_memory_ids: Vec<String> = Vec::new();

    visited.insert(memory_id.to_string());
    union_memory_ids.push(memory_id.to_string());
    frontier.push_back((memory_id.to_string(), 0));

    // Only Reflection-kind inputs trigger the BFS over reflects_on.
    // Observations short-circuit to the single-memory read (the
    // pre-L2-4 I4 behaviour) — the acceptance contract on #669 pins
    // "existing memory_replay for non-reflection memories MUST be
    // unchanged". A reflection with `depth=Some(0)` also takes this
    // path (self-only union).
    let walk =
        matches!(kind, crate::models::MemoryKind::Reflection) && depth.is_none_or(|d| d >= 1);

    if walk {
        while let Some((current, hop)) = frontier.pop_front() {
            // Stop expanding once we hit the configured depth cap.
            // `None` (full chain) folds into `is_some_and` returning
            // false so we always expand.
            if depth.is_some_and(|cap| hop >= cap) {
                continue;
            }
            for next in fetch_reflects_on_targets(conn, &current)? {
                if visited.insert(next.clone()) {
                    union_memory_ids.push(next.clone());
                    frontier.push_back((next, hop + 1));
                }
            }
        }
    }

    // Gather every transcript link anchored to any visited memory.
    // Deduplicate by transcript_id — the SAME transcript can be linked
    // to multiple memories (legitimate fan-in: a single conversation
    // produced both an observation and the reflection that summarises
    // it). First-seen wins so the closest ancestor's span metadata is
    // preferred; BFS order means "closest first" matches the
    // chronological intuition of the walk.
    let mut entries: Vec<ReplayEntry> = Vec::new();
    let mut seen_transcripts: HashSet<String> = HashSet::new();
    for mid in &union_memory_ids {
        let links = transcripts_for_memory(conn, mid)
            .with_context(|| format!("transcripts_for_memory({mid}) failed"))?;
        for link in links {
            if !seen_transcripts.insert(link.transcript_id.clone()) {
                continue;
            }
            match fetch_metadata(conn, &link.transcript_id)? {
                Some(meta) => entries.push(ReplayEntry {
                    memory_id: mid.clone(),
                    link,
                    meta,
                }),
                None => {
                    // Transcript pruned out from under us — drop
                    // silently, same shape as the I4 handler.
                    tracing::warn!(
                        target: "transcripts.replay",
                        "dangling transcript_id {} for memory {mid}",
                        link.transcript_id
                    );
                }
            }
        }
    }

    // Chronological sort, with transcript_id as the deterministic
    // tie-breaker (matches the I4 handler's pre-L2-4 ordering).
    entries.sort_by(|a, b| {
        a.meta
            .created_at
            .cmp(&b.meta.created_at)
            .then_with(|| a.meta.id.cmp(&b.meta.id))
    });

    Ok(entries)
}

/// Read every `target_id` reachable from `source_id` via a
/// `reflects_on` edge. Returns ids in `target_id` order so the BFS
/// expansion is deterministic regardless of insertion order at the
/// SQL layer.
fn fetch_reflects_on_targets(conn: &Connection, source_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT target_id FROM memory_links
             WHERE source_id = ?1 AND relation = 'reflects_on'
             ORDER BY target_id",
        )
        .context("PREPARE reflects_on edge scan failed")?;
    let rows = stmt
        .query_map(params![source_id], |r| r.get::<_, String>(0))
        .context("QUERY reflects_on edge scan failed")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("decode reflects_on edge rows")
}

// -----------------------------------------------------------------
// Unit tests — exercise the BFS, depth cap, cycle safety, and the
// non-reflection passthrough on a `:memory:` SQLite with the full
// production schema applied via `crate::db::open`.
// -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcripts::storage::{link_transcript, store};
    use chrono::Utc;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        crate::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    /// Insert a memory row with the given id, namespace, and
    /// `memory_kind`. `created_at` is "now" so the substrate's CHECK
    /// triggers accept it. The fixture keeps the minimal column set —
    /// the replay walker only reads `memory_kind` from the row, plus
    /// the `memory_links` it edges out to.
    fn insert_memory(conn: &Connection, id: &str, namespace: &str, kind: &str) {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memories (
                id, tier, namespace, title, content, created_at, updated_at, memory_kind
             ) VALUES (?1, 'short', ?2, ?3, 'body', ?4, ?4, ?5)",
            rusqlite::params![id, namespace, format!("title-{id}"), now, kind],
        )
        .expect("insert test memory");
    }

    /// Write a `reflects_on` edge in `memory_links`. Minimal column
    /// set — the relation CHECK constraint covers the value, the
    /// `created_at` column is required.
    fn link_reflects_on(conn: &Connection, source: &str, target: &str) {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at, valid_from)
             VALUES (?1, ?2, 'reflects_on', ?3, ?3)",
            rusqlite::params![source, target, now],
        )
        .expect("insert reflects_on link");
    }

    /// Backdate a transcript's `created_at` so the chronological sort
    /// in the union has a deterministic earlier timestamp to anchor on.
    fn backdate_transcript(conn: &Connection, id: &str, ts: &str) {
        conn.execute(
            "UPDATE memory_transcripts SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![ts, id],
        )
        .expect("backdate transcript created_at");
    }

    /// Non-reflection passthrough: an Observation memory with its own
    /// transcripts returns exactly those transcripts, regardless of the
    /// depth parameter. This is the contract pinned by #669:
    /// "Existing memory_replay for non-reflection memories MUST be
    /// unchanged."
    #[test]
    fn observation_returns_only_its_own_transcripts() {
        let conn = fresh_db();
        insert_memory(&conn, "obs-1", "team/eng", "observation");
        let t1 = store(&conn, "team/eng", "body-1", None).unwrap();
        link_transcript(&conn, "obs-1", &t1.id, None, None).unwrap();

        // A sibling memory with its own transcript — must NOT leak
        // into the obs-1 replay even when full-chain depth is set.
        insert_memory(&conn, "obs-2", "team/eng", "observation");
        let t2 = store(&conn, "team/eng", "body-2", None).unwrap();
        link_transcript(&conn, "obs-2", &t2.id, None, None).unwrap();

        let entries = replay_transcript_union(&conn, "obs-1", None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meta.id, t1.id);
        assert_eq!(entries[0].memory_id, "obs-1");

        // depth=Some(N) on an observation must also short-circuit to
        // the single-memory result — the walk only fires for kind=Reflection.
        let entries = replay_transcript_union(&conn, "obs-1", Some(5)).unwrap();
        assert_eq!(entries.len(), 1);
    }

    /// Reflection-union happy path: a reflection with 3 sources, each
    /// with one transcript, plus the reflection's own transcript →
    /// returns the 4-transcript union. Matches the #669 acceptance
    /// fixture verbatim.
    #[test]
    fn reflection_returns_union_of_self_plus_three_sources() {
        let conn = fresh_db();

        // Three observation sources, each with one transcript.
        for (i, ts) in [
            ("obs-a", "2026-01-01T00:00:00Z"),
            ("obs-b", "2026-01-02T00:00:00Z"),
            ("obs-c", "2026-01-03T00:00:00Z"),
        ]
        .iter()
        .enumerate()
        {
            let _ = i;
            insert_memory(&conn, ts.0, "team/eng", "observation");
            let t = store(&conn, "team/eng", &format!("body-{}", ts.0), None).unwrap();
            backdate_transcript(&conn, &t.id, ts.1);
            link_transcript(&conn, ts.0, &t.id, None, None).unwrap();
        }

        // Reflection memory with its own transcript and reflects_on
        // edges to each source.
        insert_memory(&conn, "ref-1", "team/eng", "reflection");
        let t_ref = store(&conn, "team/eng", "reflection-body", None).unwrap();
        backdate_transcript(&conn, &t_ref.id, "2026-01-04T00:00:00Z");
        link_transcript(&conn, "ref-1", &t_ref.id, None, None).unwrap();
        for src in ["obs-a", "obs-b", "obs-c"] {
            link_reflects_on(&conn, "ref-1", src);
        }

        let entries = replay_transcript_union(&conn, "ref-1", None).unwrap();
        assert_eq!(entries.len(), 4, "self + 3 source transcripts");

        // Chronological order pinned by the backdated created_at column.
        let ids: Vec<&str> = entries.iter().map(|e| e.meta.id.as_str()).collect();
        let timestamps: Vec<&str> = entries.iter().map(|e| e.meta.created_at.as_str()).collect();
        assert_eq!(
            timestamps,
            vec![
                "2026-01-01T00:00:00Z",
                "2026-01-02T00:00:00Z",
                "2026-01-03T00:00:00Z",
                "2026-01-04T00:00:00Z",
            ],
            "ascending created_at: {ids:?}"
        );

        // Every source memory appears as the anchor of exactly one
        // entry (the reflection itself anchors the final one).
        let anchor_ids: Vec<&str> = entries.iter().map(|e| e.memory_id.as_str()).collect();
        assert!(anchor_ids.contains(&"obs-a"));
        assert!(anchor_ids.contains(&"obs-b"));
        assert!(anchor_ids.contains(&"obs-c"));
        assert!(anchor_ids.contains(&"ref-1"));
    }

    /// Depth-2 reflection chain: `ref-top -> ref-mid -> obs-leaf`.
    /// `depth = None` (full) returns all three transcripts; `depth =
    /// Some(1)` stops at `ref-mid` (2 transcripts: self + mid);
    /// `depth = Some(0)` returns only the top reflection's own
    /// transcripts. Pins the depth-cap contract from #669.
    #[test]
    fn depth_cap_bounds_chain_walk() {
        let conn = fresh_db();

        insert_memory(&conn, "obs-leaf", "team/eng", "observation");
        let t_leaf = store(&conn, "team/eng", "leaf", None).unwrap();
        backdate_transcript(&conn, &t_leaf.id, "2026-01-01T00:00:00Z");
        link_transcript(&conn, "obs-leaf", &t_leaf.id, None, None).unwrap();

        insert_memory(&conn, "ref-mid", "team/eng", "reflection");
        let t_mid = store(&conn, "team/eng", "mid", None).unwrap();
        backdate_transcript(&conn, &t_mid.id, "2026-01-02T00:00:00Z");
        link_transcript(&conn, "ref-mid", &t_mid.id, None, None).unwrap();
        link_reflects_on(&conn, "ref-mid", "obs-leaf");

        insert_memory(&conn, "ref-top", "team/eng", "reflection");
        let t_top = store(&conn, "team/eng", "top", None).unwrap();
        backdate_transcript(&conn, &t_top.id, "2026-01-03T00:00:00Z");
        link_transcript(&conn, "ref-top", &t_top.id, None, None).unwrap();
        link_reflects_on(&conn, "ref-top", "ref-mid");

        // depth=None: full chain — 3 transcripts.
        let entries = replay_transcript_union(&conn, "ref-top", None).unwrap();
        assert_eq!(entries.len(), 3, "full chain returns all 3 transcripts");

        // depth=Some(1): self + one hop — 2 transcripts (ref-top, ref-mid).
        let entries = replay_transcript_union(&conn, "ref-top", Some(1)).unwrap();
        assert_eq!(entries.len(), 2);
        let ids: Vec<&str> = entries.iter().map(|e| e.meta.id.as_str()).collect();
        assert!(ids.contains(&t_top.id.as_str()));
        assert!(ids.contains(&t_mid.id.as_str()));
        assert!(!ids.contains(&t_leaf.id.as_str()));

        // depth=Some(0): self only — 1 transcript.
        let entries = replay_transcript_union(&conn, "ref-top", Some(0)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meta.id, t_top.id);
    }

    /// Missing root memory: substrate returns an empty union rather
    /// than erroring. The MCP handler layers its own "memory not
    /// found" semantics on top; the substrate read is forgiving.
    #[test]
    fn missing_root_returns_empty_union() {
        let conn = fresh_db();
        let entries = replay_transcript_union(&conn, "does-not-exist", None).unwrap();
        assert!(entries.is_empty());
    }

    /// Cycle safety: a hand-written cycle (`a -> b -> a`) does NOT
    /// loop indefinitely. L1-2 (#659) refuses to add such an edge at
    /// the API layer, but the walker keeps its own visited set as a
    /// defense-in-depth. Directly inserts the cycle via the
    /// `memory_links` table so the test does not depend on bypassing
    /// L1-2's guard.
    #[test]
    fn cycle_in_reflects_on_does_not_loop_forever() {
        let conn = fresh_db();

        insert_memory(&conn, "ref-a", "team/eng", "reflection");
        let t_a = store(&conn, "team/eng", "a", None).unwrap();
        link_transcript(&conn, "ref-a", &t_a.id, None, None).unwrap();

        insert_memory(&conn, "ref-b", "team/eng", "reflection");
        let t_b = store(&conn, "team/eng", "b", None).unwrap();
        link_transcript(&conn, "ref-b", &t_b.id, None, None).unwrap();

        // a → b
        link_reflects_on(&conn, "ref-a", "ref-b");
        // b → a  (the cycle L1-2 normally refuses)
        link_reflects_on(&conn, "ref-b", "ref-a");

        let entries = replay_transcript_union(&conn, "ref-a", None).unwrap();
        // Two distinct transcripts — the cycle does NOT inflate the
        // dedup count.
        assert_eq!(entries.len(), 2);
    }

    /// Transcript shared by two memories in the union appears exactly
    /// once. Mirrors the legitimate fan-in case: a single conversation
    /// produced both an observation and the reflection that summarises
    /// it.
    #[test]
    fn shared_transcript_deduplicates_to_one_entry() {
        let conn = fresh_db();

        insert_memory(&conn, "obs-shared", "team/eng", "observation");
        let t_shared = store(&conn, "team/eng", "shared", None).unwrap();
        link_transcript(&conn, "obs-shared", &t_shared.id, None, None).unwrap();

        insert_memory(&conn, "ref-1", "team/eng", "reflection");
        // Reflection ALSO links the same transcript — fan-in.
        link_transcript(&conn, "ref-1", &t_shared.id, None, None).unwrap();
        link_reflects_on(&conn, "ref-1", "obs-shared");

        let entries = replay_transcript_union(&conn, "ref-1", None).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "dedup keeps a single entry per transcript_id"
        );
        assert_eq!(entries[0].meta.id, t_shared.id);
    }

    /// Reflection with NO `reflects_on` edges (an orphan reflection,
    /// e.g. one whose ancestry was hard-deleted) still returns its
    /// own transcripts. Defends the "self always counts" invariant.
    #[test]
    fn orphan_reflection_returns_only_self() {
        let conn = fresh_db();
        insert_memory(&conn, "ref-orphan", "team/eng", "reflection");
        let t = store(&conn, "team/eng", "orphan", None).unwrap();
        link_transcript(&conn, "ref-orphan", &t.id, None, None).unwrap();

        let entries = replay_transcript_union(&conn, "ref-orphan", None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].memory_id, "ref-orphan");
    }

    /// Dangling transcript_id (link row exists, transcript row was
    /// pruned by I3) is silently dropped rather than surfaced as an
    /// error. Matches the pre-L2-4 I4 handler's tolerance.
    #[test]
    fn dangling_transcript_id_is_silently_dropped() {
        let conn = fresh_db();
        insert_memory(&conn, "obs-1", "team/eng", "observation");
        let t = store(&conn, "team/eng", "body", None).unwrap();
        link_transcript(&conn, "obs-1", &t.id, None, None).unwrap();
        // Hard-delete the transcript row but leave the link.
        conn.execute(
            "DELETE FROM memory_transcripts WHERE id = ?1",
            rusqlite::params![t.id],
        )
        .unwrap();

        let entries = replay_transcript_union(&conn, "obs-1", None).unwrap();
        assert!(entries.is_empty(), "dangling link drops, no error surfaced");
    }
}
