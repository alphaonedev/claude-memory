// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

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
    pub fn dim(self) -> usize {
        match self {
            Self::MiniLmL6V2 => 384,
            Self::NomicEmbedV15 => 768,
        }
    }

    /// `HuggingFace` model identifier.
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
    /// `MiniLM` embeddings + HNSW index — ~256 MB.
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
    pub fn config(self) -> TierConfig {
        match self {
            Self::Keyword => TierConfig {
                tier: self,
                embedding_model: None,
                llm_model: None,
                cross_encoder: false,
                max_memory_mb: 0,
            },
            Self::Semantic => TierConfig {
                tier: self,
                embedding_model: Some(EmbeddingModel::MiniLmL6V2),
                llm_model: None,
                cross_encoder: false,
                max_memory_mb: 256,
            },
            Self::Smart => TierConfig {
                tier: self,
                embedding_model: Some(EmbeddingModel::NomicEmbedV15),
                llm_model: Some(LlmModel::Gemma4E2B),
                cross_encoder: false,
                max_memory_mb: 1024,
            },
            Self::Autonomous => TierConfig {
                tier: self,
                embedding_model: Some(EmbeddingModel::NomicEmbedV15),
                llm_model: Some(LlmModel::Gemma4E4B),
                cross_encoder: true,
                max_memory_mb: 4096,
            },
        }
    }

    /// Automatically select the best tier that fits within `mb` megabytes.
    #[allow(dead_code)]
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
    /// Produce a [`Capabilities`] (schema v2) report suitable for JSON
    /// serialisation. The MCP / HTTP `handle_capabilities_with_conn`
    /// wrapper overlays live runtime state (recall mode, reranker mode,
    /// embedder-loaded flag) and live DB counts (active rules, hook
    /// registrations, pending approvals) before the report goes on the
    /// wire.
    ///
    /// v2 honesty patch (P1, v0.6.3.1): `recall_mode_active` and
    /// `reranker_active` start at conservative defaults (`disabled` /
    /// `off`); the wrapper updates them based on the *runtime* embedder
    /// + reranker handles, not the *configured* tier values.
    pub fn capabilities(&self) -> Capabilities {
        let has_embeddings = self.embedding_model.is_some();
        let has_llm = self.llm_model.is_some();

        Capabilities {
            // Capabilities schema v2 — see `Capabilities` doc comment.
            schema_version: "2".to_string(),
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
                // Honesty patch: planned-not-implemented. The flag was
                // previously a `bool` whose `true` value implied a wired
                // feature that does not exist in this build.
                memory_reflection: PlannedFeature::planned("v0.7+"),
                // Default false — the HTTP/MCP capabilities handler
                // overwrites this with the live runtime state when it
                // has access to the embedder handle.
                embedder_loaded: false,
                // Conservative defaults; the handler wrapper overlays the
                // live runtime state (`hybrid` when embedder is loaded,
                // `keyword_only` when it is not, `degraded` if the load
                // failed, `disabled` for the keyword tier).
                recall_mode_active: RecallMode::Disabled,
                // Conservative default; overwritten when the wrapper has
                // the actual reranker handle. `off` means no reranker is
                // configured; `lexical_fallback` means the neural model
                // failed to materialize; `neural` means the BERT
                // cross-encoder is loaded.
                reranker_active: RerankerMode::Off,
            },
            models: CapabilityModels {
                embedding: self
                    .embedding_model
                    .map_or_else(|| "none".to_string(), |m| m.hf_model_id().to_string()),
                embedding_dim: self.embedding_model.map_or(0, EmbeddingModel::dim),
                llm: self
                    .llm_model
                    .map_or_else(|| "none".to_string(), |m| m.ollama_model_id().to_string()),
                cross_encoder: if self.cross_encoder {
                    "cross-encoder/ms-marco-MiniLM-L-6-v2".to_string()
                } else {
                    "none".to_string()
                },
            },
            // v2 dynamic blocks — start at zero-state defaults. The MCP
            // and HTTP `handle_capabilities` wrappers overwrite these
            // with live counts when they have a `&Connection` handle.
            //
            // Honesty patch (P1): `permissions.mode` is `"advisory"`
            // until P4 lands the enforcement gate. Was `"ask"`, which
            // implied an active prompt loop that does not exist.
            // `rule_summary`, `hooks.by_event`, `approval.subscribers`,
            // and `approval.default_timeout_seconds` were dropped in v2
            // because they have no backing implementation.
            permissions: CapabilityPermissions {
                mode: "advisory".to_string(),
                active_rules: 0,
            },
            hooks: CapabilityHooks::default(),
            compaction: CapabilityCompaction::planned(),
            approval: CapabilityApproval {
                pending_requests: 0,
            },
            transcripts: CapabilityTranscripts::planned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Capability reporting
// ---------------------------------------------------------------------------

/// Top-level capabilities report for a running instance.
///
/// Schema versions:
/// - **v1** (legacy, pre-v0.6.3.1): `tier`, `version`, `features`,
///   `models`. Reachable via `Accept-Capabilities: v1` (HTTP) or the MCP
///   `accept` argument set to `"v1"`. See [`CapabilitiesV1`].
/// - **v2** (v0.6.3.1 honesty patch): `schema_version="2"` plus the
///   `permissions`, `hooks`, `compaction`, `approval`, `transcripts`
///   blocks. v1 fields preserved at the same top-level paths — old
///   clients that read v2 by name continue to work for the un-dropped
///   fields. Default response shape.
///
/// **v2 honesty patch (P1, v0.6.3.1):**
/// - `features.recall_mode_active` and `features.reranker_active` are
///   *runtime* state, not config-derived flags.
/// - `features.memory_reflection` is now a `{planned, version, enabled}`
///   object, not a `bool`.
/// - `compaction` and `transcripts` carry the same planned-feature
///   shape so operators can distinguish "disabled but built" from "not
///   in this build."
/// - `permissions.mode = "advisory"` until the enforcement gate ships
///   in P4. Was `"ask"`, which implied an active interactive loop.
/// - The following fields were **removed** because no backing
///   implementation exists: `permissions.rule_summary`,
///   `hooks.by_event`, `approval.subscribers`,
///   `approval.default_timeout_seconds`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    /// Schema-version discriminator. Always `"2"` since v0.6.3.
    pub schema_version: String,
    pub tier: String,
    pub version: String,
    pub features: CapabilityFeatures,
    pub models: CapabilityModels,

    /// Active permission/governance rules. Pre-P4 reports the count of
    /// namespaces that have a `metadata.governance` policy attached to
    /// their standard memory; the underlying permission system itself
    /// is P4 work.
    pub permissions: CapabilityPermissions,

    /// Registered hooks. Pre-v0.7 reports webhook subscriptions as a
    /// proxy (hook system itself is v0.7 Bucket 0).
    pub hooks: CapabilityHooks,

    /// Compaction state. v0.8 work — reports `{planned, version,
    /// enabled}` until the subsystem ships.
    pub compaction: CapabilityCompaction,

    /// Approval API state. Reports the live count of pending actions
    /// from the existing `pending_actions` table.
    pub approval: CapabilityApproval,

    /// Sidechain-transcript state. v0.7 Bucket 1.7 work — reports
    /// `{planned, version, enabled}` until the subsystem ships.
    pub transcripts: CapabilityTranscripts,
}

/// Live recall-mode tag (P1 honesty patch). Reflects the *runtime*
/// state of the embedder + LLM, not the configured tier.
///
/// - `Hybrid` — embedder loaded; semantic + keyword blending active.
/// - `KeywordOnly` — no embedder loaded; FTS5 only.
/// - `Degraded` — embedder configured but `Embedder::load()` failed
///   (offline runner, read-only fs, missing HF token, etc.).
/// - `Disabled` — keyword-tier daemon, semantic recall not configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallMode {
    Hybrid,
    KeywordOnly,
    Degraded,
    Disabled,
}

/// Live reranker-mode tag (P1 honesty patch). Reflects the *runtime*
/// `CrossEncoder` enum variant, not the configured `cross_encoder` flag.
///
/// - `Neural` — `CrossEncoder::Neural` loaded successfully.
/// - `LexicalFallback` — `cross_encoder` was requested but neural model
///   download or load failed; running on the lexical scorer.
/// - `Off` — no reranker handle in the daemon (non-autonomous tier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankerMode {
    Neural,
    LexicalFallback,
    Off,
}

/// Generic "planned but not implemented" marker used by v2 capability
/// fields whose underlying subsystem is on the roadmap but not in this
/// build. Operators reading the JSON can distinguish "disabled but
/// available" from "not in this build" by inspecting `planned`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannedFeature {
    /// `true` when the feature exists only on the roadmap.
    pub planned: bool,
    /// Earliest release that is expected to ship the feature, e.g.
    /// `"v0.7+"` or `"v0.8+"`. Free-form string; clients should treat
    /// it as advisory.
    pub version: String,
    /// `true` only when the feature is built **and** turned on in this
    /// daemon. Always `false` when `planned == true`.
    pub enabled: bool,
}

impl PlannedFeature {
    /// A planned-not-yet-shipped feature. `enabled = false`.
    #[must_use]
    pub fn planned(version: &str) -> Self {
        Self {
            planned: true,
            version: version.to_string(),
            enabled: false,
        }
    }
}

/// Boolean feature flags exposed in the capabilities report.
#[allow(clippy::struct_excessive_bools)]
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
    /// Memory-reflection (v0.7+): planned, not yet implemented.
    /// Was a `bool` before the P1 honesty patch; an object now so
    /// operators can tell "feature exists but disabled" apart from
    /// "feature not in this build".
    pub memory_reflection: PlannedFeature,
    /// v0.6.2 (S18): runtime-observed embedder state. `semantic_search`
    /// above reflects *configured* capability (derived from the tier's
    /// `embedding_model` setting). `embedder_loaded` reflects *actual*
    /// state after `Embedder::load()` attempted to materialize the
    /// `HuggingFace` model on startup. When an operator configures the
    /// `semantic` tier but the model download or mmap fails (offline
    /// runner, read-only fs, missing tokens), `semantic_search=true`
    /// would mislead. This flag exposes the truth so setup scripts can
    /// assert the daemon is actually ready for semantic recall before
    /// dispatching scenarios. Default false; populated by
    /// `handle_capabilities` when the HTTP/MCP wrapper hands in the
    /// live embedder handle.
    #[serde(default)]
    pub embedder_loaded: bool,
    /// v0.6.3.1 (P1 honesty patch): runtime recall-mode tag. Reflects
    /// the live embedder + LLM availability, not the configured tier.
    /// See [`RecallMode`].
    #[serde(default = "default_recall_mode")]
    pub recall_mode_active: RecallMode,
    /// v0.6.3.1 (P1 honesty patch): runtime reranker-mode tag.
    /// Reflects the live `CrossEncoder` variant. See [`RerankerMode`].
    #[serde(default = "default_reranker_mode")]
    pub reranker_active: RerankerMode,
}

fn default_recall_mode() -> RecallMode {
    RecallMode::Disabled
}

fn default_reranker_mode() -> RerankerMode {
    RerankerMode::Off
}

/// Model identifiers exposed in the capabilities report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityModels {
    pub embedding: String,
    pub embedding_dim: usize,
    pub llm: String,
    pub cross_encoder: String,
}

/// Permissions block (capabilities schema v2). Pre-P4 reports a live
/// count of namespace standards carrying a `metadata.governance` policy;
/// the full enforcement gate lands in P4. The honesty patch (P1)
/// renames the mode from `"ask"` (which implied an interactive prompt
/// loop) to `"advisory"` (governance metadata is recorded but not
/// enforced).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityPermissions {
    /// Enforcement mode. `"advisory"` until P4 ships the gate.
    pub mode: String,
    /// Number of namespace standards whose `metadata.governance` is
    /// non-null. Counts policies, not memories.
    pub active_rules: usize,
    // P1 honesty patch: `rule_summary` was always empty — no per-rule
    // serializer existed. Dropped from the v2 wire schema.
}

/// Hook-pipeline block (capabilities schema v2). Pre-v0.7 reports webhook
/// subscriptions as the closest analogue. The full hook pipeline lands in
/// v0.7 Bucket 0 (arch-enhancement-spec §2).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityHooks {
    /// Number of registered hook subscribers (proxy: webhook subscriptions).
    pub registered_count: usize,
    // P1 honesty patch: `by_event` was always an empty map — no event
    // registry exists. Dropped from the v2 wire schema.
}

/// Compaction block (capabilities schema v2). v0.8 Pillar 2.5 work —
/// reports `{planned, version, enabled}` plus optional run stats. The
/// honesty patch (P1) replaced the bare `enabled: false` with the
/// planned-feature shape so operators can distinguish "feature exists
/// but disabled" from "feature not in this build".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityCompaction {
    /// Planned-feature marker. `planned = true` while compaction lives
    /// only on the roadmap. When the subsystem ships the daemon will
    /// flip `planned = false` and `enabled` will reflect runtime state.
    #[serde(flatten)]
    pub status: PlannedFeature,
    /// Once shipped: scheduled compaction interval in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_minutes: Option<u64>,
    /// Once shipped: timestamp of the most recent compaction run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    /// Once shipped: arbitrary JSON describing the most recent run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_stats: Option<serde_json::Value>,
}

impl CapabilityCompaction {
    /// Pre-v0.8 zero-state: planned, not enabled.
    #[must_use]
    pub fn planned() -> Self {
        Self {
            status: PlannedFeature::planned("v0.8+"),
            interval_minutes: None,
            last_run_at: None,
            last_run_stats: None,
        }
    }
}

impl Default for CapabilityCompaction {
    fn default() -> Self {
        Self::planned()
    }
}

/// Approval-API block (capabilities schema v2). `pending_requests`
/// counts the existing `pending_actions` table (live signal).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityApproval {
    /// Live count of `pending_actions` with status='pending'.
    pub pending_requests: usize,
    // P1 honesty patch: `subscribers` (no subscription API exists) and
    // `default_timeout_seconds` (no sweeper enforces timeouts) dropped
    // from the v2 wire schema.
}

/// Sidechain-transcript block (capabilities schema v2). v0.7 Bucket 1.7
/// work — reports `{planned, version, enabled}` until the subsystem
/// ships. The honesty patch (P1) replaced the bare `enabled: false`
/// with the planned-feature shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityTranscripts {
    /// Planned-feature marker. `planned = true` while sidechain
    /// transcripts live only on the roadmap.
    #[serde(flatten)]
    pub status: PlannedFeature,
    /// Once shipped: number of stored transcripts.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub total_count: usize,
    /// Once shipped: total transcript storage in megabytes.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_size_mb: u64,
}

impl CapabilityTranscripts {
    /// Pre-v0.7 zero-state: planned, not enabled.
    #[must_use]
    pub fn planned() -> Self {
        Self {
            status: PlannedFeature::planned("v0.7+"),
            total_count: 0,
            total_size_mb: 0,
        }
    }
}

impl Default for CapabilityTranscripts {
    fn default() -> Self {
        Self::planned()
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

// ---------------------------------------------------------------------------
// Capabilities v1 — legacy shape retained for backward compat
// ---------------------------------------------------------------------------

/// Legacy (v1) capabilities shape — the structure shipped before the
/// v0.6.3.1 honesty patch. Returned only when a client opts in via
/// `Accept-Capabilities: v1` (HTTP) or the MCP `accept` argument set
/// to `"v1"`. Default response is v2.
///
/// The v1 schema is frozen — do not extend it. New fields go into v2
/// (see [`Capabilities`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesV1 {
    pub tier: String,
    pub version: String,
    pub features: CapabilityFeaturesV1,
    pub models: CapabilityModels,
}

/// Legacy v1 feature-flag block. Notably, `memory_reflection` is a
/// `bool` here (it became a `PlannedFeature` object in v2).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityFeaturesV1 {
    pub keyword_search: bool,
    pub semantic_search: bool,
    pub hybrid_recall: bool,
    pub query_expansion: bool,
    pub auto_consolidation: bool,
    pub auto_tagging: bool,
    pub contradiction_analysis: bool,
    pub cross_encoder_reranking: bool,
    pub memory_reflection: bool,
    #[serde(default)]
    pub embedder_loaded: bool,
}

impl Capabilities {
    /// Project the v2 report down to the legacy v1 shape. Used to
    /// honour `Accept-Capabilities: v1` from older clients.
    ///
    /// `memory_reflection` collapses from `{planned, enabled}` to a
    /// single bool (`enabled` value). All v2-only fields
    /// (`recall_mode_active`, `reranker_active`, `permissions`,
    /// `hooks`, `compaction`, `approval`, `transcripts`) are dropped.
    #[must_use]
    pub fn to_v1(&self) -> CapabilitiesV1 {
        CapabilitiesV1 {
            tier: self.tier.clone(),
            version: self.version.clone(),
            features: CapabilityFeaturesV1 {
                keyword_search: self.features.keyword_search,
                semantic_search: self.features.semantic_search,
                hybrid_recall: self.features.hybrid_recall,
                query_expansion: self.features.query_expansion,
                auto_consolidation: self.features.auto_consolidation,
                auto_tagging: self.features.auto_tagging,
                contradiction_analysis: self.features.contradiction_analysis,
                cross_encoder_reranking: self.features.cross_encoder_reranking,
                memory_reflection: self.features.memory_reflection.enabled,
                embedder_loaded: self.features.embedder_loaded,
            },
            models: self.models.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// TTL configuration
// ---------------------------------------------------------------------------

/// Per-tier TTL overrides loaded from `[ttl]` section of config.toml.
#[allow(clippy::struct_field_names)]
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
#[allow(clippy::struct_field_names)]
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
/// when adding Duration to `Utc::now()`.
const MAX_TTL_SECS: i64 = 315_360_000;

#[allow(dead_code)]
impl ResolvedTtl {
    /// Build from optional config overrides, falling back to compiled defaults.
    /// TTL values are clamped to `MAX_TTL_SECS` (10 years) to prevent overflow.
    /// Extension values are clamped to non-negative.
    pub fn from_config(cfg: Option<&TtlConfig>) -> Self {
        let defaults = Self::default();
        let Some(c) = cfg else {
            return defaults;
        };
        let clamp_ttl = |v: i64| -> Option<i64> {
            if v <= 0 {
                None
            } else {
                Some(v.min(MAX_TTL_SECS))
            }
        };
        Self {
            short_ttl_secs: c.short_ttl_secs.map_or(defaults.short_ttl_secs, clamp_ttl),
            mid_ttl_secs: c.mid_ttl_secs.map_or(defaults.mid_ttl_secs, clamp_ttl),
            long_ttl_secs: c.long_ttl_secs.map_or(defaults.long_ttl_secs, clamp_ttl),
            short_extend_secs: c
                .short_extend_secs
                .unwrap_or(defaults.short_extend_secs)
                .max(0),
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
// Recall scoring (time-decay half-life) — v0.6.0.0
// ---------------------------------------------------------------------------

/// Per-tier half-life (days) overrides loaded from `[scoring]` section of
/// `config.toml`.
///
/// The half-life is the number of days it takes for a memory's recall score
/// to drop to 50% of its undecayed value. Shorter half-lives prioritize fresh
/// memories; longer half-lives give older memories more weight. Defaults are
/// chosen so each tier's decay curve matches its retention expectations:
/// `short` memories decay quickly (7 d), `mid` moderately (30 d), `long`
/// slowly (365 d).
///
/// Setting `legacy_scoring = true` disables the decay multiplier entirely,
/// restoring the pre-v0.6.0.0 blended-score behavior for A/B comparison or
/// if a recall-quality regression is reported.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecallScoringConfig {
    /// Half-life for `short`-tier memories, in days (default 7).
    pub half_life_days_short: Option<f64>,
    /// Half-life for `mid`-tier memories, in days (default 30).
    pub half_life_days_mid: Option<f64>,
    /// Half-life for `long`-tier memories, in days (default 365).
    pub half_life_days_long: Option<f64>,
    /// When true, skip the decay multiplier entirely. Default false.
    #[serde(default)]
    pub legacy_scoring: bool,
}

/// Resolved scoring values after merging config overrides with compiled
/// defaults. Half-lives are clamped to the range `[0.1, 36_500.0]` days
/// (≈100 years) to keep the decay math well-behaved.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedScoring {
    pub half_life_days_short: f64,
    pub half_life_days_mid: f64,
    pub half_life_days_long: f64,
    pub legacy_scoring: bool,
}

impl Default for ResolvedScoring {
    fn default() -> Self {
        Self {
            half_life_days_short: 7.0,
            half_life_days_mid: 30.0,
            half_life_days_long: 365.0,
            legacy_scoring: false,
        }
    }
}

impl ResolvedScoring {
    const MIN_HALF_LIFE: f64 = 0.1;
    const MAX_HALF_LIFE: f64 = 36_500.0;

    /// Build from optional config overrides, falling back to compiled
    /// defaults. Out-of-range values are silently clamped.
    pub fn from_config(cfg: Option<&RecallScoringConfig>) -> Self {
        let defaults = Self::default();
        let Some(c) = cfg else {
            return defaults;
        };
        let clamp = |v: f64| -> f64 { v.clamp(Self::MIN_HALF_LIFE, Self::MAX_HALF_LIFE) };
        Self {
            half_life_days_short: c
                .half_life_days_short
                .map_or(defaults.half_life_days_short, clamp),
            half_life_days_mid: c
                .half_life_days_mid
                .map_or(defaults.half_life_days_mid, clamp),
            half_life_days_long: c
                .half_life_days_long
                .map_or(defaults.half_life_days_long, clamp),
            legacy_scoring: c.legacy_scoring,
        }
    }

    /// Half-life in days for a given tier.
    pub fn half_life_for_tier(&self, tier: &Tier) -> f64 {
        match tier {
            Tier::Short => self.half_life_days_short,
            Tier::Mid => self.half_life_days_mid,
            Tier::Long => self.half_life_days_long,
        }
    }

    /// Compute the decay multiplier `exp(-ln(2) * age_days / half_life)`
    /// for a memory of the given tier and age. Returns `1.0` when
    /// `legacy_scoring` is true (no decay) or when `age_days` is non-positive
    /// (future timestamps, clock skew, or new memories).
    #[must_use]
    pub fn decay_multiplier(&self, tier: &Tier, age_days: f64) -> f64 {
        if self.legacy_scoring || age_days <= 0.0 {
            return 1.0;
        }
        let half_life = self.half_life_for_tier(tier);
        (-std::f64::consts::LN_2 * age_days / half_life).exp()
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
    /// Path to the `SQLite` database file
    pub db: Option<String>,
    /// Ollama base URL for LLM generation (default: <http://localhost:11434>)
    pub ollama_url: Option<String>,
    /// Separate URL for embedding model (defaults to `ollama_url` if unset)
    pub embed_url: Option<String>,
    /// Embedding model override: `mini_lm_l6_v2` or `nomic_embed_v15`
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
    /// Optional API key for HTTP API authentication
    pub api_key: Option<String>,
    /// Maximum archive age in days for automatic purge during GC (default: disabled)
    pub archive_max_days: Option<i64>,
    /// Identity-resolution overrides (Task 1.2 follow-up #198).
    pub identity: Option<IdentityConfig>,
    /// Recall scoring — per-tier half-life for time-decay, and `legacy_scoring`
    /// kill switch (v0.6.0.0).
    pub scoring: Option<RecallScoringConfig>,
    /// v0.6.0.0: when true, fire LLM autonomy hooks (`auto_tag` +
    /// `detect_contradiction`) synchronously on every successful
    /// `memory_store`. Off by default — the hook blocks store latency
    /// behind an Ollama round-trip. `AI_MEMORY_AUTONOMOUS_HOOKS=1`
    /// env var overrides the config file.
    pub autonomous_hooks: Option<bool>,
}

/// Identity-resolution configuration (Task 1.2 follow-up #198).
///
/// Lets operators opt out of the default `host:<hostname>:pid-<pid>-<uuid8>`
/// fallback when no explicit `agent_id` is supplied. `anonymize_default = true`
/// swaps the hostname-revealing default for `anonymous:pid-<pid>-<uuid8>`,
/// matching what the `AI_MEMORY_ANONYMIZE=1` env var does ephemerally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfig {
    /// When true, the "no flag, no env, no MCP clientInfo" fallback uses
    /// `anonymous:pid-<pid>-<uuid8>` instead of the hostname-revealing
    /// `host:<hostname>:pid-<pid>-<uuid8>`. Default false.
    #[serde(default)]
    pub anonymize_default: bool,
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
            .map_or_else(|| cli_db.to_path_buf(), PathBuf::from)
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

    /// Resolve recall-scoring configuration (time-decay half-life) from the
    /// config file, falling back to compiled defaults. v0.6.0.0.
    pub fn effective_scoring(&self) -> ResolvedScoring {
        ResolvedScoring::from_config(self.scoring.as_ref())
    }

    /// Whether to archive memories before GC deletion (default: true).
    pub fn effective_archive_on_gc(&self) -> bool {
        self.archive_on_gc.unwrap_or(true)
    }

    /// Whether post-store autonomy hooks (`auto_tag` + `detect_contradiction`)
    /// fire on every successful `memory_store`. v0.6.0.0.
    /// Precedence: `AI_MEMORY_AUTONOMOUS_HOOKS=1` env var (truthy) >
    /// config file > default false. `AI_MEMORY_AUTONOMOUS_HOOKS=0` also
    /// honored for explicit-off.
    pub fn effective_autonomous_hooks(&self) -> bool {
        if let Ok(v) = std::env::var("AI_MEMORY_AUTONOMOUS_HOOKS") {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "1" | "true" | "yes" | "on") {
                return true;
            }
            if matches!(v.as_str(), "0" | "false" | "no" | "off" | "") {
                return false;
            }
        }
        self.autonomous_hooks.unwrap_or(false)
    }

    /// Whether to anonymize the default `agent_id` fallback (Task 1.2 #198).
    /// Precedence: `AI_MEMORY_ANONYMIZE=1` env var (truthy) > config file > default false.
    pub fn effective_anonymize_default(&self) -> bool {
        if let Ok(v) = std::env::var("AI_MEMORY_ANONYMIZE") {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "1" | "true" | "yes" | "on") {
                return true;
            }
            if matches!(v.as_str(), "0" | "false" | "no" | "off" | "") {
                return false;
            }
        }
        self.identity.as_ref().is_some_and(|i| i.anonymize_default)
    }

    /// Resolve URL for embedding model (falls back to `ollama_url`).
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
        let caps = cfg.capabilities();
        assert!(caps.features.cross_encoder_reranking);
        // P1 honesty patch: memory_reflection is a planned-feature
        // object now. Even on the autonomous tier the underlying
        // subsystem is roadmap (v0.7+), so `planned == true` and
        // `enabled == false` regardless of tier.
        assert!(caps.features.memory_reflection.planned);
        assert!(!caps.features.memory_reflection.enabled);
        assert_eq!(caps.features.memory_reflection.version, "v0.7+");
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

    /// v0.6.3.1 (capabilities schema v2, P1 honesty patch).
    /// Round-trip the new struct through serde_json and assert the v2
    /// honesty contract: dropped fields absent, planned-feature blocks
    /// shaped correctly, runtime-state defaults conservative.
    #[test]
    fn capabilities_v2_zero_state_round_trip() {
        let caps = FeatureTier::Keyword.config().capabilities();
        let val: serde_json::Value = serde_json::to_value(&caps).unwrap();

        assert_eq!(val["schema_version"], "2");

        // permissions zero-state: mode="advisory" (was "ask" in v1),
        // active_rules=0. `rule_summary` dropped from v2.
        assert_eq!(val["permissions"]["mode"], "advisory");
        assert_eq!(val["permissions"]["active_rules"], 0);
        assert!(
            val["permissions"].get("rule_summary").is_none(),
            "v2 honesty patch drops `permissions.rule_summary` (no per-rule serializer)"
        );

        // hooks zero-state: 0 registered. `by_event` dropped from v2.
        assert_eq!(val["hooks"]["registered_count"], 0);
        assert!(
            val["hooks"].get("by_event").is_none(),
            "v2 honesty patch drops `hooks.by_event` (no event registry)"
        );

        // compaction zero-state: planned, not enabled, optional fields omitted
        assert_eq!(val["compaction"]["planned"], true);
        assert_eq!(val["compaction"]["enabled"], false);
        assert_eq!(val["compaction"]["version"], "v0.8+");
        assert!(
            val["compaction"].get("interval_minutes").is_none(),
            "Option::None values must be skipped in serialization"
        );
        assert!(val["compaction"].get("last_run_at").is_none());
        assert!(val["compaction"].get("last_run_stats").is_none());

        // approval zero-state: 0 pending. `subscribers` and
        // `default_timeout_seconds` dropped from v2.
        assert_eq!(val["approval"]["pending_requests"], 0);
        assert!(
            val["approval"].get("subscribers").is_none(),
            "v2 honesty patch drops `approval.subscribers` (no subscription API)"
        );
        assert!(
            val["approval"].get("default_timeout_seconds").is_none(),
            "v2 honesty patch drops `approval.default_timeout_seconds` (no sweeper)"
        );

        // transcripts zero-state: planned, not enabled, zero counts skipped
        assert_eq!(val["transcripts"]["planned"], true);
        assert_eq!(val["transcripts"]["enabled"], false);
        assert_eq!(val["transcripts"]["version"], "v0.7+");

        // memory_reflection: planned-feature object (was bool)
        assert_eq!(val["features"]["memory_reflection"]["planned"], true);
        assert_eq!(val["features"]["memory_reflection"]["enabled"], false);
        assert_eq!(val["features"]["memory_reflection"]["version"], "v0.7+");

        // Runtime-state defaults are conservative — they get overlaid
        // at the handler boundary based on the live embedder + reranker
        // handles. With no overlays, the keyword-tier daemon reports
        // `disabled` / `off`.
        assert_eq!(val["features"]["recall_mode_active"], "disabled");
        assert_eq!(val["features"]["reranker_active"], "off");

        // Round-trip back to a typed Capabilities and confirm field
        // identity (proves Deserialize works for all reshaped structs).
        let restored: Capabilities = serde_json::from_value(val).unwrap();
        assert_eq!(restored.schema_version, "2");
        assert_eq!(restored.permissions.mode, "advisory");
        assert!(restored.compaction.status.planned);
        assert!(restored.transcripts.status.planned);
        assert_eq!(restored.features.recall_mode_active, RecallMode::Disabled);
        assert_eq!(restored.features.reranker_active, RerankerMode::Off);
    }

    /// P1 honesty patch: legacy v1 projection preserves the old shape
    /// for clients that opt in via `Accept-Capabilities: v1`.
    #[test]
    fn capabilities_v1_projection_preserves_legacy_shape() {
        let caps = FeatureTier::Autonomous.config().capabilities();
        let v1 = caps.to_v1();
        let val: serde_json::Value = serde_json::to_value(&v1).unwrap();

        // v1: no schema_version, no v2-only blocks
        assert!(
            val.get("schema_version").is_none(),
            "v1 has no schema_version"
        );
        assert!(
            val.get("permissions").is_none(),
            "v1 has no permissions block"
        );
        assert!(val.get("hooks").is_none());
        assert!(val.get("compaction").is_none());
        assert!(val.get("approval").is_none());
        assert!(val.get("transcripts").is_none());

        // v1 keeps the four legacy top-level keys
        assert!(val["tier"].is_string());
        assert!(val["version"].is_string());
        assert!(val["features"].is_object());
        assert!(val["models"].is_object());

        // v1 features.memory_reflection collapses to a bool — autonomous
        // tier had cross_encoder + has_llm but the planned object's
        // `enabled = false`, so the v1 bool is `false`.
        assert!(val["features"]["memory_reflection"].is_boolean());
        assert_eq!(val["features"]["memory_reflection"], false);

        // v1 features carry no recall_mode_active / reranker_active
        assert!(val["features"].get("recall_mode_active").is_none());
        assert!(val["features"].get("reranker_active").is_none());
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

    // --- v0.6.0.0 recall scoring (time-decay half-life) ---

    #[test]
    fn scoring_defaults_match_spec() {
        let s = ResolvedScoring::default();
        assert!((s.half_life_days_short - 7.0).abs() < f64::EPSILON);
        assert!((s.half_life_days_mid - 30.0).abs() < f64::EPSILON);
        assert!((s.half_life_days_long - 365.0).abs() < f64::EPSILON);
        assert!(!s.legacy_scoring);
    }

    #[test]
    fn scoring_from_config_overrides() {
        let cfg = RecallScoringConfig {
            half_life_days_short: Some(3.5),
            half_life_days_mid: Some(14.0),
            half_life_days_long: Some(730.0),
            legacy_scoring: false,
        };
        let s = ResolvedScoring::from_config(Some(&cfg));
        assert!((s.half_life_days_short - 3.5).abs() < f64::EPSILON);
        assert!((s.half_life_days_mid - 14.0).abs() < f64::EPSILON);
        assert!((s.half_life_days_long - 730.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scoring_clamps_out_of_range() {
        let cfg = RecallScoringConfig {
            half_life_days_short: Some(-10.0),
            half_life_days_mid: Some(0.0),
            half_life_days_long: Some(1_000_000.0),
            legacy_scoring: false,
        };
        let s = ResolvedScoring::from_config(Some(&cfg));
        assert!(s.half_life_days_short >= ResolvedScoring::MIN_HALF_LIFE);
        assert!(s.half_life_days_mid >= ResolvedScoring::MIN_HALF_LIFE);
        assert!(s.half_life_days_long <= ResolvedScoring::MAX_HALF_LIFE);
    }

    #[test]
    fn scoring_decay_at_half_life_is_half() {
        let s = ResolvedScoring::default();
        // Short tier half-life is 7 days → at age=7d, decay=0.5
        let d = s.decay_multiplier(&Tier::Short, 7.0);
        assert!((d - 0.5).abs() < 1e-9);
        let d = s.decay_multiplier(&Tier::Mid, 30.0);
        assert!((d - 0.5).abs() < 1e-9);
        let d = s.decay_multiplier(&Tier::Long, 365.0);
        assert!((d - 0.5).abs() < 1e-9);
    }

    #[test]
    fn scoring_decay_monotonic() {
        let s = ResolvedScoring::default();
        let d_new = s.decay_multiplier(&Tier::Mid, 1.0);
        let d_old = s.decay_multiplier(&Tier::Mid, 60.0);
        // Older memories decay more (lower multiplier).
        assert!(d_new > d_old);
        assert!(d_new < 1.0);
        assert!(d_old > 0.0);
    }

    #[test]
    fn scoring_decay_zero_age_is_one() {
        let s = ResolvedScoring::default();
        assert!((s.decay_multiplier(&Tier::Short, 0.0) - 1.0).abs() < f64::EPSILON);
        // Negative ages (clock skew, future timestamps) are also treated as fresh.
        assert!((s.decay_multiplier(&Tier::Short, -5.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scoring_legacy_disables_decay() {
        let cfg = RecallScoringConfig {
            legacy_scoring: true,
            ..Default::default()
        };
        let s = ResolvedScoring::from_config(Some(&cfg));
        // No decay regardless of age.
        assert!((s.decay_multiplier(&Tier::Short, 100.0) - 1.0).abs() < f64::EPSILON);
        assert!((s.decay_multiplier(&Tier::Mid, 1000.0) - 1.0).abs() < f64::EPSILON);
        assert!((s.decay_multiplier(&Tier::Long, 10_000.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn effective_scoring_on_empty_config() {
        let cfg = AppConfig::default();
        let s = cfg.effective_scoring();
        assert_eq!(s.half_life_days_short, 7.0);
        assert!(!s.legacy_scoring);
    }

    #[test]
    fn scoring_roundtrip_through_toml() {
        let toml_src = r"
[scoring]
half_life_days_short = 5.0
half_life_days_mid = 25.0
legacy_scoring = false
";
        let cfg: AppConfig = toml::from_str(toml_src).expect("parses");
        let s = cfg.effective_scoring();
        assert!((s.half_life_days_short - 5.0).abs() < f64::EPSILON);
        assert!((s.half_life_days_mid - 25.0).abs() < f64::EPSILON);
        // Unset long defaults.
        assert!((s.half_life_days_long - 365.0).abs() < f64::EPSILON);
    }

    // ---- Wave 3 (Closer T) tests for uncovered effective_* helpers
    // and write_default_if_missing. ----

    #[test]
    fn effective_tier_cli_overrides_config() {
        let cfg = AppConfig {
            tier: Some("smart".to_string()),
            ..AppConfig::default()
        };
        // CLI flag wins over config.
        assert_eq!(
            cfg.effective_tier(Some("autonomous")),
            FeatureTier::Autonomous
        );
        // No CLI flag → config used.
        assert_eq!(cfg.effective_tier(None), FeatureTier::Smart);
    }

    #[test]
    fn effective_tier_unknown_falls_back_to_semantic() {
        let cfg = AppConfig::default();
        assert_eq!(
            cfg.effective_tier(Some("invalid-tier")),
            FeatureTier::Semantic
        );
        // No CLI, no config → default semantic.
        assert_eq!(cfg.effective_tier(None), FeatureTier::Semantic);
    }

    #[test]
    fn effective_db_cli_path_wins_when_non_default() {
        let cfg = AppConfig {
            db: Some("/from/config.db".to_string()),
            ..AppConfig::default()
        };
        let cli_path = Path::new("/from/cli.db");
        assert_eq!(cfg.effective_db(cli_path), PathBuf::from("/from/cli.db"));
    }

    #[test]
    fn effective_db_falls_back_to_config_when_cli_default() {
        let cfg = AppConfig {
            db: Some("/from/config.db".to_string()),
            ..AppConfig::default()
        };
        // The CLI default is "ai-memory.db" — config wins for that case.
        assert_eq!(
            cfg.effective_db(Path::new("ai-memory.db")),
            PathBuf::from("/from/config.db")
        );
    }

    #[test]
    fn effective_db_falls_back_to_cli_when_no_config() {
        let cfg = AppConfig::default();
        let cli_path = Path::new("ai-memory.db");
        assert_eq!(cfg.effective_db(cli_path), PathBuf::from("ai-memory.db"));
    }

    #[test]
    fn effective_ollama_url_default_when_unset() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.effective_ollama_url(), "http://localhost:11434");
    }

    #[test]
    fn effective_ollama_url_uses_configured_value() {
        let cfg = AppConfig {
            ollama_url: Some("http://my-host:9999".to_string()),
            ..AppConfig::default()
        };
        assert_eq!(cfg.effective_ollama_url(), "http://my-host:9999");
    }

    #[test]
    fn effective_embed_url_falls_back_to_ollama_url() {
        let cfg = AppConfig {
            ollama_url: Some("http://ollama:11434".to_string()),
            ..AppConfig::default()
        };
        // No embed_url → fall back to ollama_url.
        assert_eq!(cfg.effective_embed_url(), "http://ollama:11434");
    }

    #[test]
    fn effective_embed_url_uses_dedicated_value_when_set() {
        let cfg = AppConfig {
            ollama_url: Some("http://ollama:11434".to_string()),
            embed_url: Some("http://embed:8080".to_string()),
            ..AppConfig::default()
        };
        // Dedicated embed_url wins.
        assert_eq!(cfg.effective_embed_url(), "http://embed:8080");
    }

    #[test]
    fn effective_embed_url_uses_default_when_neither_set() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.effective_embed_url(), "http://localhost:11434");
    }

    #[test]
    fn effective_archive_on_gc_default_is_true() {
        let cfg = AppConfig::default();
        assert!(cfg.effective_archive_on_gc());
    }

    #[test]
    fn effective_archive_on_gc_respects_explicit_false() {
        let cfg = AppConfig {
            archive_on_gc: Some(false),
            ..AppConfig::default()
        };
        assert!(!cfg.effective_archive_on_gc());
    }

    #[test]
    fn effective_autonomous_hooks_default_is_false() {
        // SAFETY: clear env so this test is deterministic; tests run with
        // --test-threads=1 in CI for env-based tests, but we stay
        // defensive and set+unset locally.
        // SAFETY: env mutation is acceptable here because we set then unset.
        unsafe { std::env::remove_var("AI_MEMORY_AUTONOMOUS_HOOKS") };
        let cfg = AppConfig::default();
        assert!(!cfg.effective_autonomous_hooks());
    }

    #[test]
    fn effective_autonomous_hooks_config_value_used_when_env_unset() {
        unsafe { std::env::remove_var("AI_MEMORY_AUTONOMOUS_HOOKS") };
        let cfg = AppConfig {
            autonomous_hooks: Some(true),
            ..AppConfig::default()
        };
        assert!(cfg.effective_autonomous_hooks());
    }

    #[test]
    fn effective_anonymize_default_falls_back_to_config() {
        unsafe { std::env::remove_var("AI_MEMORY_ANONYMIZE") };
        let cfg = AppConfig::default();
        assert!(!cfg.effective_anonymize_default());
    }

    #[test]
    fn write_default_if_missing_creates_file_then_noops() {
        // Use a temp dir as $HOME so we don't clobber a real config.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: env mutation is contained; we restore at end.
        unsafe { std::env::set_var("HOME", tmp.path()) };
        // First call writes the file.
        AppConfig::write_default_if_missing();
        let expected = AppConfig::config_path().unwrap();
        assert!(expected.exists(), "config not written at {expected:?}");
        let original = std::fs::read_to_string(&expected).unwrap();
        assert!(original.contains("ai-memory configuration"));
        // Second call must NOT overwrite (idempotent).
        std::fs::write(&expected, "# user-edited\n").unwrap();
        AppConfig::write_default_if_missing();
        let after = std::fs::read_to_string(&expected).unwrap();
        assert_eq!(after, "# user-edited\n");
    }

    #[test]
    fn config_path_returns_some_when_home_set() {
        // SAFETY: env mutation contained to this test.
        unsafe { std::env::set_var("HOME", "/some/home") };
        let path = AppConfig::config_path().unwrap();
        assert!(path.starts_with("/some/home"));
    }

    #[test]
    fn load_from_returns_default_for_missing_file() {
        // Non-existent path → default config.
        let cfg = AppConfig::load_from(Path::new("/non/existent/path.toml"));
        assert!(cfg.tier.is_none());
        assert!(cfg.db.is_none());
    }

    #[test]
    fn load_from_returns_default_for_unparseable_toml() {
        // Garbage TOML → load_from prints a warning and returns default.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "this is not [valid toml]]]").unwrap();
        let cfg = AppConfig::load_from(tmp.path());
        assert!(cfg.tier.is_none());
    }

    #[test]
    fn load_from_parses_valid_toml() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"
                tier = "smart"
                db = "/disk.db"
            "#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(tmp.path());
        assert_eq!(cfg.tier.as_deref(), Some("smart"));
        assert_eq!(cfg.db.as_deref(), Some("/disk.db"));
    }
}
