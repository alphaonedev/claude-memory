// Copyright (c) 2026 AlphaOne LLC. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Embedding models
// ---------------------------------------------------------------------------

/// Supported embedding models for semantic search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModel {
    /// sentence-transformers/all-MiniLM-L6-v2 — 384-dim, ~90 MB
    MiniLmL6V2,
    /// nomic-ai/nomic-embed-text-v1.5 — 768-dim, ~270 MB
    NomicEmbedV15,
}

impl EmbeddingModel {
    /// Embedding vector dimensionality.
    pub fn dim(&self) -> usize {
        match self {
            Self::MiniLmL6V2 => 384,
            Self::NomicEmbedV15 => 768,
        }
    }

    /// HuggingFace model identifier.
    pub fn hf_model_id(&self) -> &str {
        match self {
            Self::MiniLmL6V2 => "sentence-transformers/all-MiniLM-L6-v2",
            Self::NomicEmbedV15 => "nomic-ai/nomic-embed-text-v1.5",
        }
    }
}

// ---------------------------------------------------------------------------
// LLM models
// ---------------------------------------------------------------------------

/// Supported LLM models (served via Ollama).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmModel {
    /// Google Gemma 4 Effective 2B — ~1 GB Q4
    Gemma4E2B,
    /// Google Gemma 4 Effective 4B — ~2.3 GB Q4
    Gemma4E4B,
}

impl LlmModel {
    /// Ollama model tag used to pull / run this model.
    pub fn ollama_model_id(&self) -> &str {
        match self {
            Self::Gemma4E2B => "gemma4:e2b",
            Self::Gemma4E4B => "gemma4:e4b",
        }
    }

    /// Human-readable display name.
    pub fn display_name(&self) -> &str {
        match self {
            Self::Gemma4E2B => "Gemma 4 Effective 2B (Q4)",
            Self::Gemma4E4B => "Gemma 4 Effective 4B (Q4)",
        }
    }
}

// ---------------------------------------------------------------------------
// Feature tiers
// ---------------------------------------------------------------------------

/// Feature tiers control which AI capabilities are active based on the
/// available memory budget on the host machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureTier {
    /// FTS5 keyword search only — 0 MB extra.
    Keyword,
    /// MiniLM embeddings + HNSW index — ~256 MB.
    Semantic,
    /// nomic-embed + Gemma 4 E2B via Ollama — ~1 GB.
    Smart,
    /// nomic-embed + Gemma 4 E4B + cross-encoder via Ollama — ~4 GB.
    Autonomous,
}

impl FeatureTier {
    /// Parse a tier name (case-insensitive).
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "keyword" => Some(Self::Keyword),
            "semantic" => Some(Self::Semantic),
            "smart" => Some(Self::Smart),
            "autonomous" => Some(Self::Autonomous),
            _ => None,
        }
    }

    /// Canonical lowercase name.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Keyword => "keyword",
            Self::Semantic => "semantic",
            Self::Smart => "smart",
            Self::Autonomous => "autonomous",
        }
    }

    /// Build the full [`TierConfig`] for this tier.
    pub fn config(&self) -> TierConfig {
        match self {
            Self::Keyword => TierConfig {
                tier: *self,
                embedding_model: None,
                llm_model: None,
                cross_encoder: false,
                max_memory_mb: 0,
            },
            Self::Semantic => TierConfig {
                tier: *self,
                embedding_model: Some(EmbeddingModel::MiniLmL6V2),
                llm_model: None,
                cross_encoder: false,
                max_memory_mb: 256,
            },
            Self::Smart => TierConfig {
                tier: *self,
                embedding_model: Some(EmbeddingModel::NomicEmbedV15),
                llm_model: Some(LlmModel::Gemma4E2B),
                cross_encoder: false,
                max_memory_mb: 1024,
            },
            Self::Autonomous => TierConfig {
                tier: *self,
                embedding_model: Some(EmbeddingModel::NomicEmbedV15),
                llm_model: Some(LlmModel::Gemma4E4B),
                cross_encoder: true,
                max_memory_mb: 4096,
            },
        }
    }

    /// Automatically select the best tier that fits within `mb` megabytes.
    pub fn from_memory_budget(mb: usize) -> Self {
        if mb >= 4096 {
            Self::Autonomous
        } else if mb >= 1024 {
            Self::Smart
        } else if mb >= 256 {
            Self::Semantic
        } else {
            Self::Keyword
        }
    }
}

impl std::fmt::Display for FeatureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Tier configuration
// ---------------------------------------------------------------------------

/// Runtime configuration derived from a [`FeatureTier`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    pub tier: FeatureTier,
    pub embedding_model: Option<EmbeddingModel>,
    pub llm_model: Option<LlmModel>,
    pub cross_encoder: bool,
    pub max_memory_mb: usize,
}

impl TierConfig {
    /// Produce a [`Capabilities`] report suitable for JSON serialisation.
    pub fn capabilities(&self) -> Capabilities {
        let has_embeddings = self.embedding_model.is_some();
        let has_llm = self.llm_model.is_some();

        Capabilities {
            tier: self.tier.as_str().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: CapabilityFeatures {
                keyword_search: true,
                semantic_search: has_embeddings,
                hybrid_recall: has_embeddings,
                query_expansion: has_llm,
                auto_consolidation: has_llm,
                auto_tagging: has_llm,
                contradiction_analysis: has_llm,
                cross_encoder_reranking: self.cross_encoder,
                memory_reflection: self.cross_encoder && has_llm,
            },
            models: CapabilityModels {
                embedding: self
                    .embedding_model
                    .map(|m| m.hf_model_id().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                embedding_dim: self.embedding_model.map(|m| m.dim()).unwrap_or(0),
                llm: self
                    .llm_model
                    .map(|m| m.ollama_model_id().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                cross_encoder: if self.cross_encoder {
                    "cross-encoder/ms-marco-MiniLM-L-6-v2".to_string()
                } else {
                    "none".to_string()
                },
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Capability reporting
// ---------------------------------------------------------------------------

/// Top-level capabilities report for a running instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub tier: String,
    pub version: String,
    pub features: CapabilityFeatures,
    pub models: CapabilityModels,
}

/// Boolean feature flags exposed in the capabilities report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityFeatures {
    pub keyword_search: bool,
    pub semantic_search: bool,
    pub hybrid_recall: bool,
    pub query_expansion: bool,
    pub auto_consolidation: bool,
    pub auto_tagging: bool,
    pub contradiction_analysis: bool,
    pub cross_encoder_reranking: bool,
    pub memory_reflection: bool,
}

/// Model identifiers exposed in the capabilities report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityModels {
    pub embedding: String,
    pub embedding_dim: usize,
    pub llm: String,
    pub cross_encoder: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_roundtrip() {
        for tier in [
            FeatureTier::Keyword,
            FeatureTier::Semantic,
            FeatureTier::Smart,
            FeatureTier::Autonomous,
        ] {
            assert_eq!(FeatureTier::from_str(tier.as_str()), Some(tier));
        }
    }

    #[test]
    fn budget_selection() {
        assert_eq!(FeatureTier::from_memory_budget(0), FeatureTier::Keyword);
        assert_eq!(FeatureTier::from_memory_budget(128), FeatureTier::Keyword);
        assert_eq!(FeatureTier::from_memory_budget(256), FeatureTier::Semantic);
        assert_eq!(FeatureTier::from_memory_budget(512), FeatureTier::Semantic);
        assert_eq!(FeatureTier::from_memory_budget(1024), FeatureTier::Smart);
        assert_eq!(FeatureTier::from_memory_budget(2048), FeatureTier::Smart);
        assert_eq!(
            FeatureTier::from_memory_budget(4096),
            FeatureTier::Autonomous
        );
        assert_eq!(
            FeatureTier::from_memory_budget(8192),
            FeatureTier::Autonomous
        );
    }

    #[test]
    fn embedding_dimensions() {
        assert_eq!(EmbeddingModel::MiniLmL6V2.dim(), 384);
        assert_eq!(EmbeddingModel::NomicEmbedV15.dim(), 768);
    }

    #[test]
    fn autonomous_has_cross_encoder() {
        let cfg = FeatureTier::Autonomous.config();
        assert!(cfg.cross_encoder);
        assert!(cfg.capabilities().features.cross_encoder_reranking);
        assert!(cfg.capabilities().features.memory_reflection);
    }

    #[test]
    fn keyword_has_no_models() {
        let cfg = FeatureTier::Keyword.config();
        assert!(cfg.embedding_model.is_none());
        assert!(cfg.llm_model.is_none());
        assert!(!cfg.cross_encoder);
        assert_eq!(cfg.max_memory_mb, 0);
    }

    #[test]
    fn capabilities_serialize() {
        let caps = FeatureTier::Smart.config().capabilities();
        let json = serde_json::to_string_pretty(&caps).unwrap();
        assert!(json.contains("\"tier\": \"smart\""));
        assert!(json.contains("nomic"));
        assert!(json.contains("gemma4:e2b"));
    }
}
