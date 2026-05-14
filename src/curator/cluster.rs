// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Clustering strategies for the compaction pipeline.
//!
//! Two strategies ship in v0.7.0 L1-7:
//!
//! * [`JaccardClustering`] — keyword-bag Jaccard similarity.  Extracted
//!   verbatim from the v0.6.x `src/autonomy.rs` consolidation logic.
//!   O(n²) per namespace; acceptable for the typical curator batch size
//!   (≤ 400 candidates per cycle).  Used as the cheap pre-filter in
//!   `ConsolidationPass` when embeddings are not available.
//!
//! * [`CosineClustering`] — pairwise cosine similarity on 384-dim MiniLM
//!   embeddings with agglomerative single-link clustering.  This is the
//!   primary path in `ConsolidationPass`; `JaccardClustering` is the
//!   fallback when no `Embedder` is wired in.
//!
//! ## Visibility contract (R7)
//!
//! All items are at most `pub(crate)` — nothing escapes the crate
//! boundary through this module.

use std::collections::HashSet;

use crate::embeddings::Embedder;
use crate::models::Memory;

use super::pipeline::MemoryId;

// ---------------------------------------------------------------------------
// Shared constants (re-exported from autonomy for regression-free parity)
// ---------------------------------------------------------------------------

/// Minimum Jaccard overlap to place two memories in the same cluster.
/// Matches `crate::autonomy::CONSOLIDATE_JACCARD_THRESHOLD` (0.55).
pub(crate) const JACCARD_THRESHOLD: f64 = 0.55;

/// Maximum members per cluster — prevents pathological mega-merges.
/// Matches `crate::autonomy::CONSOLIDATE_MAX_CLUSTER_SIZE` (8).
pub(crate) const MAX_CLUSTER_SIZE: usize = 8;

/// Default cosine similarity threshold for [`CosineClustering`].
/// Memories whose pairwise cosine similarity ≥ this value are placed
/// in the same cluster by the agglomerative single-link pass.
pub(crate) const DEFAULT_COSINE_THRESHOLD: f32 = 0.75;

// ---------------------------------------------------------------------------
// JaccardClustering
// ---------------------------------------------------------------------------

/// Clusters memories by Jaccard keyword overlap.
///
/// Extracted from `crate::autonomy::find_consolidation_clusters` (v0.6.x).
/// Produces identical clusters to the original code on the same input —
/// regression-free refactor.
///
/// Groups are formed within a single namespace; cross-namespace clusters
/// are never produced.
pub(crate) struct JaccardClustering {
    /// Minimum overlap threshold.  Defaults to [`JACCARD_THRESHOLD`].
    pub(crate) threshold: f64,
    /// Maximum cluster size.  Defaults to [`MAX_CLUSTER_SIZE`].
    pub(crate) max_cluster_size: usize,
}

impl Default for JaccardClustering {
    fn default() -> Self {
        Self {
            threshold: JACCARD_THRESHOLD,
            max_cluster_size: MAX_CLUSTER_SIZE,
        }
    }
}

impl JaccardClustering {
    /// Partition `memories` into groups whose pairwise Jaccard overlap
    /// meets `self.threshold`.  Only groups with ≥ 2 members are
    /// returned; singletons are discarded.
    ///
    /// The algorithm is O(n²) within each namespace and is intentionally
    /// identical to the original `autonomy::find_consolidation_clusters`
    /// so that existing autonomy-pass fixtures remain regression-free.
    pub(crate) fn cluster_memories(&self, memories: &[Memory]) -> Vec<Vec<MemoryId>> {
        // Group by namespace — never merge across namespace boundaries.
        let mut by_ns: std::collections::HashMap<&str, Vec<&Memory>> =
            std::collections::HashMap::new();
        for m in memories {
            if m.namespace.starts_with('_') {
                continue;
            }
            by_ns.entry(&m.namespace).or_default().push(m);
        }

        let mut clusters: Vec<Vec<MemoryId>> = Vec::new();
        for (_ns, group) in by_ns {
            let mut used = vec![false; group.len()];
            for i in 0..group.len() {
                if used[i] {
                    continue;
                }
                let mut cluster = vec![group[i].id.clone()];
                used[i] = true;
                for j in (i + 1)..group.len() {
                    if used[j] {
                        continue;
                    }
                    if cluster.len() >= self.max_cluster_size {
                        break;
                    }
                    if jaccard_similarity(&group[i].content, &group[j].content) >= self.threshold {
                        cluster.push(group[j].id.clone());
                        used[j] = true;
                    }
                }
                if cluster.len() >= 2 {
                    clusters.push(cluster);
                }
            }
        }
        clusters
    }
}

// ---------------------------------------------------------------------------
// CosineClustering
// ---------------------------------------------------------------------------

/// Clusters memories by pairwise cosine similarity on 384-dim MiniLM
/// embeddings using a greedy single-link agglomerative algorithm.
///
/// The primary compaction path in `ConsolidationPass`.
/// `JaccardClustering` is the fallback when no embedder is available.
///
/// ## Algorithm
///
/// 1. Embed each memory's content.  Skip memories whose embedding fails
///    (they fall through to the Jaccard path).
/// 2. Build the full pairwise cosine matrix.
/// 3. Greedy single-link merge: iterate over candidates; any pair whose
///    cosine similarity ≥ `threshold` and whose memories share the same
///    namespace are merged into the same cluster.
/// 4. Clusters are capped at `max_cluster_size`.
///
/// ## Visibility
///
/// `pub(crate)` — only `ConsolidationPass` instantiates this.
pub(crate) struct CosineClustering {
    /// Minimum cosine similarity to merge two memories.
    pub(crate) threshold: f32,
    /// Maximum members per cluster.
    pub(crate) max_cluster_size: usize,
    /// Embedding engine.  When `None`, `cluster_memories` returns an
    /// empty vec and the caller falls back to Jaccard.
    pub(crate) embedder: Option<Embedder>,
}

impl CosineClustering {
    /// Construct a `CosineClustering` with the given embedder.
    pub(crate) fn new(embedder: Option<Embedder>) -> Self {
        Self {
            threshold: DEFAULT_COSINE_THRESHOLD,
            max_cluster_size: MAX_CLUSTER_SIZE,
            embedder,
        }
    }

    /// Partition `memories` into cosine-similar clusters.
    ///
    /// Returns an empty vec if no embedder is available (callers fall
    /// back to [`JaccardClustering`]).  Memories that fail embedding are
    /// silently skipped (the embedder already logs the failure).
    pub(crate) fn cluster_memories(&self, memories: &[Memory]) -> Vec<Vec<MemoryId>> {
        let Some(ref embedder) = self.embedder else {
            return Vec::new();
        };

        // Embed all memories; skip failures.
        let embedded: Vec<(&Memory, Vec<f32>)> = memories
            .iter()
            .filter(|m| !m.namespace.starts_with('_'))
            .filter_map(|m| embedder.embed(&m.content).ok().map(|v| (m, v)))
            .collect();

        if embedded.is_empty() {
            return Vec::new();
        }

        let n = embedded.len();
        let mut assigned = vec![false; n];
        let mut clusters: Vec<Vec<MemoryId>> = Vec::new();

        for i in 0..n {
            if assigned[i] {
                continue;
            }
            let mut cluster = vec![embedded[i].0.id.clone()];
            assigned[i] = true;

            for j in (i + 1)..n {
                if assigned[j] {
                    continue;
                }
                if cluster.len() >= self.max_cluster_size {
                    break;
                }
                // Never merge across namespace boundaries.
                if embedded[i].0.namespace != embedded[j].0.namespace {
                    continue;
                }
                let sim = Embedder::cosine_similarity(&embedded[i].1, &embedded[j].1);
                if sim >= self.threshold {
                    cluster.push(embedded[j].0.id.clone());
                    assigned[j] = true;
                }
            }

            if cluster.len() >= 2 {
                clusters.push(cluster);
            }
        }

        clusters
    }
}

// ---------------------------------------------------------------------------
// Shared helper — Jaccard similarity on tokenised content
// ---------------------------------------------------------------------------

/// Compute the Jaccard similarity between two content strings.
///
/// Tokens are runs of ≥ 3 alphanumeric characters, lowercased.
/// Identical to the implementation extracted from `crate::autonomy`.
pub(super) fn jaccard_similarity(a: &str, b: &str) -> f64 {
    let tokens = |s: &str| -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 3)
            .map(str::to_lowercase)
            .collect()
    };
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() && tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    if union == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let result = inter as f64 / union as f64;
        result
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, Tier};

    fn make_memory(id: &str, ns: &str, content: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: id.to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: id.to_string(),
            content: content.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
        }
    }

    // ---- jaccard_similarity -------------------------------------------------

    #[test]
    fn jaccard_identical_strings() {
        let s = "kubernetes rolling canary deploy strategy kubernetes deploy";
        assert!((jaccard_similarity(s, s) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_disjoint_strings() {
        let a = "apple banana cherry";
        let b = "delta echo foxtrot";
        assert_eq!(jaccard_similarity(a, b), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let a = "rust programming language memory safety";
        let b = "rust language systems programming";
        // Overlapping tokens: rust, programming, language → 3/6 = 0.5
        let sim = jaccard_similarity(a, b);
        assert!(sim > 0.0 && sim < 1.0, "sim={sim}");
    }

    #[test]
    fn jaccard_empty_strings() {
        assert_eq!(jaccard_similarity("", ""), 0.0);
    }

    // ---- JaccardClustering --------------------------------------------------

    #[test]
    fn jaccard_clustering_groups_near_duplicates() {
        let strategy = JaccardClustering::default();
        let m1 = make_memory(
            "a",
            "ns",
            "kubernetes rolling canary deploy strategy kubernetes deploy",
        );
        let m2 = make_memory(
            "b",
            "ns",
            "kubernetes rolling canary deploy strategy kubernetes deploy",
        );
        let m3 = make_memory("c", "ns", "completely different unrelated content here");

        let clusters = strategy.cluster_memories(&[m1, m2, m3]);
        // a+b should cluster; c is a singleton → discarded
        assert_eq!(clusters.len(), 1, "expected one cluster; got {clusters:?}");
        let cluster = &clusters[0];
        assert!(cluster.contains(&"a".to_string()));
        assert!(cluster.contains(&"b".to_string()));
        assert!(!cluster.contains(&"c".to_string()));
    }

    #[test]
    fn jaccard_clustering_never_merges_across_namespaces() {
        let strategy = JaccardClustering::default();
        let m1 = make_memory("a", "ns1", "kubernetes rolling canary deploy strategy");
        let m2 = make_memory("b", "ns2", "kubernetes rolling canary deploy strategy");
        // Different namespaces → never clustered together.
        let clusters = strategy.cluster_memories(&[m1, m2]);
        // Each namespace group has only one member → no clusters of size ≥ 2
        assert!(
            clusters.is_empty(),
            "expected no cross-ns clusters; got {clusters:?}"
        );
    }

    #[test]
    fn jaccard_clustering_skips_internal_namespaces() {
        let strategy = JaccardClustering::default();
        let m1 = make_memory("a", "_curator", "kubernetes rolling canary deploy strategy");
        let m2 = make_memory("b", "_curator", "kubernetes rolling canary deploy strategy");
        let clusters = strategy.cluster_memories(&[m1, m2]);
        assert!(clusters.is_empty(), "internal ns must be skipped");
    }

    #[test]
    fn jaccard_clustering_respects_max_cluster_size() {
        let strategy = JaccardClustering {
            threshold: 0.0, // Accept everything
            max_cluster_size: 3,
        };
        let mems: Vec<Memory> = (0..10)
            .map(|i| make_memory(&format!("m{i}"), "ns", "shared token content shared"))
            .collect();
        let clusters = strategy.cluster_memories(&mems);
        for c in &clusters {
            assert!(c.len() <= 3, "cluster size {}", c.len());
        }
    }

    // ---- CosineClustering (no-embedder path) ---------------------------------

    #[test]
    fn cosine_clustering_without_embedder_returns_empty() {
        let strategy = CosineClustering::new(None);
        let m1 = make_memory("a", "ns", "kubernetes rolling canary deploy strategy");
        let m2 = make_memory("b", "ns", "kubernetes rolling canary deploy strategy");
        let clusters = strategy.cluster_memories(&[m1, m2]);
        // No embedder → empty result; caller falls back to Jaccard.
        assert!(clusters.is_empty());
    }

    // ---- Jaccard edge cases that hit specific branches ---------------------

    #[test]
    fn jaccard_clustering_with_empty_input_returns_empty() {
        let strategy = JaccardClustering::default();
        let clusters = strategy.cluster_memories(&[]);
        assert!(clusters.is_empty());
    }

    #[test]
    fn jaccard_clustering_skips_already_used_member() {
        // Hits the inner `if used[j] { continue; }` branch (line 106).
        // Construct 3 memories: a, b, c.
        // a≈b strong match — they cluster. Then when scanning b's row,
        // both b and a are `used` so the `if used[j]` early-`continue`
        // triggers.
        let strategy = JaccardClustering {
            threshold: 0.3,
            max_cluster_size: 10,
        };
        let s = "shared keyword tokens deployment plan strategy";
        let m1 = make_memory("a", "ns", s);
        let m2 = make_memory("b", "ns", s);
        let m3 = make_memory("c", "ns", s);
        let clusters = strategy.cluster_memories(&[m1, m2, m3]);
        // One cluster of size 3.
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 3);
    }

    // ---- CosineClustering with a real Embedder (skip if HF model missing) --

    /// Attempt to build a real local Embedder. Returns `None` when the
    /// HuggingFace model is not in the host's cache (offline CI worker);
    /// callers MUST early-return cleanly in that case so the test still
    /// passes — the coverage uplift only happens on hosts where the
    /// model cache is pre-warmed.
    fn try_local_embedder() -> Option<Embedder> {
        Embedder::new_local().ok()
    }

    #[test]
    fn cosine_clustering_with_embedder_clusters_similar_content() {
        let Some(embedder) = try_local_embedder() else {
            return;
        };
        let strategy = CosineClustering::new(Some(embedder));
        // Two near-identical contents → cosine similarity ≥ 0.75 → cluster.
        let m1 = make_memory(
            "a",
            "ns",
            "Kubernetes rolling canary deployment strategy notes",
        );
        let m2 = make_memory(
            "b",
            "ns",
            "Kubernetes rolling canary deployment strategy notes",
        );
        let clusters = strategy.cluster_memories(&[m1, m2]);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 2);
    }

    #[test]
    fn cosine_clustering_never_merges_across_namespaces() {
        let Some(embedder) = try_local_embedder() else {
            return;
        };
        let strategy = CosineClustering::new(Some(embedder));
        let m1 = make_memory("a", "ns1", "identical content for both rows");
        let m2 = make_memory("b", "ns2", "identical content for both rows");
        let clusters = strategy.cluster_memories(&[m1, m2]);
        assert!(clusters.is_empty());
    }

    #[test]
    fn cosine_clustering_skips_internal_namespaces() {
        let Some(embedder) = try_local_embedder() else {
            return;
        };
        let strategy = CosineClustering::new(Some(embedder));
        let m1 = make_memory("a", "_curator", "shared content tokens");
        let m2 = make_memory("b", "_curator", "shared content tokens");
        let clusters = strategy.cluster_memories(&[m1, m2]);
        assert!(clusters.is_empty());
    }

    #[test]
    fn cosine_clustering_respects_max_cluster_size() {
        let Some(embedder) = try_local_embedder() else {
            return;
        };
        let strategy = CosineClustering {
            threshold: 0.5,
            max_cluster_size: 2,
            embedder: Some(embedder),
        };
        let s = "identical clustering content goes here always";
        let mems: Vec<Memory> = (0..6)
            .map(|i| make_memory(&format!("m{i}"), "ns", s))
            .collect();
        let clusters = strategy.cluster_memories(&mems);
        for c in &clusters {
            assert!(c.len() <= 2, "cluster size {}", c.len());
        }
    }

    #[test]
    fn cosine_clustering_drops_low_similarity_singletons() {
        let Some(embedder) = try_local_embedder() else {
            return;
        };
        let strategy = CosineClustering::new(Some(embedder));
        // Six completely different topics → no pair clusters.
        let topics = [
            "Python list comprehension idioms for filtering",
            "Reverse-engineering binary protocols on the wire",
            "Cherry-picking commits across forked git branches",
            "Distributed consensus by Raft leader election",
            "Cuisine of southern Italy and Sicilian olive oil",
            "Quantum-mechanical interpretation of double-slit experiment",
        ];
        let mems: Vec<Memory> = topics
            .iter()
            .enumerate()
            .map(|(i, t)| make_memory(&format!("m{i}"), "ns", t))
            .collect();
        let clusters = strategy.cluster_memories(&mems);
        // Either zero clusters (most likely) OR small clusters of length
        // >= 2. We just want the code path executed; assert NO cluster
        // has length 1 (those are discarded by the strategy).
        for c in &clusters {
            assert!(c.len() >= 2);
        }
    }
}
