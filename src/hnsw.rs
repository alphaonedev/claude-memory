// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HNSW (Hierarchical Navigable Small World) vector index for fast approximate
//! nearest-neighbor search over memory embeddings.
//!
//! Built on `instant-distance`. The index is constructed at startup from all
//! stored embeddings. New memories added during the session go into an overflow
//! list that is scanned linearly alongside the HNSW results — the index is
//! rebuilt lazily once the overflow exceeds a threshold.

use instant_distance::{Builder, HnswMap, Point, Search};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use crate::hooks::EvictionEvent;

/// Maximum overflow entries before triggering a rebuild.
const REBUILD_THRESHOLD: usize = 200;

/// Maximum entries before evicting oldest to prevent unbounded memory growth.
///
/// Production code uses the constant 100_000. Tests may construct a
/// `VectorIndex` with a custom cap via [`VectorIndex::with_max_entries_for_test`]
/// — that knob is stored on the index instance itself, so it does
/// NOT affect concurrent tests running with the default cap. The
/// constant lives here so call sites (and the per-event tracing
/// payload) reference one canonical value.
const MAX_ENTRIES: usize = 100_000;

// ---------------------------------------------------------------------------
// v0.6.3.1 (P3, G2): eviction observability
//
// `MAX_ENTRIES`-triggered eviction in `insert()` previously dropped the
// oldest embeddings silently — operators near the cap lost recall quality
// invisibly. The two counters below + the structured `hnsw.eviction`
// tracing event close that gap:
//
//   - `INDEX_EVICTIONS_TOTAL` — cumulative count surfaced via
//     `db::stats().index_evictions_total` (and capabilities).
//   - `LAST_EVICTION_AT_NANOS` — wall-clock UNIX nanoseconds of the most
//     recent eviction; capabilities derive `hnsw.evicted_recently` from
//     this with a 60 s rolling window.
//
// Process-local. The counters reset on restart because the index itself
// resets on restart. Both atomics are touched only on the eviction edge
// (rare: requires >100k vectors), so there is no measurable hot-path cost.
// ---------------------------------------------------------------------------

static INDEX_EVICTIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static LAST_EVICTION_AT_NANOS: AtomicU64 = AtomicU64::new(0);

/// Cumulative HNSW oldest-eviction count since process start.
///
/// Surfaces in `memory_stats`. Non-zero indicates the in-memory vector
/// index has hit `MAX_ENTRIES` and dropped older embeddings; recall
/// quality may have degraded for evicted ids until they are re-inserted
/// (e.g. on next access via `recall` touch path).
#[must_use]
pub fn index_evictions_total() -> u64 {
    INDEX_EVICTIONS_TOTAL.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// M8 (v0.7.0 round-2) — eviction-rate observability.
//
// Operators who hit the 100k cap need two signals:
//
//   1. Per-eviction WARN — surface every eviction event so operators
//      see drift before recall quality has noticeably degraded.
//   2. Rolling-rate ERROR — when the trailing-hour eviction rate
//      exceeds the M8 ceiling, escalate to ERROR so the ops dashboard
//      raises a page. The escalation message names the operator
//      knobs (`vector_index_capacity` / "move to dedicated vector DB")
//      so the on-call has the remediation in the log line.
//
// Implementation: a small fixed-size ring buffer of UNIX-nanosecond
// timestamps. Each eviction `push`es a stamp; the rolling-rate check
// counts how many stamps sit inside the trailing-hour window. The
// ring is locked behind a `Mutex` for write-coherent visibility; the
// path runs only on the eviction edge so the lock cost is negligible.
// ---------------------------------------------------------------------------

/// M8 eviction-rate ceiling: events / hour past which the rolling
/// observer escalates from WARN to ERROR.
const EVICTION_RATE_CEILING_PER_HOUR: usize = 10;

/// Rolling-hour ring buffer capacity. Chosen so the ring can hold the
/// ceiling plus headroom for burstiness; older entries are
/// transparently evicted on push.
const EVICTION_RATE_RING_CAP: usize = 64;

static EVICTION_RATE_RING: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Whether an eviction occurred within the trailing `window_secs`.
///
/// Used by capabilities (P1) to set `hnsw.evicted_recently` so operators
/// can see ongoing pressure on the cap, not just the cumulative count.
/// Returns `false` when no evictions have ever happened in this process.
#[must_use]
pub fn evicted_recently(window_secs: u64) -> bool {
    let last = LAST_EVICTION_AT_NANOS.load(Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Saturating math: clock can move backwards on some VMs.
    let elapsed_nanos = u128::from(u64::MAX).min(now_nanos.saturating_sub(u128::from(last)));
    elapsed_nanos < u128::from(window_secs).saturating_mul(1_000_000_000)
}

/// Reset the eviction counters. Test-only — production callers must not
/// reach into the counter directly. The function is `pub` (rather than
/// `pub(crate)`) so the integration-test crate at `tests/` can drive it
/// alongside the public `index_evictions_total()` accessor; renaming
/// keeps the intent obvious at every call site.
#[doc(hidden)]
pub fn reset_eviction_counters_for_test() {
    INDEX_EVICTIONS_TOTAL.store(0, Ordering::Relaxed);
    LAST_EVICTION_AT_NANOS.store(0, Ordering::Relaxed);
    if let Ok(mut g) = EVICTION_RATE_RING.lock() {
        g.clear();
    }
}

/// M8 (v0.7.0 round-2) — push the latest eviction timestamp into the
/// rolling-hour ring and return how many stamps now sit inside the
/// trailing hour. Producers call this once per eviction event;
/// the caller branches on the returned count to escalate from WARN
/// (already emitted) to ERROR.
fn record_eviction_and_count_recent(now_nanos: u64) -> usize {
    const ONE_HOUR_NANOS: u64 = 3_600 * 1_000_000_000;
    let cutoff = now_nanos.saturating_sub(ONE_HOUR_NANOS);
    let Ok(mut ring) = EVICTION_RATE_RING.lock() else {
        // Poisoned lock — observability is best-effort, return 0 so
        // the caller does not over-escalate.
        return 0;
    };
    // Drop stale entries first so the ring stays bounded and the
    // count reflects the trailing hour.
    ring.retain(|t| *t >= cutoff);
    if ring.len() >= EVICTION_RATE_RING_CAP {
        // Cap reached — drop the oldest before appending.
        ring.remove(0);
    }
    ring.push(now_nanos);
    ring.len()
}

/// A point in the HNSW index — wraps a dense embedding vector.
#[derive(Clone, Debug)]
pub struct EmbeddingPoint(pub Vec<f32>);

impl instant_distance::Point for EmbeddingPoint {
    fn distance(&self, other: &Self) -> f32 {
        // Cosine distance = 1 - cosine_similarity.
        // Embeddings are L2-normalised so dot product = cosine similarity.
        let dot: f32 = self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum();
        1.0 - dot
    }
}

/// Thread-safe HNSW index over memory embeddings.
pub struct VectorIndex {
    /// The built HNSW index — maps embedding points to memory IDs.
    inner: Mutex<IndexState>,
    /// v0.7.0 (R3-S1) — eviction sink. The `MAX_ENTRIES`-triggered
    /// drain in `insert()` pushes an [`EvictionEvent`] onto this
    /// channel for each evicted id; a hook-aware observer above this
    /// layer drains the channel and fires the `on_index_eviction`
    /// chain off the hot path. Wired by the daemon at startup
    /// (`daemon_runtime`) via [`Self::set_eviction_sink`]. Optional —
    /// CLI / test builds that never bring up the hooks pipeline leave
    /// it `None` and the sink-push is a no-op so eviction throughput
    /// is unaffected. Closes the G2 / G8 "fire site exists but not
    /// wired" gap that the prior `tracing::warn!`-only implementation
    /// left open.
    ///
    /// `Mutex` (not `RwLock`) because writes happen exactly twice in
    /// the process lifetime (`set_eviction_sink` at startup and
    /// `Drop`) and reads happen only on the eviction edge which is
    /// itself already serialized through `inner`. The non-blocking
    /// `try_send` semantics on the channel make sink-push safe to
    /// hold across the inner-state lock without risk of deadlock.
    eviction_sink: Mutex<Option<Sender<EvictionEvent>>>,
}

struct IndexState {
    hnsw: Option<HnswMap<EmbeddingPoint, String>>,
    /// Entries added after the last rebuild. Searched linearly.
    overflow: Vec<(String, Vec<f32>)>,
    /// All entries (for rebuild). Kept in sync with the index + overflow.
    all_entries: Vec<(String, Vec<f32>)>,
    /// v0.7.0 R3-S1 — per-instance eviction cap. Defaults to
    /// [`MAX_ENTRIES`] (the production 100k). Tests construct an
    /// index with a smaller cap via
    /// [`VectorIndex::with_max_entries_for_test`] so the eviction
    /// edge can be exercised without inserting 100k vectors. Storing
    /// the cap per-instance (rather than as a process-wide atomic)
    /// keeps concurrent tests independent.
    max_entries: usize,
}

/// A search result from the vector index.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: String,
    pub distance: f32,
}

impl VectorIndex {
    /// Build a new index from a list of (`memory_id`, embedding) pairs.
    pub fn build(entries: Vec<(String, Vec<f32>)>) -> Self {
        let hnsw = Self::build_hnsw(&entries);
        VectorIndex {
            inner: Mutex::new(IndexState {
                hnsw,
                overflow: Vec::new(),
                all_entries: entries,
                max_entries: MAX_ENTRIES,
            }),
            eviction_sink: Mutex::new(None),
        }
    }

    /// Build an empty index.
    pub fn empty() -> Self {
        VectorIndex {
            inner: Mutex::new(IndexState {
                hnsw: None,
                overflow: Vec::new(),
                all_entries: Vec::new(),
                max_entries: MAX_ENTRIES,
            }),
            eviction_sink: Mutex::new(None),
        }
    }

    /// v0.7.0 R3-S1 — Build an empty index with a custom eviction
    /// cap. Test-only: lets a 5-entry insert sequence exercise the
    /// eviction edge in milliseconds (vs. the ~minute-scale cost of
    /// inserting 100k vectors at the production cap). The knob is
    /// stored per-instance so concurrent tests using the default
    /// cap are unaffected.
    #[doc(hidden)]
    #[must_use]
    pub fn with_max_entries_for_test(max_entries: usize) -> Self {
        VectorIndex {
            inner: Mutex::new(IndexState {
                hnsw: None,
                overflow: Vec::new(),
                all_entries: Vec::new(),
                max_entries,
            }),
            eviction_sink: Mutex::new(None),
        }
    }

    /// v0.7.0 (R3-S1) — wire the eviction sink.
    ///
    /// The daemon calls this once at startup with the send-half of an
    /// mpsc channel; a hook-aware observer task drains the recv-half
    /// off the hot path and fires the `on_index_eviction` chain
    /// (`fire_on_index_eviction` in `src/hooks/chain.rs`). Replacing
    /// an existing sink is allowed — useful when the daemon
    /// reconfigures the hook chain at runtime — and drops the prior
    /// sender, which terminates the prior observer cleanly.
    ///
    /// Build-time / CLI / test builds that never wire a sink retain
    /// the `None` default; the eviction path's `try_send` then
    /// becomes a no-op short-circuit so there is no measurable cost
    /// to leaving the sink unset.
    pub fn set_eviction_sink(&self, sink: Sender<EvictionEvent>) {
        if let Ok(mut guard) = self.eviction_sink.lock() {
            *guard = Some(sink);
        }
    }

    fn build_hnsw(entries: &[(String, Vec<f32>)]) -> Option<HnswMap<EmbeddingPoint, String>> {
        if entries.is_empty() {
            return None;
        }
        let points: Vec<EmbeddingPoint> = entries
            .iter()
            .map(|(_, emb)| EmbeddingPoint(emb.clone()))
            .collect();
        let values: Vec<String> = entries.iter().map(|(id, _)| id.clone()).collect();
        Some(Builder::default().build(points, values))
    }

    /// Add a new entry to the index (goes to overflow until next rebuild).
    pub fn insert(&self, id: String, embedding: Vec<f32>) {
        let mut state = match self.inner.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.all_entries.push((id.clone(), embedding.clone()));
        state.overflow.push((id, embedding));

        // Auto-rebuild if overflow is large
        if state.overflow.len() >= REBUILD_THRESHOLD {
            state.hnsw = Self::build_hnsw(&state.all_entries);
            state.overflow.clear();
        }

        // Evict oldest entries if over capacity
        let max_entries = state.max_entries;
        if state.all_entries.len() > max_entries {
            let excess = state.all_entries.len() - max_entries;
            // M8 (v0.7.0 round-2) — emit ONE summary WARN per eviction
            // event so the operator sees the batch drop in the daemon
            // log without scrolling past N per-id lines first. The
            // per-id WARNs (below) still fire for post-mortem
            // attribution; this one is the high-level "the index
            // dropped N oldest embeddings" signal operators alert on.
            tracing::warn!(
                target: "hnsw.eviction",
                dropped = excess,
                max_entries = max_entries,
                "HNSW eviction: dropped {} oldest embeddings to make room",
                excess,
            );
            // v0.7.0 (R3-S1) — fire the `on_index_eviction` hook event
            // for each evicted id BEFORE we drop the rows. The sink
            // is a non-blocking `try_send` (see below); a downstream
            // hook-aware observer drains the channel off the hot path
            // and invokes `crate::hooks::fire_on_index_eviction` per
            // event. This closes the G2/G8 "fire site exists but not
            // wired" gap that the prior `tracing::warn!`-only
            // implementation left open.
            //
            // The sink push happens INSIDE the inner-state lock — the
            // channel is unbounded so `try_send`-equivalent `send`
            // never blocks (unbounded mpsc has no backpressure). The
            // sink lock is independent of the inner lock so there is
            // no ordering hazard.
            //
            // The hook subscriber (if any) is responsible for its own
            // logging; the warn-level tracing event is preserved here
            // as a no-op-when-no-subscriber fallback so operators
            // without hooks configured still see eviction pressure in
            // daemon logs, matching the v0.6.3.1 observability contract.
            let sink_guard = self.eviction_sink.lock().ok();
            for (evicted_id, _) in state.all_entries.iter().take(excess) {
                tracing::warn!(
                    target: "hnsw.eviction",
                    evicted_id = %evicted_id,
                    reason = "max_entries_reached",
                    max_entries = max_entries,
                    "hnsw index evicting oldest entry: cap reached"
                );
                if let Some(sink) = sink_guard.as_ref().and_then(|g| g.as_ref()) {
                    // mpsc::Sender::send is non-blocking on an unbounded
                    // channel (it only blocks on bounded). Errors mean the
                    // receiver dropped — observability is best-effort, no
                    // recovery action needed.
                    let payload = EvictionEvent::new(
                        evicted_id.clone(),
                        String::new(), // namespace not in scope at hnsw layer
                        "max_entries_reached",
                    );
                    let _ = sink.send(payload);
                }
            }
            drop(sink_guard);
            #[allow(clippy::cast_possible_truncation)]
            let evicted = excess as u64;
            INDEX_EVICTIONS_TOTAL.fetch_add(evicted, Ordering::Relaxed);

            state.all_entries.drain(..excess);
            state.hnsw = Self::build_hnsw(&state.all_entries);
            state.overflow.clear();

            // Record completion time AFTER the rebuild. `evicted_recently` is
            // a "did we evict in the trailing N seconds" check; an operator
            // asking that wants the operation completion time, not the
            // start. At cap, build_hnsw dominates wall time (~minutes at
            // 100k entries) — using the start would make evicted_recently
            // misreport even immediately after insert returns.
            let now_nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let now_nanos_u64 = u64::try_from(now_nanos).unwrap_or(u64::MAX);
            LAST_EVICTION_AT_NANOS.store(now_nanos_u64, Ordering::Relaxed);

            // M8 (v0.7.0 round-2) — rolling-hour rate observer. Push
            // a stamp on this eviction, then count stamps in the
            // trailing hour. If the rate clears the M8 ceiling,
            // escalate to ERROR so the dashboard pages the on-call.
            let recent = record_eviction_and_count_recent(now_nanos_u64);
            if recent > EVICTION_RATE_CEILING_PER_HOUR {
                tracing::error!(
                    target: "hnsw.eviction",
                    rate_per_hour = recent,
                    ceiling = EVICTION_RATE_CEILING_PER_HOUR,
                    "HNSW eviction rate exceeded {}/hour — recall quality is degrading; \
                     increase vector_index_capacity or move to dedicated vector DB",
                    EVICTION_RATE_CEILING_PER_HOUR,
                );
            }
        }
    }

    /// Remove an entry by ID (marks for exclusion; cleaned up on rebuild).
    pub fn remove(&self, id: &str) {
        let mut state = match self.inner.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.all_entries.retain(|(eid, _)| eid != id);
        state.overflow.retain(|(eid, _)| eid != id);
        // Note: the HNSW index itself is immutable — removed IDs are filtered
        // from search results. A rebuild will fully remove them.
    }

    /// Search for the `k` nearest neighbors to the query embedding.
    ///
    /// Combines HNSW approximate search with linear scan of overflow entries.
    /// Returns results sorted by ascending distance (closest first).
    pub fn search(&self, query: &[f32], k: usize) -> Vec<VectorHit> {
        let state = match self.inner.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let query_point = EmbeddingPoint(query.to_vec());

        let mut results: Vec<VectorHit> = Vec::with_capacity(k * 2);

        // Collect valid IDs from all_entries for filtering removed entries
        let valid_ids: std::collections::HashSet<&str> = state
            .all_entries
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();

        // Search the HNSW index
        if let Some(ref hnsw) = state.hnsw {
            let mut search = Search::default();
            for item in hnsw.search(&query_point, &mut search) {
                if !valid_ids.contains(item.value.as_str()) {
                    continue; // Removed entry
                }
                results.push(VectorHit {
                    id: item.value.clone(),
                    distance: item.distance,
                });
                if results.len() >= k * 2 {
                    break;
                }
            }
        }

        // Linear scan of overflow entries
        let mut overflow_hits: Vec<VectorHit> = state
            .overflow
            .iter()
            .map(|(id, emb)| {
                let point = EmbeddingPoint(emb.clone());
                VectorHit {
                    id: id.clone(),
                    distance: query_point.distance(&point),
                }
            })
            .collect();
        overflow_hits.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap());

        results.extend(overflow_hits);

        // Deduplicate by ID (prefer lower distance)
        let mut seen = std::collections::HashSet::new();
        results.retain(|hit| seen.insert(hit.id.clone()));

        // Sort by distance and truncate
        results.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap());
        results.truncate(k);
        results
    }

    /// Return the total number of indexed entries (HNSW + overflow).
    pub fn len(&self) -> usize {
        let state = match self.inner.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.all_entries.len()
    }

    /// Force a full rebuild of the HNSW index from all entries.
    #[allow(dead_code)]
    pub fn rebuild(&self) {
        let mut state = match self.inner.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.hnsw = Self::build_hnsw(&state.all_entries);
        state.overflow.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_embedding(values: &[f32]) -> Vec<f32> {
        // L2-normalize
        let norm: f32 = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        values.iter().map(|v| v / norm).collect()
    }

    #[test]
    fn empty_index_returns_empty() {
        let idx = VectorIndex::empty();
        let results = idx.search(&[1.0, 0.0, 0.0], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn basic_search() {
        let entries = vec![
            ("a".into(), make_embedding(&[1.0, 0.0, 0.0])),
            ("b".into(), make_embedding(&[0.0, 1.0, 0.0])),
            ("c".into(), make_embedding(&[0.0, 0.0, 1.0])),
        ];
        let idx = VectorIndex::build(entries);
        let results = idx.search(&make_embedding(&[1.0, 0.1, 0.0]), 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a"); // Closest to [1, 0.1, 0]
    }

    #[test]
    fn insert_and_search_overflow() {
        let entries = vec![("a".into(), make_embedding(&[1.0, 0.0, 0.0]))];
        let idx = VectorIndex::build(entries);
        idx.insert("b".into(), make_embedding(&[0.9, 0.1, 0.0]));
        let results = idx.search(&make_embedding(&[1.0, 0.0, 0.0]), 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert_eq!(results[1].id, "b");
    }

    #[test]
    fn remove_excludes_from_results() {
        let entries = vec![
            ("a".into(), make_embedding(&[1.0, 0.0, 0.0])),
            ("b".into(), make_embedding(&[0.9, 0.1, 0.0])),
        ];
        let idx = VectorIndex::build(entries);
        idx.remove("a");
        let results = idx.search(&make_embedding(&[1.0, 0.0, 0.0]), 5);
        assert!(results.iter().all(|h| h.id != "a"));
    }

    // -----------------------------------------------------------------
    // W11/S11b — rebuild + batched-insert hardening
    // -----------------------------------------------------------------

    #[test]
    fn test_rebuild_preserves_all_entries() {
        // Build a small but non-trivial set of orthonormal-ish vectors,
        // rebuild the index, and confirm every id is still findable via
        // search with a top-k that covers them all.
        let raw: Vec<(String, Vec<f32>)> = (0..12)
            .map(|i| {
                let mut v = vec![0.0_f32; 16];
                #[allow(clippy::cast_precision_loss)]
                let f = i as f32;
                v[i % 16] = 1.0 + f * 0.01; // bias to make L2 norm non-trivial
                (format!("id-{i}"), make_embedding(&v))
            })
            .collect();

        let idx = VectorIndex::build(raw.clone());
        idx.rebuild();
        assert_eq!(idx.len(), raw.len());

        // Every id should appear when we ask for top-N where N >= count.
        let query = make_embedding(&[1.0; 16]);
        let hits = idx.search(&query, raw.len() * 2);
        let found: std::collections::HashSet<String> = hits.into_iter().map(|h| h.id).collect();
        for (id, _) in &raw {
            assert!(
                found.contains(id),
                "rebuild must preserve id {id}, found: {:?}",
                found
            );
        }
    }

    #[test]
    fn test_remove_then_search_excludes_id() {
        let entries = vec![
            ("alpha".into(), make_embedding(&[1.0, 0.0, 0.0, 0.0])),
            ("beta".into(), make_embedding(&[0.9, 0.1, 0.0, 0.0])),
            ("gamma".into(), make_embedding(&[0.8, 0.2, 0.0, 0.0])),
        ];
        let idx = VectorIndex::build(entries);
        // Pre-remove: alpha should be the closest to (1,0,0,0).
        let pre = idx.search(&make_embedding(&[1.0, 0.0, 0.0, 0.0]), 5);
        assert!(pre.iter().any(|h| h.id == "alpha"));

        idx.remove("alpha");
        // Post-remove: alpha must not appear regardless of k.
        for k in 1..=10 {
            let hits = idx.search(&make_embedding(&[1.0, 0.0, 0.0, 0.0]), k);
            assert!(
                hits.iter().all(|h| h.id != "alpha"),
                "removed id `alpha` resurfaced with k={k}: {:?}",
                hits.iter().map(|h| &h.id).collect::<Vec<_>>()
            );
        }

        // Other entries still findable.
        let hits = idx.search(&make_embedding(&[1.0, 0.0, 0.0, 0.0]), 5);
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(ids.contains(&"beta"));
        assert!(ids.contains(&"gamma"));
    }

    // -----------------------------------------------------------------
    // W12-H — small edge cases
    // -----------------------------------------------------------------

    #[test]
    fn empty_index_len_is_zero() {
        let idx = VectorIndex::empty();
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn build_with_empty_entries_search_empty() {
        let idx = VectorIndex::build(Vec::new());
        assert_eq!(idx.len(), 0);
        let results = idx.search(&[1.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn search_with_k_zero_returns_empty() {
        let entries = vec![("a".into(), make_embedding(&[1.0, 0.0, 0.0]))];
        let idx = VectorIndex::build(entries);
        let results = idx.search(&make_embedding(&[1.0, 0.0, 0.0]), 0);
        assert!(results.is_empty());
    }

    #[test]
    fn rebuild_on_empty_does_not_crash() {
        let idx = VectorIndex::empty();
        idx.rebuild();
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn insert_increases_len() {
        let idx = VectorIndex::empty();
        idx.insert("a".into(), make_embedding(&[1.0, 0.0, 0.0]));
        idx.insert("b".into(), make_embedding(&[0.0, 1.0, 0.0]));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn embedding_point_distance_orthogonal() {
        let a = EmbeddingPoint(vec![1.0, 0.0, 0.0]);
        let b = EmbeddingPoint(vec![0.0, 1.0, 0.0]);
        // 1 - dot = 1 - 0 = 1
        assert!((a.distance(&b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn embedding_point_distance_identical_is_zero() {
        let a = EmbeddingPoint(make_embedding(&[1.0, 1.0, 1.0]));
        // 1 - 1 = 0 (L2-normalised)
        assert!(a.distance(&a).abs() < 1e-6);
    }

    #[test]
    fn remove_on_empty_index_is_noop() {
        let idx = VectorIndex::empty();
        idx.remove("nonexistent");
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn insert_triggers_auto_rebuild_at_threshold() {
        // REBUILD_THRESHOLD = 200. Inserting that many into a fresh index
        // exercises the auto-rebuild branch in `insert`.
        let idx = VectorIndex::empty();
        for i in 0..205_usize {
            let mut v = vec![0.0_f32; 8];
            #[allow(clippy::cast_precision_loss)]
            let f = i as f32;
            v[i % 8] = 1.0 + f * 0.001;
            idx.insert(format!("id-{i}"), make_embedding(&v));
        }
        assert_eq!(idx.len(), 205);
        // After auto-rebuild, search still works — top-k returns hits.
        let q = make_embedding(&[1.0_f32; 8]);
        let hits = idx.search(&q, 5);
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn test_rebuild_after_batch_insert_settles() {
        // Start empty, batch-insert N entries, force a rebuild, then assert
        // that top-K search returns exactly K results (deterministic count
        // for a fully-populated index with K <= len).
        let idx = VectorIndex::empty();
        let n = 25_usize;
        for i in 0..n {
            let mut v = vec![0.0_f32; 8];
            #[allow(clippy::cast_precision_loss)]
            let f = i as f32;
            v[i % 8] = 1.0 + f * 0.001;
            idx.insert(format!("id-{i}"), make_embedding(&v));
        }
        // Force a rebuild — overflow may not have hit REBUILD_THRESHOLD.
        idx.rebuild();
        assert_eq!(idx.len(), n);

        let query = make_embedding(&[1.0; 8]);
        let k = 5;
        let hits = idx.search(&query, k);
        assert_eq!(
            hits.len(),
            k,
            "post-rebuild search top-{k} must return exactly {k} hits, got {:?}",
            hits.iter().map(|h| &h.id).collect::<Vec<_>>()
        );

        // Distances should be sorted ascending (closest first).
        for w in hits.windows(2) {
            assert!(
                w[0].distance <= w[1].distance,
                "search results must be ascending by distance: {} > {}",
                w[0].distance,
                w[1].distance
            );
        }

        // No duplicate ids in the result.
        let mut seen = std::collections::HashSet::new();
        for h in &hits {
            assert!(
                seen.insert(h.id.clone()),
                "duplicate id in search: {}",
                h.id
            );
        }
    }

    // -----------------------------------------------------------------
    // v0.7.0 R3-S1 — eviction sink wires the on_index_eviction hook
    // -----------------------------------------------------------------

    /// `test_hnsw_eviction_fires_hook` — when a sink is wired via
    /// [`VectorIndex::set_eviction_sink`] and the index inserts past
    /// its eviction cap, the eviction-edge code path pushes one
    /// [`EvictionEvent`] per evicted id onto the channel. This closes
    /// the G2/G8 "fire site exists but not wired" gap. We construct
    /// the index via [`VectorIndex::with_max_entries_for_test`] so a
    /// 6-entry insert sequence trips the eviction path in
    /// milliseconds without touching the production 100k cap.
    #[test]
    fn test_hnsw_eviction_fires_hook() {
        let (tx, rx) = std::sync::mpsc::channel::<EvictionEvent>();
        let idx = VectorIndex::with_max_entries_for_test(4);
        idx.set_eviction_sink(tx);

        // Reset the process-local counters so concurrent tests
        // sharing the static don't bleed assertions into ours.
        reset_eviction_counters_for_test();

        // Insert cap+2 entries — eviction drops the 2 oldest.
        let n = 6_usize;
        for i in 0..n {
            let mut v = vec![0.0_f32; 4];
            #[allow(clippy::cast_precision_loss)]
            let f = i as f32;
            v[i % 4] = 1.0 + f * 0.01;
            idx.insert(format!("evict-{i}"), make_embedding(&v));
        }

        // Drain the channel. Expect TWO events (n=6, cap=4) — one
        // per evicted id. The unbounded sender does not block; the
        // events should already be enqueued by the time `insert`
        // returns, but we give the channel a small grace window for
        // thread-scheduling jitter on slow CI runners.
        let mut received: Vec<EvictionEvent> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline && received.len() < 2 {
            while let Ok(ev) = rx.try_recv() {
                received.push(ev);
            }
            if received.len() < 2 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        assert_eq!(
            received.len(),
            2,
            "expected one EvictionEvent per evicted id (2 evictions for n=6, cap=4), got {}: {:?}",
            received.len(),
            received.iter().map(|e| &e.memory_id).collect::<Vec<_>>(),
        );

        let ids: Vec<&str> = received.iter().map(|e| e.memory_id.as_str()).collect();
        assert!(
            ids.contains(&"evict-0"),
            "expected evict-0 in evicted ids; got {ids:?}"
        );
        assert!(
            ids.contains(&"evict-1"),
            "expected evict-1 in evicted ids; got {ids:?}"
        );

        for ev in &received {
            assert_eq!(
                ev.reason, "max_entries_reached",
                "evicted reason should match the canonical tag, got {:?}",
                ev.reason
            );
            // namespace is intentionally empty at the hnsw layer
            // (the index does not carry namespace context); G9+ may
            // plumb it through. The wire field MUST be present even
            // when empty.
            assert_eq!(ev.namespace, "");
            assert!(
                !ev.evicted_at.is_empty(),
                "evicted_at must be set (rfc3339), got empty"
            );
        }
    }

    /// Sanity: insertion without a sink wired is a no-op for the
    /// hook path. The eviction-edge code path must remain functional
    /// (counters bump, oldest drained) even when no sink is set, so
    /// the CLI / test build's zero-cost posture is preserved.
    #[test]
    fn test_hnsw_eviction_without_sink_is_noop_for_hook() {
        let idx = VectorIndex::with_max_entries_for_test(4);
        // No `set_eviction_sink` call here — the index runs as in
        // CLI / pre-R3-S1 builds without a hooks pipeline.

        let before = index_evictions_total();
        for i in 0..6_usize {
            let mut v = vec![0.0_f32; 4];
            #[allow(clippy::cast_precision_loss)]
            let f = i as f32;
            v[i % 4] = 1.0 + f * 0.01;
            idx.insert(format!("noopsink-{i}"), make_embedding(&v));
        }
        let delta = index_evictions_total().saturating_sub(before);

        assert!(
            delta >= 2,
            "eviction counters must still bump even without a sink wired (got delta={delta})"
        );
    }
}
