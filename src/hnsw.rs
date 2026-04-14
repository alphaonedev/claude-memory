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
    /// Build a new index from a list of (memory_id, embedding) pairs.
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
}
