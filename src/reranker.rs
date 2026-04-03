// Copyright (c) 2026 AlphaOne LLC. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root.

//! Cross-encoder reranking for search results.
//!
//! A cross-encoder takes a (query, document) pair and produces a relevance
//! score. This is more accurate than cosine similarity of independent
//! embeddings but slower since it must run for each candidate.
//!
//! **Current implementation:** lightweight lexical cross-encoder using term
//! overlap, TF-IDF-like weighting, bigram overlap, and title match bonus.
//!
//! **Phase 4 target:** swap in `cross-encoder/ms-marco-MiniLM-L-6-v2` (ONNX,
//! ~80 MB) for neural cross-encoding. The public interface is designed so that
//! swap is non-breaking.

use std::collections::{HashMap, HashSet};

use crate::models::Memory;

/// Blend weight applied to the original (embedding/FTS) score.
const ORIGINAL_WEIGHT: f64 = 0.6;
/// Blend weight applied to the cross-encoder score.
const CROSS_ENCODER_WEIGHT: f64 = 0.4;

/// Placeholder cross-encoder that scores (query, document) pairs using
/// lexical signals. Designed to be replaced by an ONNX neural model in a
/// future PR without changing the public API.
pub struct CrossEncoder {
    // No state needed for the lexical variant.
    // When we wire in the ONNX model this will hold the session + tokenizer.
    _private: (),
}

impl CrossEncoder {
    /// Create a new `CrossEncoder`.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Score a single (query, document) pair.
    ///
    /// Returns a relevance score in `0.0..=1.0`.
    pub fn score(&self, query: &str, title: &str, content: &str) -> f32 {
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return 0.0;
        }

        let title_terms = tokenize(title);
        let content_terms = tokenize(content);

        // Combine title + content into one document term set for overlap
        // calculations, but keep them separate for the title bonus.
        let doc_terms: HashSet<&str> = title_terms
            .iter()
            .chain(content_terms.iter())
            .copied()
            .collect();
        let query_set: HashSet<&str> = query_terms.iter().copied().collect();

        // ----- 1. Jaccard term overlap -----
        let intersection = query_set.intersection(&doc_terms).count() as f32;
        let union = query_set.union(&doc_terms).count() as f32;
        let jaccard = if union > 0.0 {
            intersection / union
        } else {
            0.0
        };

        // ----- 2. TF-IDF-like term weighting -----
        // Treat each unique term in the document as a "document". Terms that
        // appear in fewer positions relative to total tokens carry more weight.
        let doc_all: Vec<&str> = title_terms
            .iter()
            .chain(content_terms.iter())
            .copied()
            .collect();
        let tf_idf = tfidf_score(&query_terms, &doc_all);

        // ----- 3. Bigram overlap bonus -----
        let query_bigrams = bigrams(&query_terms);
        let doc_bigrams = bigrams(&doc_all);
        let bigram_overlap = if query_bigrams.is_empty() {
            0.0
        } else {
            let doc_bigram_set: HashSet<(&str, &str)> = doc_bigrams.into_iter().collect();
            let hits = query_bigrams
                .iter()
                .filter(|b| doc_bigram_set.contains(b))
                .count() as f32;
            hits / query_bigrams.len() as f32
        };

        // ----- 4. Title match bonus -----
        let title_set: HashSet<&str> = title_terms.iter().copied().collect();
        let title_hits = query_set.intersection(&title_set).count() as f32;
        let title_bonus = if query_set.is_empty() {
            0.0
        } else {
            title_hits / query_set.len() as f32
        };

        // ----- Combine signals -----
        // Weights chosen so the sum of maximums ≈ 1.0.
        let raw = 0.30 * jaccard + 0.30 * tf_idf + 0.20 * bigram_overlap + 0.20 * title_bonus;

        raw.clamp(0.0, 1.0)
    }

    /// Rerank a set of candidates by blending their original scores with
    /// cross-encoder scores.
    ///
    /// **Blend formula:** `final = 0.6 * original + 0.4 * cross_encoder`
    ///
    /// Results are returned sorted by `final_score` descending.
    pub fn rerank(
        &self,
        query: &str,
        mut candidates: Vec<(Memory, f64)>,
    ) -> Vec<(Memory, f64)> {
        // Score each candidate and blend.
        let mut scored: Vec<(Memory, f64)> = candidates
            .drain(..)
            .map(|(mem, original_score)| {
                let ce_score = self.score(query, &mem.title, &mem.content) as f64;
                let final_score =
                    ORIGINAL_WEIGHT * original_score + CROSS_ENCODER_WEIGHT * ce_score;
                (mem, final_score)
            })
            .collect();

        // Sort descending by final score.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

impl Default for CrossEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Lowercase, strip non-alphanumeric, split on whitespace. Returns tokens
/// with lifetime tied to the owned `String` -- we return `Vec<String>` to
/// avoid lifetime issues but the callers re-borrow as `&str`.
fn tokenize(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect::<Vec<&str>>()
        .into_iter()
        .collect()
}

/// Compute a simplified TF-IDF-like score for query terms against a document
/// token stream.
///
/// TF  = occurrences of term in doc / total doc tokens
/// IDF = log(total_unique / (1 + docs_containing_term))  (smoothed)
///
/// Here "docs_containing_term" is approximated by the number of unique
/// document tokens that equal the query term (0 or 1), making IDF act as a
/// binary discriminator: terms that appear in the document score higher.
fn tfidf_score(query_terms: &[&str], doc_tokens: &[&str]) -> f32 {
    if doc_tokens.is_empty() || query_terms.is_empty() {
        return 0.0;
    }

    // Build term frequency map for the document.
    let mut tf_map: HashMap<&str, usize> = HashMap::new();
    for &tok in doc_tokens {
        *tf_map.entry(tok).or_insert(0) += 1;
    }

    let total = doc_tokens.len() as f32;
    let unique = tf_map.len() as f32;

    let mut score_sum: f32 = 0.0;
    let query_lower: Vec<String> = query_terms.iter().map(|t| t.to_lowercase()).collect();

    for qt in &query_lower {
        // Find case-insensitive match in tf_map.
        let tf = tf_map
            .iter()
            .filter(|(k, _)| k.to_lowercase() == *qt)
            .map(|(_, &v)| v)
            .sum::<usize>() as f32;

        if tf == 0.0 {
            continue;
        }

        let tf_norm = tf / total;
        // IDF: boost rare terms within the document.
        let doc_freq = tf_map
            .keys()
            .filter(|k| k.to_lowercase() == *qt)
            .count() as f32;
        let idf = (unique / (1.0 + doc_freq)).ln() + 1.0;

        score_sum += tf_norm * idf;
    }

    // Normalize so maximum ≈ 1.0 regardless of query length.
    let max_possible = query_lower.len() as f32; // each term could contribute ~1.0
    (score_sum / max_possible).clamp(0.0, 1.0)
}

/// Extract consecutive bigrams from a token list.
fn bigrams<'a>(tokens: &'a [&str]) -> Vec<(&'a str, &'a str)> {
    tokens.windows(2).map(|w| (w[0], w[1])).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, Tier};

    fn make_memory(title: &str, content: &str) -> Memory {
        Memory {
            id: "test-id".to_string(),
            tier: Tier::Mid,
            namespace: "test".to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
        }
    }

    #[test]
    fn score_returns_zero_for_empty_query() {
        let ce = CrossEncoder::new();
        assert_eq!(ce.score("", "some title", "some content"), 0.0);
    }

    #[test]
    fn score_returns_zero_for_no_overlap() {
        let ce = CrossEncoder::new();
        let s = ce.score("quantum physics", "grocery list", "milk eggs bread butter");
        assert!(s < 0.05, "expected near-zero, got {s}");
    }

    #[test]
    fn score_rewards_title_match() {
        let ce = CrossEncoder::new();
        let content = "This document discusses network configuration for LAN setups.";
        let s_title_match = ce.score("network configuration", "Network Configuration Guide", content);
        let s_no_title = ce.score("network configuration", "Unrelated Title", content);
        assert!(
            s_title_match > s_no_title,
            "title match ({s_title_match}) should beat no title match ({s_no_title})"
        );
    }

    #[test]
    fn score_is_bounded_zero_one() {
        let ce = CrossEncoder::new();
        let s = ce.score(
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox",
            "the quick brown fox jumps over the lazy dog and more words",
        );
        assert!((0.0..=1.0).contains(&s), "score {s} out of bounds");
    }

    #[test]
    fn rerank_reorders_candidates() {
        let ce = CrossEncoder::new();

        // Candidate A: low original score but perfect content match.
        let a = make_memory("Rust cross-encoder", "cross-encoder reranking for search");
        // Candidate B: high original score but poor content match.
        let b = make_memory("Grocery list", "milk eggs bread butter cheese");

        let candidates = vec![
            (b.clone(), 0.55), // B ranked first by original score
            (a.clone(), 0.45), // A ranked second — close enough for CE to flip
        ];

        let reranked = ce.rerank("cross-encoder reranking", candidates);

        assert_eq!(reranked[0].0.title, "Rust cross-encoder");
    }

    #[test]
    fn rerank_preserves_candidate_count() {
        let ce = CrossEncoder::new();
        let candidates = vec![
            (make_memory("A", "alpha"), 0.5),
            (make_memory("B", "beta"), 0.6),
            (make_memory("C", "gamma"), 0.7),
        ];
        let reranked = ce.rerank("alpha", candidates);
        assert_eq!(reranked.len(), 3);
    }

    #[test]
    fn bigram_overlap_boosts_phrase_match() {
        let ce = CrossEncoder::new();
        // Exact phrase present in document.
        let s_phrase = ce.score(
            "network adapter",
            "title",
            "the network adapter is connected to the LAN",
        );
        // Same words but not adjacent.
        let s_scattered = ce.score(
            "network adapter",
            "title",
            "the adapter handles the network traffic independently",
        );
        assert!(
            s_phrase > s_scattered,
            "phrase match ({s_phrase}) should beat scattered ({s_scattered})"
        );
    }
}
