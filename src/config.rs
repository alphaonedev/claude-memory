// Copyright (c) 2026 AlphaOne LLC. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::models::Tier;

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
// TTL configuration
// ---------------------------------------------------------------------------

/// Per-tier TTL overrides loaded from `[ttl]` section of config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TtlConfig {
    /// Short-tier default TTL in seconds (default: 21600 = 6 hours)
    pub short_ttl_secs: Option<i64>,
    /// Mid-tier default TTL in seconds (default: 604800 = 7 days)
    pub mid_ttl_secs: Option<i64>,
    /// Long-tier TTL in seconds (default: none = never expires). Set >0 to add expiry.
    pub long_ttl_secs: Option<i64>,
    /// Short-tier TTL extension on access in seconds (default: 3600 = 1 hour)
    pub short_extend_secs: Option<i64>,
    /// Mid-tier TTL extension on access in seconds (default: 86400 = 1 day)
    pub mid_extend_secs: Option<i64>,
}

/// Resolved TTL values after merging config overrides with compiled defaults.
#[derive(Debug, Clone)]
pub struct ResolvedTtl {
    pub short_ttl_secs: Option<i64>,
    pub mid_ttl_secs: Option<i64>,
    pub long_ttl_secs: Option<i64>,
    pub short_extend_secs: i64,
    pub mid_extend_secs: i64,
}

impl Default for ResolvedTtl {
    fn default() -> Self {
        Self {
            short_ttl_secs: Tier::Short.default_ttl_secs(),
            mid_ttl_secs: Tier::Mid.default_ttl_secs(),
            long_ttl_secs: Tier::Long.default_ttl_secs(),
            short_extend_secs: crate::models::SHORT_TTL_EXTEND_SECS,
            mid_extend_secs: crate::models::MID_TTL_EXTEND_SECS,
        }
    }
}

/// Maximum configurable TTL: 10 years in seconds. Prevents integer overflow
/// when adding Duration to Utc::now().
const MAX_TTL_SECS: i64 = 315_360_000;

impl ResolvedTtl {
    /// Build from optional config overrides, falling back to compiled defaults.
    /// TTL values are clamped to MAX_TTL_SECS (10 years) to prevent overflow.
    /// Extension values are clamped to non-negative.
    pub fn from_config(cfg: Option<&TtlConfig>) -> Self {
        let defaults = Self::default();
        let Some(c) = cfg else {
            return defaults;
        };
        let clamp_ttl = |v: i64| -> Option<i64> {
            if v <= 0 { None } else { Some(v.min(MAX_TTL_SECS)) }
        };
        Self {
            short_ttl_secs: c.short_ttl_secs.map(clamp_ttl).unwrap_or(defaults.short_ttl_secs),
            mid_ttl_secs: c.mid_ttl_secs.map(clamp_ttl).unwrap_or(defaults.mid_ttl_secs),
            long_ttl_secs: c.long_ttl_secs.map(clamp_ttl).unwrap_or(defaults.long_ttl_secs),
            short_extend_secs: c.short_extend_secs.unwrap_or(defaults.short_extend_secs).max(0),
            mid_extend_secs: c.mid_extend_secs.unwrap_or(defaults.mid_extend_secs).max(0),
        }
    }

    /// Get the default TTL for a given tier.
    pub fn ttl_for_tier(&self, tier: &Tier) -> Option<i64> {
        match tier {
            Tier::Short => self.short_ttl_secs,
            Tier::Mid => self.mid_ttl_secs,
            Tier::Long => self.long_ttl_secs,
        }
    }

    /// Get the TTL extension on access for a given tier.
    pub fn extend_for_tier(&self, tier: &Tier) -> Option<i64> {
        match tier {
            Tier::Short => Some(self.short_extend_secs),
            Tier::Mid => Some(self.mid_extend_secs),
            Tier::Long => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent config file (~/.config/ai-memory/config.toml)
// ---------------------------------------------------------------------------

const CONFIG_DIR: &str = ".config/ai-memory";
const CONFIG_FILE: &str = "config.toml";

/// Persistent configuration loaded from `~/.config/ai-memory/config.toml`.
///
/// All fields are optional — CLI flags override file values, which override
/// compiled defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Feature tier: keyword, semantic, smart, autonomous
    pub tier: Option<String>,
    /// Path to the SQLite database file
    pub db: Option<String>,
    /// Ollama base URL for LLM generation (default: http://localhost:11434)
    pub ollama_url: Option<String>,
    /// Separate URL for embedding model (defaults to ollama_url if unset)
    pub embed_url: Option<String>,
    /// Embedding model override: mini_lm_l6_v2 or nomic_embed_v15
    pub embedding_model: Option<String>,
    /// LLM model override (Ollama tag, e.g. "gemma4:e2b")
    pub llm_model: Option<String>,
    /// Enable cross-encoder reranking (true/false)
    pub cross_encoder: Option<bool>,
    /// Default namespace for new memories
    pub default_namespace: Option<String>,
    /// Maximum memory budget in MB (used for auto tier selection)
    pub max_memory_mb: Option<usize>,
    /// Per-tier TTL overrides
    pub ttl: Option<TtlConfig>,
    /// Archive memories before GC deletion (default: true)
    pub archive_on_gc: Option<bool>,
}

impl AppConfig {
    /// Returns the config file path: `~/.config/ai-memory/config.toml`
    pub fn config_path() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        Some(Path::new(&home).join(CONFIG_DIR).join(CONFIG_FILE))
    }

    /// Load config from disk. Returns `AppConfig::default()` if file is missing.
    /// Set `AI_MEMORY_NO_CONFIG=1` to skip config loading (used by integration tests).
    pub fn load() -> Self {
        if std::env::var("AI_MEMORY_NO_CONFIG").is_ok() {
            return Self::default();
        }
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    /// Load config from a specific path.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(cfg) => {
                    eprintln!("ai-memory: loaded config from {}", path.display());
                    cfg
                }
                Err(e) => {
                    eprintln!("ai-memory: config parse error ({}): {}", path.display(), e);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Resolve the effective feature tier from config (CLI flag overrides).
    pub fn effective_tier(&self, cli_tier: Option<&str>) -> FeatureTier {
        let tier_str = cli_tier.or(self.tier.as_deref()).unwrap_or("semantic");
        FeatureTier::from_str(tier_str).unwrap_or(FeatureTier::Semantic)
    }

    /// Resolve the effective database path (CLI flag overrides config).
    pub fn effective_db(&self, cli_db: &Path) -> PathBuf {
        // If CLI provided a non-default path, use it
        let default_db = PathBuf::from("ai-memory.db");
        if cli_db != default_db {
            return cli_db.to_path_buf();
        }
        // Otherwise check config
        self.db
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cli_db.to_path_buf())
    }

    /// Resolve Ollama URL for LLM generation (config or default).
    pub fn effective_ollama_url(&self) -> &str {
        self.ollama_url
            .as_deref()
            .unwrap_or("http://localhost:11434")
    }

    /// Resolve TTL configuration from config file, falling back to compiled defaults.
    pub fn effective_ttl(&self) -> ResolvedTtl {
        ResolvedTtl::from_config(self.ttl.as_ref())
    }

    /// Whether to archive memories before GC deletion (default: true).
    pub fn effective_archive_on_gc(&self) -> bool {
        self.archive_on_gc.unwrap_or(true)
    }

    /// Resolve URL for embedding model (falls back to ollama_url).
    pub fn effective_embed_url(&self) -> &str {
        self.embed_url
            .as_deref()
            .or(self.ollama_url.as_deref())
            .unwrap_or("http://localhost:11434")
    }

    /// Write a default config file if one doesn't exist yet.
    pub fn write_default_if_missing() {
        let Some(path) = Self::config_path() else {
            return;
        };
        if path.exists() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let default_toml = r#"# ai-memory configuration
# See: https://github.com/alphaonedev/ai-memory-mcp

# Feature tier: keyword, semantic, smart, autonomous
# tier = "semantic"

# Path to SQLite database
# db = "~/.claude/ai-memory.db"

# Ollama base URL (for smart/autonomous tiers)
# ollama_url = "http://localhost:11434"

# Embedding model: mini_lm_l6_v2 (384-dim) or nomic_embed_v15 (768-dim)
# embedding_model = "mini_lm_l6_v2"

# LLM model tag for Ollama
# llm_model = "gemma4:e2b"

# Enable neural cross-encoder reranking (autonomous tier)
# cross_encoder = true

# Default namespace for new memories
# default_namespace = "global"

# Memory budget in MB (for auto tier selection)
# max_memory_mb = 4096

# Archive expired memories before GC deletion (default: true)
# archive_on_gc = true

# Per-tier TTL overrides (uncomment to customize)
# [ttl]
# short_ttl_secs = 21600        # 6 hours (default)
# mid_ttl_secs = 604800         # 7 days (default)
# long_ttl_secs = 0             # 0 = never expires (default)
# short_extend_secs = 3600      # +1h on access (default)
# mid_extend_secs = 86400       # +1d on access (default)
"#;
        let _ = std::fs::write(&path, default_toml);
    }
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

    #[test]
    fn config_default_is_empty() {
        let cfg = AppConfig::default();
        assert!(cfg.tier.is_none());
        assert!(cfg.db.is_none());
        assert!(cfg.ollama_url.is_none());
    }

    #[test]
    fn config_parse_toml() {
        let toml_str = r#"
            tier = "smart"
            db = "/tmp/test.db"
            ollama_url = "http://localhost:11434"
            cross_encoder = true
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.tier.as_deref(), Some("smart"));
        assert_eq!(cfg.db.as_deref(), Some("/tmp/test.db"));
        assert!(cfg.cross_encoder.unwrap());
    }

    #[test]
    fn resolved_ttl_defaults_match_hardcoded() {
        let resolved = ResolvedTtl::default();
        assert_eq!(resolved.short_ttl_secs, Some(6 * 3600));
        assert_eq!(resolved.mid_ttl_secs, Some(7 * 24 * 3600));
        assert_eq!(resolved.long_ttl_secs, None);
        assert_eq!(resolved.short_extend_secs, 3600);
        assert_eq!(resolved.mid_extend_secs, 86400);
    }

    #[test]
    fn resolved_ttl_from_partial_config() {
        let cfg = TtlConfig {
            mid_ttl_secs: Some(90 * 24 * 3600), // ~3 months
            ..Default::default()
        };
        let resolved = ResolvedTtl::from_config(Some(&cfg));
        assert_eq!(resolved.short_ttl_secs, Some(6 * 3600)); // unchanged
        assert_eq!(resolved.mid_ttl_secs, Some(90 * 24 * 3600)); // overridden
        assert_eq!(resolved.long_ttl_secs, None); // unchanged
    }

    #[test]
    fn resolved_ttl_zero_means_no_expiry() {
        let cfg = TtlConfig {
            short_ttl_secs: Some(0),
            mid_ttl_secs: Some(0),
            ..Default::default()
        };
        let resolved = ResolvedTtl::from_config(Some(&cfg));
        assert_eq!(resolved.short_ttl_secs, None); // 0 → no expiry
        assert_eq!(resolved.mid_ttl_secs, None);
    }

    #[test]
    fn resolved_ttl_clamps_overflow() {
        let cfg = TtlConfig {
            mid_ttl_secs: Some(i64::MAX),
            short_extend_secs: Some(-3600),
            ..Default::default()
        };
        let resolved = ResolvedTtl::from_config(Some(&cfg));
        // i64::MAX should be clamped to MAX_TTL_SECS (10 years)
        assert_eq!(resolved.mid_ttl_secs, Some(super::MAX_TTL_SECS));
        // negative extend should be clamped to 0
        assert_eq!(resolved.short_extend_secs, 0);
    }

    #[test]
    fn ttl_config_parse_toml() {
        let toml_str = r#"
            tier = "semantic"
            archive_on_gc = false
            [ttl]
            mid_ttl_secs = 7776000
            short_extend_secs = 7200
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.ttl.as_ref().unwrap().mid_ttl_secs, Some(7776000));
        assert_eq!(cfg.ttl.as_ref().unwrap().short_extend_secs, Some(7200));
        assert!(!cfg.effective_archive_on_gc());
    }

    #[test]
    fn resolved_ttl_tier_methods() {
        let resolved = ResolvedTtl::default();
        assert_eq!(resolved.ttl_for_tier(&Tier::Short), Some(6 * 3600));
        assert_eq!(resolved.ttl_for_tier(&Tier::Mid), Some(7 * 24 * 3600));
        assert_eq!(resolved.ttl_for_tier(&Tier::Long), None);
        assert_eq!(resolved.extend_for_tier(&Tier::Short), Some(3600));
        assert_eq!(resolved.extend_for_tier(&Tier::Mid), Some(86400));
        assert_eq!(resolved.extend_for_tier(&Tier::Long), None);
    }

    #[test]
    fn config_effective_tier() {
        let cfg = AppConfig {
            tier: Some("smart".to_string()),
            ..Default::default()
        };
        // CLI override wins
        assert_eq!(
            cfg.effective_tier(Some("autonomous")),
            FeatureTier::Autonomous
        );
        // Config value used when no CLI
        assert_eq!(cfg.effective_tier(None), FeatureTier::Smart);
    }
}
