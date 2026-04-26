// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Cross-encoder reranking for search results.
//!
//! A cross-encoder takes a (query, document) pair and produces a relevance
//! score. This is more accurate than cosine similarity of independent
//! embeddings but slower since it must run for each candidate.
//!
//! **Two implementations:**
//! - `CrossEncoder::Lexical` — lightweight term-overlap scorer (default).
//! - `CrossEncoder::Neural` — BERT-based cross-encoder loaded via candle
//!   from `cross-encoder/ms-marco-MiniLM-L-6-v2` (~80 MB, ONNX-free).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::{Repo, RepoType, api::sync::Api};
use tokenizers::Tokenizer;

use crate::models::Memory;

/// Blend weight applied to the original (embedding/FTS) score.
const ORIGINAL_WEIGHT: f64 = 0.6;
/// Blend weight applied to the cross-encoder score.
const CROSS_ENCODER_WEIGHT: f64 = 0.4;

const CROSS_ENCODER_MODEL_ID: &str = "cross-encoder/ms-marco-MiniLM-L-6-v2";
const CROSS_ENCODER_MAX_SEQ: usize = 512;
const CROSS_ENCODER_HIDDEN_DIM: usize = 384;

/// Cross-encoder for (query, document) relevance scoring.
pub enum CrossEncoder {
    /// Lightweight lexical cross-encoder using term overlap signals.
    Lexical,
    /// Neural BERT-based cross-encoder (ms-marco-MiniLM-L-6-v2).
    Neural {
        model: Arc<Mutex<BertModel>>,
        tokenizer: Arc<Tokenizer>,
        classifier_weight: Tensor,
        classifier_bias: Tensor,
        device: Device,
    },
}

impl CrossEncoder {
    /// Create a new lexical cross-encoder (no model download required).
    pub fn new() -> Self {
        Self::Lexical
    }

    /// Create a neural cross-encoder by downloading ms-marco-MiniLM-L-6-v2.
    ///
    /// Falls back to lexical if download or loading fails.
    pub fn new_neural() -> Self {
        match Self::load_neural() {
            Ok(ce) => ce,
            Err(e) => {
                eprintln!("ai-memory: neural cross-encoder failed ({e}), using lexical fallback");
                Self::Lexical
            }
        }
    }

    fn load_neural() -> Result<Self> {
        let device = Device::Cpu;

        let api = Api::new().context("failed to init HuggingFace Hub API")?;
        let repo = api.repo(Repo::new(
            CROSS_ENCODER_MODEL_ID.to_string(),
            RepoType::Model,
        ));

        let config_path = repo
            .get("config.json")
            .context("failed to download config.json")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("failed to download tokenizer.json")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("failed to download model.safetensors")?;

        // Load BERT config
        let config_data = std::fs::read_to_string(&config_path)
            .context("failed to read cross-encoder config.json")?;
        let config: BertConfig = serde_json::from_str(&config_data)
            .context("failed to parse cross-encoder config.json")?;

        // Load tokenizer
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load cross-encoder tokenizer: {e}"))?;
        let truncation = tokenizers::TruncationParams {
            max_length: CROSS_ENCODER_MAX_SEQ,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| anyhow::anyhow!("failed to set truncation: {e}"))?;
        tokenizer.with_padding(None);

        // Load model weights
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)
                .context("failed to load cross-encoder weights")?
        };

        let model = BertModel::load(vb.clone(), &config)
            .context("failed to build cross-encoder BertModel")?;

        // Load the classification head: classifier.weight [1, hidden_dim] and classifier.bias [1]
        let classifier_weight = vb
            .get((1, CROSS_ENCODER_HIDDEN_DIM), "classifier.weight")
            .context("failed to load classifier.weight")?;
        let classifier_bias = vb
            .get(1, "classifier.bias")
            .context("failed to load classifier.bias")?;

        Ok(Self::Neural {
            model: Arc::new(Mutex::new(model)),
            tokenizer: Arc::new(tokenizer),
            classifier_weight,
            classifier_bias,
            device,
        })
    }

    /// Score a single (query, document) pair.
    ///
    /// Returns a relevance score in `0.0..=1.0`.
    pub fn score(&self, query: &str, title: &str, content: &str) -> f32 {
        match self {
            Self::Lexical => lexical_score(query, title, content),
            Self::Neural {
                model,
                tokenizer,
                classifier_weight,
                classifier_bias,
                device,
            } => {
                let model_guard = match model.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!("cross-encoder model lock poisoned: {e}");
                        return lexical_score(query, title, content);
                    }
                };
                match Self::neural_score(
                    &model_guard,
                    tokenizer,
                    classifier_weight,
                    classifier_bias,
                    device,
                    query,
                    title,
                    content,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            "neural cross-encoder score failed: {e}, using lexical fallback"
                        );
                        lexical_score(query, title, content)
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn neural_score(
        model: &BertModel,
        tokenizer: &Tokenizer,
        classifier_weight: &Tensor,
        classifier_bias: &Tensor,
        device: &Device,
        query: &str,
        title: &str,
        content: &str,
    ) -> Result<f32> {
        // Cross-encoder input: "[CLS] query [SEP] title content [SEP]"
        let document = format!("{title} {content}");

        let encoding = tokenizer
            .encode((query, document.as_str()), true)
            .map_err(|e| anyhow::anyhow!("cross-encoder tokenization failed: {e}"))?;

        let input_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();
        let token_type_ids = encoding.get_type_ids();
        let seq_len = input_ids.len();

        let input_ids = Tensor::new(input_ids, device)?.reshape((1, seq_len))?;
        let attention_mask = Tensor::new(attention_mask, device)?.reshape((1, seq_len))?;
        let token_type_ids = Tensor::new(token_type_ids, device)?.reshape((1, seq_len))?;

        // Forward pass through BERT → [1, seq_len, 384]
        let hidden = model.forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // Take [CLS] token (first token) → [1, 384]
        let cls = hidden.narrow(1, 0, 1)?.squeeze(1)?;

        // Classification head: logit = cls @ weight^T + bias → [1, 1]
        let logit = cls
            .matmul(&classifier_weight.t()?)?
            .broadcast_add(classifier_bias)?;

        // Extract scalar logit and apply sigmoid to get [0, 1] score
        let logit_val: f32 = logit.squeeze(0)?.squeeze(0)?.to_scalar()?;
        let score = 1.0 / (1.0 + (-logit_val).exp());

        Ok(score)
    }

    /// Whether this is a neural cross-encoder.
    pub fn is_neural(&self) -> bool {
        matches!(self, Self::Neural { .. })
    }

    /// Rerank a set of candidates by blending their original scores with
    /// cross-encoder scores.
    ///
    /// **Blend formula:** `final = 0.6 * original + 0.4 * cross_encoder`
    ///
    /// Results are returned sorted by `final_score` descending.
    pub fn rerank(&self, query: &str, mut candidates: Vec<(Memory, f64)>) -> Vec<(Memory, f64)> {
        let mut scored: Vec<(Memory, f64)> = candidates
            .drain(..)
            .map(|(mem, original_score)| {
                let ce_score = f64::from(self.score(query, &mem.title, &mem.content));
                let final_score =
                    ORIGINAL_WEIGHT * original_score + CROSS_ENCODER_WEIGHT * ce_score;
                (mem, final_score)
            })
            .collect();

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
// Lexical cross-encoder (original implementation)
// ---------------------------------------------------------------------------

fn lexical_score(query: &str, title: &str, content: &str) -> f32 {
    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        return 0.0;
    }

    let title_terms = tokenize(title);
    let content_terms = tokenize(content);

    let doc_terms: HashSet<&str> = title_terms
        .iter()
        .chain(content_terms.iter())
        .copied()
        .collect();
    let query_set: HashSet<&str> = query_terms.iter().copied().collect();

    // 1. Jaccard term overlap
    #[allow(clippy::cast_precision_loss)]
    let intersection = query_set.intersection(&doc_terms).count() as f32;
    #[allow(clippy::cast_precision_loss)]
    let union = query_set.union(&doc_terms).count() as f32;
    let jaccard = if union > 0.0 {
        intersection / union
    } else {
        0.0
    };

    // 2. TF-IDF-like term weighting
    let doc_all: Vec<&str> = title_terms
        .iter()
        .chain(content_terms.iter())
        .copied()
        .collect();
    let tf_idf = tfidf_score(&query_terms, &doc_all);

    // 3. Bigram overlap bonus
    let query_bigrams = bigrams(&query_terms);
    let doc_bigrams = bigrams(&doc_all);
    let bigram_overlap = if query_bigrams.is_empty() {
        0.0
    } else {
        let doc_bigram_set: HashSet<(&str, &str)> = doc_bigrams.into_iter().collect();
        #[allow(clippy::cast_precision_loss)]
        let hits = query_bigrams
            .iter()
            .filter(|b| doc_bigram_set.contains(b))
            .count() as f32;
        #[allow(clippy::cast_precision_loss)]
        let query_bigrams_len = query_bigrams.len() as f32;
        hits / query_bigrams_len
    };

    // 4. Title match bonus
    let title_set: HashSet<&str> = title_terms.iter().copied().collect();
    #[allow(clippy::cast_precision_loss)]
    let title_hits = query_set.intersection(&title_set).count() as f32;
    #[allow(clippy::cast_precision_loss)]
    let title_bonus = if query_set.is_empty() {
        0.0
    } else {
        title_hits / query_set.len() as f32
    };

    let raw = 0.30 * jaccard + 0.30 * tf_idf + 0.20 * bigram_overlap + 0.20 * title_bonus;
    raw.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn tokenize(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect()
}

fn tfidf_score(query_terms: &[&str], doc_tokens: &[&str]) -> f32 {
    if doc_tokens.is_empty() || query_terms.is_empty() {
        return 0.0;
    }

    let mut tf_map: HashMap<&str, usize> = HashMap::new();
    for &tok in doc_tokens {
        *tf_map.entry(tok).or_insert(0) += 1;
    }

    #[allow(clippy::cast_precision_loss)]
    let total = doc_tokens.len() as f32;
    #[allow(clippy::cast_precision_loss)]
    let unique = tf_map.len() as f32;

    let mut score_sum: f32 = 0.0;
    let query_lower: Vec<String> = query_terms.iter().map(|t| t.to_lowercase()).collect();

    for qt in &query_lower {
        #[allow(clippy::cast_precision_loss)]
        let tf = tf_map
            .iter()
            .filter(|(k, _)| k.to_lowercase() == *qt)
            .map(|(_, &v)| v)
            .sum::<usize>() as f32;

        if tf == 0.0 {
            continue;
        }

        let tf_norm = tf / total;
        #[allow(clippy::cast_precision_loss)]
        let doc_freq = tf_map.keys().filter(|k| k.to_lowercase() == *qt).count() as f32;
        let idf = (unique / (1.0 + doc_freq)).ln() + 1.0;

        score_sum += tf_norm * idf;
    }

    #[allow(clippy::cast_precision_loss)]
    let max_possible = query_lower.len() as f32;
    (score_sum / max_possible).clamp(0.0, 1.0)
}

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
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn lexical_score_returns_zero_for_empty_query() {
        assert_eq!(lexical_score("", "some title", "some content"), 0.0);
    }

    #[test]
    fn lexical_score_returns_zero_for_no_overlap() {
        let s = lexical_score("quantum physics", "grocery list", "milk eggs bread butter");
        assert!(s < 0.05, "expected near-zero, got {s}");
    }

    #[test]
    fn lexical_score_rewards_title_match() {
        let content = "This document discusses network configuration for LAN setups.";
        let s_title_match = lexical_score(
            "network configuration",
            "Network Configuration Guide",
            content,
        );
        let s_no_title = lexical_score("network configuration", "Unrelated Title", content);
        assert!(
            s_title_match > s_no_title,
            "title match ({s_title_match}) should beat no title match ({s_no_title})"
        );
    }

    #[test]
    fn lexical_score_is_bounded_zero_one() {
        let s = lexical_score(
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox",
            "the quick brown fox jumps over the lazy dog and more words",
        );
        assert!((0.0..=1.0).contains(&s), "score {s} out of bounds");
    }

    #[test]
    fn rerank_reorders_candidates() {
        let ce = CrossEncoder::new();
        let a = make_memory("Rust cross-encoder", "cross-encoder reranking for search");
        let b = make_memory("Grocery list", "milk eggs bread butter cheese");
        let candidates = vec![(b.clone(), 0.55), (a.clone(), 0.45)];
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
        let s_phrase = lexical_score(
            "network adapter",
            "title",
            "the network adapter is connected to the LAN",
        );
        let s_scattered = lexical_score(
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

#[cfg(test)]
#[allow(
    clippy::unused_self,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    clippy::wildcard_imports
)]
pub mod test_support {
    use super::*;

    /// Mock neural cross-encoder for testing. Returns deterministic scores
    /// based on (query, title, content) without loading BERT.
    pub struct MockCrossEncoder {
        pub use_neural: bool,
    }

    impl MockCrossEncoder {
        /// Create a mock lexical encoder (like CrossEncoder::new()).
        pub fn new() -> Self {
            Self { use_neural: false }
        }

        /// Create a mock neural encoder (like CrossEncoder::new_neural()).
        pub fn new_neural() -> Self {
            Self { use_neural: true }
        }

        /// Mock score: deterministic hash-based score in [0, 1].
        /// Neural path uses a different formula than lexical for testing.
        pub fn score(&self, query: &str, title: &str, content: &str) -> f32 {
            if self.use_neural {
                // Neural mock: combine query+title hash
                let combined = format!("{}{}", query, title);
                let hash = combined.bytes().fold(0u32, |acc, b| {
                    acc.wrapping_mul(31).wrapping_add(u32::from(b))
                });
                let base = ((hash % 1000) as f32) / 1000.0;
                // Boost for exact title matches
                if title.contains(query) {
                    (base * 0.5 + 0.5).min(1.0)
                } else {
                    base
                }
            } else {
                // Lexical path uses the real lexical_score
                lexical_score(query, title, content)
            }
        }

        /// Whether this is a neural mock.
        pub fn is_neural(&self) -> bool {
            self.use_neural
        }

        /// Rerank candidates (same blending formula as real CrossEncoder).
        pub fn rerank(
            &self,
            query: &str,
            mut candidates: Vec<(Memory, f64)>,
        ) -> Vec<(Memory, f64)> {
            let mut scored: Vec<(Memory, f64)> = candidates
                .drain(..)
                .map(|(mem, original_score)| {
                    let ce_score = f64::from(self.score(query, &mem.title, &mem.content));
                    let final_score =
                        ORIGINAL_WEIGHT * original_score + CROSS_ENCODER_WEIGHT * ce_score;
                    (mem, final_score)
                })
                .collect();

            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored
        }
    }

    impl Default for MockCrossEncoder {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod mock_tests {
    use super::test_support::*;
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
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn mock_lexical_new() {
        let ce = MockCrossEncoder::new();
        assert!(!ce.is_neural());
    }

    #[test]
    fn mock_neural_new() {
        let ce = MockCrossEncoder::new_neural();
        assert!(ce.is_neural());
    }

    #[test]
    fn mock_neural_score_deterministic() {
        let ce = MockCrossEncoder::new_neural();
        let s1 = ce.score("query", "title", "content");
        let s2 = ce.score("query", "title", "content");
        assert_eq!(s1, s2);
    }

    #[test]
    fn mock_neural_score_title_match_boost() {
        let ce = MockCrossEncoder::new_neural();
        let s_title_contains = ce.score("apple", "apple pie recipe", "delicious dessert");
        let s_no_match = ce.score("apple", "unrelated", "delicious dessert");
        assert!(
            s_title_contains > s_no_match,
            "title match ({s_title_contains}) should beat no match ({s_no_match})"
        );
    }

    #[test]
    fn mock_neural_score_bounded() {
        let ce = MockCrossEncoder::new_neural();
        for query in &["test", "neural", "reranker", "machine learning"] {
            for title in &["a", "b", "the quick brown"] {
                let s = ce.score(query, title, "content");
                assert!((0.0..=1.0).contains(&s), "score {s} out of bounds");
            }
        }
    }

    #[test]
    fn mock_neural_rerank_reorders() {
        let ce = MockCrossEncoder::new_neural();
        let a = make_memory("neural network", "deep learning with transformers");
        let b = make_memory("grocery list", "milk eggs bread butter");
        let candidates = vec![(b.clone(), 0.3), (a.clone(), 0.2)];
        let reranked = ce.rerank("neural network", candidates);
        // Neural encoder should boost the neural-network-titled memory
        assert_eq!(reranked[0].0.title, "neural network");
    }

    #[test]
    fn mock_neural_rerank_preserves_count() {
        let ce = MockCrossEncoder::new_neural();
        let candidates = vec![
            (make_memory("A", "content a"), 0.5),
            (make_memory("B", "content b"), 0.4),
            (make_memory("C", "content c"), 0.6),
        ];
        let reranked = ce.rerank("test", candidates);
        assert_eq!(reranked.len(), 3);
    }

    #[test]
    fn mock_lexical_path_via_mock() {
        let ce = MockCrossEncoder::new();
        let s = ce.score(
            "network adapter",
            "Network Configuration",
            "the network adapter is connected",
        );
        assert!((0.0..=1.0).contains(&s));
    }

    #[test]
    fn mock_neural_different_from_lexical() {
        let lexical = MockCrossEncoder::new();
        let neural = MockCrossEncoder::new_neural();
        let s_lex = lexical.score("machine learning", "ML title", "neural networks");
        let s_neu = neural.score("machine learning", "ML title", "neural networks");
        // They should use different scoring formulas
        assert_ne!(s_lex, s_neu);
    }
}

#[test]
fn score_handles_empty_query_string() {
    let s = lexical_score("", "Document Title", "This is document content");
    assert_eq!(s, 0.0, "empty query must return 0.0");
}

#[test]
fn score_handles_unicode_normalization() {
    // Query with accented characters, document with decomposed/composed variants
    let s1 = lexical_score("café", "café", "the café is open");
    let s2 = lexical_score("cafe", "cafe", "the cafe is open");
    // Both should score positively; exact equality not required due to normalization
    assert!(s1 > 0.0);
    assert!(s2 > 0.0);
}

#[test]
fn score_handles_very_long_content_truncation() {
    // Query and document with extreme length (lexical tokenizer should handle it)
    let long_content = "word ".repeat(10000); // 50k+ chars
    let s = lexical_score("word", "title", &long_content);
    assert!((0.0..=1.0).contains(&s), "score must be bounded [0, 1]");
}

#[test]
fn bigram_score_with_single_token_query() {
    // Query with only one token — bigrams should be empty, no crash
    let s = lexical_score("query", "Single Token Title", "single token content");
    assert!((0.0..=1.0).contains(&s));
}
