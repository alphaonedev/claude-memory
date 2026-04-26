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

/// Maximum overflow entries before triggering a rebuild.
const REBUILD_THRESHOLD: usize = 200;

/// Maximum entries before evicting oldest to prevent unbounded memory growth.
const MAX_ENTRIES: usize = 100_000;

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
}

struct IndexState {
    hnsw: Option<HnswMap<EmbeddingPoint, String>>,
    /// Entries added after the last rebuild. Searched linearly.
    overflow: Vec<(String, Vec<f32>)>,
    /// All entries (for rebuild). Kept in sync with the index + overflow.
    all_entries: Vec<(String, Vec<f32>)>,
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
            }),
        }
    }

    /// Build an empty index.
    pub fn empty() -> Self {
        VectorIndex {
            inner: Mutex::new(IndexState {
                hnsw: None,
                overflow: Vec::new(),
                all_entries: Vec::new(),
            }),
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
        if state.all_entries.len() > MAX_ENTRIES {
            let excess = state.all_entries.len() - MAX_ENTRIES;
            state.all_entries.drain(..excess);
            state.hnsw = Self::build_hnsw(&state.all_entries);
            state.overflow.clear();
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
}
