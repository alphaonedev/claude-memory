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
                // v0.7.0 K3: surface the *active* mode (the one the
                // gate will actually consult), not a hard-coded string.
                // Falls through to the K3 default (`advisory`) when
                // `[permissions].mode` is unset in `config.toml`.
                mode: active_permissions_mode().as_str().to_string(),
                active_rules: 0,
                // v0.6.3.1 (P4, G1): chain-walking enforcement landed
                // in this release. Surface "enforced" so consumers can
                // distinguish a governed deployment from the historical
                // "display_only" posture.
                inheritance: Some("enforced".to_string()),
                // v0.7.0 K3: per-mode decision counts. Snapshot at
                // capability-build time so operators can correlate
                // doctor reports with capability responses.
                decision_counts: Some(permissions_decision_counts()),
            },
            hooks: CapabilityHooks::default(),
            compaction: CapabilityCompaction::planned(),
            approval: CapabilityApproval {
                pending_requests: 0,
            },
            transcripts: CapabilityTranscripts::planned(),
            hnsw: CapabilityHnsw::default(),
            // v0.7 J1 — populated by the SAL wrapper at runtime when a
            // Postgres adapter is active. None at config-construction
            // time (no SAL handle here); the MCP/HTTP wrapper overlays
            // the live tag from `PostgresStore::kg_backend()` once
            // J2 wires the SAL into AppState.
            kg_backend: None,
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

    /// v0.6.3.1 (P3, G2): HNSW vector-index health. Defaults to a
    /// quiet zero-state report; the MCP/HTTP capabilities wrapper
    /// overwrites with live process counters when the index module
    /// has run an eviction.
    #[serde(default)]
    pub hnsw: CapabilityHnsw,

    /// v0.7 J1 — knowledge-graph backend tag. `"age"` when a Postgres
    /// SAL adapter probed Apache AGE successfully at connect time;
    /// `"cte"` when the deployment falls back to the recursive-CTE
    /// path (every SQLite deployment + Postgres without AGE
    /// installed). `None` when no SAL adapter is wired (the active
    /// dispatch path through the legacy `crate::db` free functions
    /// pre-J2). Operators consult this through `ai-memory doctor` and
    /// `memory_capabilities` to verify which traversal path their
    /// daemon actually runs. Skipped from the JSON wire when `None`
    /// so v1 / v2 clients that don't know the field round-trip cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kg_backend: Option<String>,
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
    /// v0.6.3.1 (P4, audit G1): governance-inheritance posture.
    /// `"enforced"` = `resolve_governance_policy` walks the namespace
    /// chain leaf-first and returns the most-specific policy (with
    /// `inherit: false` short-circuiting). Pre-v0.6.3.1 was
    /// `"display_only"` — the UI surfaced the chain but the gate
    /// consulted only the leaf, leaving children of governed parents
    /// completely ungoverned. The field is `Option<String>` so older
    /// capabilities responses (without the field) round-trip cleanly
    /// via `#[serde(default)]`.
    #[serde(default)]
    pub inheritance: Option<String>,
    /// v0.7.0 K3: per-mode decision counts since process start. Lets
    /// operators verify the gate is actually being consulted and spot
    /// drift between advertised policy and enforced policy. `None` on
    /// older responses (`#[serde(default)]` round-trips cleanly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_counts: Option<PermissionsDecisionCounts>,
}

/// Hook-pipeline block (capabilities schema v2). Pre-v0.7 reports webhook
/// subscriptions as the closest analogue. The full hook pipeline lands in
/// v0.7 Bucket 0 (arch-enhancement-spec §2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityHooks {
    /// Number of registered hook subscribers (proxy: webhook subscriptions).
    pub registered_count: usize,
    // P1 honesty patch: `by_event` was always an empty map — no event
    // registry exists. Dropped from the v2 wire schema.
    /// v0.6.3.1 P5 (G9): canonical list of webhook event types the
    /// daemon emits. Integrators pin the `subscribe(event_types: …)`
    /// filter against these strings. Always populated so downstream
    /// callers do not have to handle a missing field.
    #[serde(default = "default_webhook_events")]
    pub webhook_events: Vec<String>,
}

impl Default for CapabilityHooks {
    fn default() -> Self {
        Self {
            registered_count: 0,
            webhook_events: default_webhook_events(),
        }
    }
}

/// Default webhook events list — kept in sync with
/// `crate::subscriptions::WEBHOOK_EVENT_TYPES`. The constant lives in
/// `subscriptions.rs` (the surface that uses it at runtime); this
/// helper exists so `serde(default = …)` and `CapabilityHooks::default`
/// can fill the field without a cross-module dep on `subscriptions`.
fn default_webhook_events() -> Vec<String> {
    vec![
        "memory_store".to_string(),
        "memory_promote".to_string(),
        "memory_delete".to_string(),
        "memory_link_created".to_string(),
        "memory_consolidated".to_string(),
    ]
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

/// HNSW vector-index health (capabilities schema v2, v0.6.3.1 P3).
///
/// Closes the G2 audit gap by surfacing both the cumulative oldest-eviction
/// count and a rolling-window flag so operators can distinguish "this
/// process has hit the cap once, long ago" from "we are currently
/// sustained at the cap and shedding embeddings now". Both numbers are
/// process-local — the index itself resets on restart so persistence
/// would be misleading.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityHnsw {
    /// Cumulative count of vectors evicted by the `MAX_ENTRIES`-cap path
    /// since this process started.
    pub evictions_total: u64,
    /// True when at least one eviction has occurred in the last 60 s.
    /// Lets dashboards alert on *active* pressure rather than only the
    /// historical counter.
    pub evicted_recently: bool,
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

    /// v0.7.0 (A1+A2+A3+A4): project the report into the v3 shape.
    ///
    /// v3 = v2 +
    ///   - top-level `summary` (A1) — terse description of operational
    ///     access plus the three named recovery paths.
    ///   - top-level `to_describe_to_user` (A2) — plain-English
    ///     end-user-facing sentence the LLM should repeat verbatim
    ///     when asked "what tools do you have?". No MCP jargon.
    ///   - top-level `tools` (A3) — per-tool array carrying name,
    ///     family, `loaded`, and `callable_now`. `callable_now`
    ///     combines profile-side loaded-state with the
    ///     `[mcp.allowlist]` agent-can-call decision so an LLM that
    ///     keeps a manifest cache doesn't need to ask twice to know
    ///     whether a tool will resolve.
    ///   - top-level `agent_permitted_families` (A4, optional) — when
    ///     the `[mcp.allowlist]` is enabled AND an `agent_id` is
    ///     provided, lists the family names the requesting agent is
    ///     allowed to access (collapses every callable_now=true entry's
    ///     family to a unique list). When the allowlist is disabled or
    ///     no agent_id is provided, the field is omitted from the wire
    ///     (so v2-shaped consumers see no churn from A4 alone).
    ///
    /// All four are computed by the caller from the live `Profile` +
    /// `McpConfig` + `agent_id` state because the [`Capabilities`]
    /// struct itself doesn't know which families the MCP server
    /// actually advertised or which agent is asking.
    ///
    /// A5 bumps the default wire shape to v3. v2 stays supported
    /// indefinitely.
    #[must_use]
    pub fn to_v3(
        &self,
        summary: String,
        to_describe_to_user: String,
        tools: Vec<ToolEntry>,
        agent_permitted_families: Option<Vec<String>>,
    ) -> CapabilitiesV3 {
        CapabilitiesV3 {
            schema_version: "3".to_string(),
            summary,
            to_describe_to_user,
            tools,
            agent_permitted_families,
            tier: self.tier.clone(),
            version: self.version.clone(),
            features: self.features.clone(),
            models: self.models.clone(),
            permissions: self.permissions.clone(),
            hooks: self.hooks.clone(),
            compaction: self.compaction.clone(),
            approval: self.approval.clone(),
            transcripts: self.transcripts.clone(),
            hnsw: self.hnsw.clone(),
            // v0.7 J1 — propagate the resolved KG backend tag verbatim.
            // None when no SAL adapter is wired (every pre-J2 build);
            // `Some("age" | "cte")` once the SAL handle is threaded.
            kg_backend: self.kg_backend.clone(),
        }
    }
}

/// v0.7.0 A3 — per-tool entry in the capabilities-v3 `tools` array.
///
/// `loaded` mirrors `Profile::loads(name)` — true when the active
/// profile would advertise this tool in `tools/list`.
///
/// `callable_now` is the AND of `loaded` with the
/// `[mcp.allowlist]` per-agent gate. When the allowlist is disabled
/// (no `[mcp.allowlist]` table or empty table), `callable_now ==
/// loaded`. When the allowlist is active and the requesting agent
/// has no entry granting the tool's family, `callable_now == false`
/// even though `loaded == true`.
///
/// LLMs that cache the v3 manifest can use this to skip a doomed
/// JSON-RPC call rather than discover -32601 the hard way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolEntry {
    /// Fully-qualified MCP tool name (e.g., `memory_store`).
    pub name: String,
    /// Family the tool belongs to. Always one of the eight canonical
    /// family names (`core`, `lifecycle`, `graph`, etc.) or
    /// `"always_on"` for the `memory_capabilities` bootstrap which
    /// doesn't sit in any single family from a registration standpoint.
    pub family: String,
    /// Whether the active profile's family set includes this tool's
    /// family (i.e., it appears in `tools/list`).
    pub loaded: bool,
    /// `loaded && agent_can_call(agent_id, family)`. When the
    /// `[mcp.allowlist]` is disabled, `callable_now == loaded`.
    pub callable_now: bool,
}

// ---------------------------------------------------------------------------
// Capabilities v3 — v0.7.0 attested-cortex schema (additive over v2)
// ---------------------------------------------------------------------------

/// v0.7.0 capabilities schema (A1 increment). Additive over [`Capabilities`]
/// (v2): the top-level `summary` field carries a pre-computed,
/// plain-language description of the LLM's operational tool surface so
/// reasoning-class LLMs converge on accurate first-answer descriptions
/// without having to traverse `families[]` and count manually.
///
/// Wire selection: clients opt in via `accept="v3"` on the MCP
/// `memory_capabilities` call, or `Accept-Capabilities: v3` over HTTP
/// (HTTP wiring lands with A5). Default response remains v2 until A5
/// flips the default. v2 stays supported indefinitely.
///
/// Increment plan: A2 adds `to_describe_to_user`, A3 adds per-tool
/// `callable_now`, A4 adds `agent_permitted_families`. A5 bumps the
/// default wire shape and seals v3 as the recommended client target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesV3 {
    /// Schema-version discriminator. Always `"3"` in v0.7.0.
    pub schema_version: String,

    /// Pre-computed plain-language summary of operational access.
    /// Carries the loaded vs total tool counts under the active profile
    /// plus the three named recovery paths (`--profile`,
    /// `memory_load_family`, `memory_smart_load`). Computed at response
    /// time from the live profile state — never cached at build time
    /// because the count of advertised tools depends on the running
    /// server's `--profile` flag.
    pub summary: String,

    /// v0.7.0 A2 — pre-computed end-user-facing sentence the LLM should
    /// repeat verbatim when an end-user asks "what tools do you have?".
    /// Distinct in tone from [`Self::summary`]: no MCP jargon, no
    /// `--profile` references, no JSON-RPC vocabulary. Reads as a
    /// normal sentence a human would say. See
    /// `docs/v0.7/canonical-phrasings.md` for the canonical
    /// substitution template + worked examples per profile.
    pub to_describe_to_user: String,

    /// v0.7.0 A3 — per-tool array carrying name, family, `loaded`, and
    /// `callable_now`. `callable_now` combines profile-side
    /// loaded-state with the `[mcp.allowlist]` agent-can-call decision
    /// so an LLM that caches this manifest can skip a doomed JSON-RPC
    /// call rather than discovering -32601 the hard way. Order matches
    /// `tool_definitions()`'s registration walk so a sequential reader
    /// gets a stable presentation.
    pub tools: Vec<ToolEntry>,

    /// v0.7.0 A4 — list of family names this agent is permitted to
    /// access via the `[mcp.allowlist]` gate. Present (with possibly
    /// an empty array) only when the allowlist is configured AND an
    /// `agent_id` was provided. Absent when the allowlist is disabled
    /// or no agent_id was provided — that absence is meaningful, not a
    /// drift, hence `Option<Vec<String>>` + `skip_serializing_if`.
    ///
    /// LLMs that keep a per-agent manifest cache can use this to
    /// short-circuit family-level decisions without iterating
    /// `tools[]` and counting unique families.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_permitted_families: Option<Vec<String>>,

    pub tier: String,
    pub version: String,
    pub features: CapabilityFeatures,
    pub models: CapabilityModels,
    pub permissions: CapabilityPermissions,
    pub hooks: CapabilityHooks,
    pub compaction: CapabilityCompaction,
    pub approval: CapabilityApproval,
    pub transcripts: CapabilityTranscripts,

    #[serde(default)]
    pub hnsw: CapabilityHnsw,

    /// v0.7 J1 — knowledge-graph backend tag forwarded from the v2
    /// projection. `Some("age" | "cte")` once the SAL handle is
    /// threaded through `AppState`; `None` while no SAL adapter is
    /// wired. Skipped from the JSON wire when `None` so older clients
    /// that don't know the field round-trip cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kg_backend: Option<String>,
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
    /// v0.6.3.1 (PR-5 / issue #487) — operational logging facility.
    /// Default-OFF for privacy; opt-in turns on the rolling file
    /// appender that captures every `tracing::*` call site to disk.
    pub logging: Option<LoggingConfig>,
    /// v0.6.3.1 (PR-5 / issue #487) — security audit trail. Default-OFF
    /// for privacy; opt-in emits a hash-chained, tamper-evident JSON
    /// log of every memory mutation suitable for SIEM ingestion and
    /// SOC2 / HIPAA / GDPR / FedRAMP compliance evidence.
    pub audit: Option<AuditConfig>,
    /// v0.6.3.1 (PR-9h / issue #487 PR #497 req #73) — boot privacy
    /// kill-switch. Default-ON (existing users see no behavior change);
    /// `[boot] enabled = false` silences boot entirely (empty stdout +
    /// empty stderr, exit 0) for privacy-sensitive hosts where memory
    /// titles must not enter CI logs. `[boot] redact_titles = true`
    /// keeps the manifest header but replaces row titles with
    /// `<redacted>` for compliance contexts that need the audit-trail
    /// signal of "boot ran with N memories" without exposing subjects.
    pub boot: Option<BootConfig>,
    /// v0.6.4 — MCP server tunables. Today this only carries `profile`
    /// (the named tool surface). Future v0.6.4 phases add the
    /// `[mcp.allowlist]` per-agent capability table (Track D —
    /// v0.6.4-008).
    pub mcp: Option<McpConfig>,
    /// v0.7.0 K3 — `[permissions]` block. Drives the gate's enforcement
    /// posture (`enforce` / `advisory` / `off`). When unset, the
    /// compiled default in [`PermissionsConfig::default`] applies
    /// (`advisory` — preserves the v0.6.x honest-disclosure posture
    /// where governance metadata was recorded but not blocked at the
    /// gate). New installs that want the strict gate set
    /// `[permissions] mode = "enforce"` explicitly.
    pub permissions: Option<PermissionsConfig>,
}

// ---------------------------------------------------------------------------
// Permissions / governance gate (K3)
// ---------------------------------------------------------------------------

/// Enforcement posture consulted by [`crate::db::enforce_governance`].
///
/// v0.7.0 K3 — closes the v0.6.3.1 honest-Capabilities-v2 disclosure
/// that `permissions.mode = "advisory"` was advertised but the gate
/// itself returned `Deny` / `Pending` regardless. The gate now actually
/// honors this knob.
///
/// Wire format on `config.toml`:
///
/// ```toml
/// [permissions]
/// mode = "advisory"   # or "enforce" / "off"
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionsMode {
    /// Block on policy violation. `Deny`/`Pending` decisions returned
    /// to the caller as-is. The strict, audit-ready posture.
    Enforce,
    /// Log a warning and allow the action. Governance metadata is
    /// recorded but does not block writes. Default for v0.7.0 to
    /// preserve the v0.6.x posture for upgrading operators.
    Advisory,
    /// Skip the gate entirely. No policy resolution, no log, no
    /// `pending_actions` row. Useful for benchmarking and temporary
    /// freeze-thaw incident response.
    Off,
}

impl Default for PermissionsMode {
    fn default() -> Self {
        Self::Advisory
    }
}

impl PermissionsMode {
    /// Lowercase wire string for capabilities + doctor surfaces.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Advisory => "advisory",
            Self::Off => "off",
        }
    }
}

/// `[permissions]` block in `config.toml`. Carries the gate's
/// enforcement posture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsConfig {
    /// Enforcement mode. Defaults to [`PermissionsMode::Advisory`] when
    /// omitted from the config file.
    #[serde(default)]
    pub mode: PermissionsMode,
}

// ---------------------------------------------------------------------------
// Process-wide permissions-mode handle (K3)
// ---------------------------------------------------------------------------
//
// The gate (`db::enforce_governance`) needs to consult the active mode
// at decision time but lives in the `db` module, which has no handle on
// `AppConfig`. We use a `OnceLock` set by `main` (and the daemon
// runtime) so the gate can read the mode without an API churn through
// every callsite. When the lock is unset — the case for unit and
// integration tests that drive `db::enforce_governance` directly
// without booting the daemon — the gate defaults to
// [`PermissionsMode::Enforce`] so the strict semantics that the K1
// ship-gate suite codifies remain the load-bearing default for
// programmatic callers.

use std::sync::OnceLock;

static ACTIVE_PERMISSIONS_MODE: OnceLock<PermissionsMode> = OnceLock::new();

/// Set the process-wide active [`PermissionsMode`]. Idempotent — the
/// first caller wins; subsequent calls are no-ops. Called from
/// `main` (CLI) and the daemon bootstrap path with the value resolved
/// from `[permissions].mode` in `config.toml`.
pub fn set_active_permissions_mode(mode: PermissionsMode) {
    let _ = ACTIVE_PERMISSIONS_MODE.set(mode);
}

/// Read the process-wide active [`PermissionsMode`]. Falls back to
/// [`PermissionsMode::default`] (`advisory`) when unset, matching the
/// v0.7.0 K3 default for upgrading operators (governance recorded but
/// not blocked at the gate).
///
/// Test note: the K1 ship-gate matrix asserts `Pending`/`Deny`
/// outcomes from `db::enforce_governance` and therefore opts into
/// `Enforce` via [`set_active_permissions_mode`] at the start of each
/// scenario.
#[must_use]
pub fn active_permissions_mode() -> PermissionsMode {
    let override_tag = OVERRIDE_PERMISSIONS_MODE.load(std::sync::atomic::Ordering::SeqCst);
    match override_tag {
        1 => return PermissionsMode::Enforce,
        2 => return PermissionsMode::Advisory,
        3 => return PermissionsMode::Off,
        _ => {}
    }
    ACTIVE_PERMISSIONS_MODE
        .get()
        .copied()
        .unwrap_or(PermissionsMode::Advisory)
}

/// Test-only override of the active mode. Production code MUST use
/// [`set_active_permissions_mode`]; this helper exists so the K3 test
/// matrix can flip mode mid-test without spinning up a fresh process.
#[doc(hidden)]
pub fn override_active_permissions_mode_for_test(mode: PermissionsMode) {
    // SAFETY: OnceLock::set returns Err when already set; we want
    // last-writer-wins for tests only. Use a static Mutex to serialize
    // and an inner OnceCell-like reset via take + set is not possible
    // (OnceLock has no take). Instead, store an atomic indirection.
    OVERRIDE_PERMISSIONS_MODE.store(
        match mode {
            PermissionsMode::Enforce => 1,
            PermissionsMode::Advisory => 2,
            PermissionsMode::Off => 3,
        },
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// Test-only override slot. `0` = no override, otherwise encodes the
/// mode tag. Read by [`active_permissions_mode`] when set.
static OVERRIDE_PERMISSIONS_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Test-only: clear any override so subsequent tests see the
/// `OnceLock` value (or the default).
#[doc(hidden)]
pub fn clear_permissions_mode_override_for_test() {
    OVERRIDE_PERMISSIONS_MODE.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Test-only: acquire the global gate-mode serialization lock.
///
/// The active [`PermissionsMode`] lives in a process-wide atomic so
/// the gate at `db::enforce_governance` can read it without an API
/// churn through every callsite. Multiple lib tests flip the mode
/// (the K3 mode-matrix file, the CLI / HTTP gate scenarios, the
/// capabilities zero-state round-trip) and `cargo test --lib` runs
/// them in parallel by default. Each scenario MUST hold this guard
/// for its duration so two scenarios cannot race the atomic. The
/// returned guard poisons-OK so one panicking scenario does not
/// chain-fail the rest.
#[doc(hidden)]
#[must_use]
pub fn lock_permissions_mode_for_test() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;
    static GATE_LOCK: Mutex<()> = Mutex::new(());
    GATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Decision counters per mode (K3 — surfaced by doctor + capabilities)
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

static DECISIONS_ENFORCE: AtomicU64 = AtomicU64::new(0);
static DECISIONS_ADVISORY: AtomicU64 = AtomicU64::new(0);
static DECISIONS_OFF: AtomicU64 = AtomicU64::new(0);

/// Snapshot of decision counts per mode since process start. Surfaced
/// by `ai-memory doctor` and the capabilities `permissions` block so
/// operators can verify the gate is wired and observe drift between
/// "policies advertised" and "policies enforced".
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionsDecisionCounts {
    pub enforce: u64,
    pub advisory: u64,
    pub off: u64,
}

/// Increment the decision counter for `mode`. Called by the gate on
/// every consult. `Relaxed` is fine: the counters are observability,
/// not load-bearing for correctness.
pub fn record_permissions_decision(mode: PermissionsMode) {
    let c = match mode {
        PermissionsMode::Enforce => &DECISIONS_ENFORCE,
        PermissionsMode::Advisory => &DECISIONS_ADVISORY,
        PermissionsMode::Off => &DECISIONS_OFF,
    };
    c.fetch_add(1, Ordering::Relaxed);
}

/// Snapshot the current per-mode decision counts.
#[must_use]
pub fn permissions_decision_counts() -> PermissionsDecisionCounts {
    PermissionsDecisionCounts {
        enforce: DECISIONS_ENFORCE.load(Ordering::Relaxed),
        advisory: DECISIONS_ADVISORY.load(Ordering::Relaxed),
        off: DECISIONS_OFF.load(Ordering::Relaxed),
    }
}

/// Test-only: zero the counters between scenarios so the K3 matrix
/// can assert exact deltas.
#[doc(hidden)]
pub fn reset_permissions_decision_counts_for_test() {
    DECISIONS_ENFORCE.store(0, Ordering::SeqCst);
    DECISIONS_ADVISORY.store(0, Ordering::SeqCst);
    DECISIONS_OFF.store(0, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Logging facility (PR-5)
// ---------------------------------------------------------------------------

/// `[logging]` block in `config.toml`. Every field is `Option`; missing
/// fields fall back to the documented defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Master toggle. Default `false`.
    pub enabled: Option<bool>,
    /// Directory for rotated logs. Default `~/.local/state/ai-memory/logs/`.
    pub path: Option<String>,
    /// Soft cap on a single rotated file (advisory — informs rotation
    /// configuration; the appender enforces this via the chosen
    /// `rotation` cadence). Default 100.
    pub max_size_mb: Option<u64>,
    /// Maximum number of rotated files retained on disk. Default 30.
    pub max_files: Option<usize>,
    /// Days of log history to keep before `ai-memory logs archive`
    /// would compress them. Default 90.
    pub retention_days: Option<u32>,
    /// Emit JSON lines instead of the human-readable fmt layer. Default `false`.
    pub structured: Option<bool>,
    /// Tracing level / `EnvFilter` directive. Default `"info"`.
    pub level: Option<String>,
    /// Rotation policy: `minutely | hourly | daily | never`. Default `"daily"`.
    pub rotation: Option<String>,
    /// Override the rotated-file prefix. Default `"ai-memory.log"`.
    pub filename_prefix: Option<String>,
}

// ---------------------------------------------------------------------------
// Audit facility (PR-5)
// ---------------------------------------------------------------------------

/// `[audit]` block in `config.toml`. Drives the hash-chained audit
/// trail emitted from every memory mutation call site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Master toggle. Default `false`.
    pub enabled: Option<bool>,
    /// Audit log path. Either a directory (in which case `audit.log`
    /// is appended) or an explicit file path. Default
    /// `~/.local/state/ai-memory/audit/`.
    pub path: Option<String>,
    /// Documented schema version on the wire. The binary always emits
    /// `audit::SCHEMA_VERSION`; this knob is reserved for forward
    /// compatibility and must equal the binary's emitted version
    /// today (validated at init).
    pub schema_version: Option<u32>,
    /// Whether to redact `memory.content` from emitted events. **The
    /// only supported value in v1 is `true`** — the audit schema does
    /// not expose a content field at all; this flag is reserved for a
    /// future per-namespace exception API.
    pub redact_content: Option<bool>,
    /// Whether to compute and verify the per-line hash chain. Default `true`.
    pub hash_chain: Option<bool>,
    /// Cadence in minutes for the periodic `CHECKPOINT.sig`
    /// attestation marker. The marker is a synthetic audit event that
    /// pins the chain head into the log so an attacker who truncates
    /// the file can't silently rewind history. Default 60. 0 disables.
    pub attestation_cadence_minutes: Option<u32>,
    /// Apply the platform-appropriate "append-only" file flag at
    /// startup. Best-effort defense in depth; the chain is the
    /// load-bearing tamper-evidence. Default `true`.
    pub append_only: Option<bool>,
    /// Retention horizon (days). `ai-memory logs purge` warns about
    /// deleting audit records younger than this, and `audit verify`
    /// surfaces gaps when retention is shorter than the chain extent.
    /// Default 90. Compliance presets override.
    pub retention_days: Option<u32>,
    /// Compliance presets — apply industry-standard retention /
    /// redaction policy on top of the base config. See
    /// `docs/security/audit-trail.md` §Compliance.
    pub compliance: Option<AuditComplianceConfig>,
}

impl AuditConfig {
    /// Resolve the effective retention horizon after applying any
    /// active compliance preset. Presets win when `applied = true`;
    /// when multiple presets are applied the most-conservative
    /// (longest) retention wins so the binary never picks a value
    /// that violates any active policy.
    #[must_use]
    pub fn effective_retention_days(&self) -> u32 {
        let mut chosen = self.retention_days.unwrap_or(90);
        if let Some(comp) = &self.compliance {
            for preset in comp.applied_presets() {
                if let Some(d) = preset.retention_days
                    && d > chosen
                {
                    chosen = d;
                }
            }
        }
        chosen
    }

    /// Resolve the effective attestation cadence — the most-frequent
    /// (smallest non-zero) cadence across the base config and applied
    /// presets so the strictest compliance rule wins.
    #[must_use]
    pub fn effective_attestation_cadence_minutes(&self) -> u32 {
        let base = self.attestation_cadence_minutes.unwrap_or(60);
        let mut chosen = base;
        if let Some(comp) = &self.compliance {
            for preset in comp.applied_presets() {
                if let Some(m) = preset.attestation_cadence_minutes
                    && m > 0
                    && (chosen == 0 || m < chosen)
                {
                    chosen = m;
                }
            }
        }
        chosen
    }
}

// ---------------------------------------------------------------------------
// Boot privacy controls (PR-9h, v0.6.3.1, issue #487 PR #497 req #73)
// ---------------------------------------------------------------------------

/// `[boot]` block in `config.toml`. Drives the privacy kill-switch +
/// title-redaction behaviour of `ai-memory boot`. Both fields default
/// to the historical (pre-v0.6.3.1) behaviour so existing users see no
/// change.
///
/// Precedence for `enabled`:
///   `AI_MEMORY_BOOT_ENABLED=0` env var (truthy "0/false/no/off") >
///   `[boot] enabled` config value > compiled default `true`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootConfig {
    /// Master toggle. Default `true`. When set to `false`, `ai-memory
    /// boot` exits 0 with **empty stdout AND empty stderr** — the
    /// privacy-sensitive escape hatch for hosts where memory titles
    /// must never enter CI logs. The hook injects nothing.
    pub enabled: Option<bool>,
    /// When `true`, the manifest header still appears but every
    /// memory row's `title` field is replaced with `<redacted>` —
    /// useful for compliance contexts that need an audit trail of
    /// "boot ran with N memories" without exposing memory subjects.
    /// Default `false`.
    pub redact_titles: Option<bool>,
}

impl BootConfig {
    /// Resolve the effective `enabled` value with env-var precedence.
    /// `AI_MEMORY_BOOT_ENABLED=0/false/no/off` forces disabled;
    /// `=1/true/yes/on` forces enabled. Anything else falls through to
    /// the config file value (or the compiled default `true`).
    #[must_use]
    pub fn effective_enabled(&self) -> bool {
        if let Ok(v) = std::env::var("AI_MEMORY_BOOT_ENABLED") {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "0" | "false" | "no" | "off") {
                return false;
            }
            if matches!(v.as_str(), "1" | "true" | "yes" | "on") {
                return true;
            }
        }
        self.enabled.unwrap_or(true)
    }

    /// Resolve the effective `redact_titles` value. Default `false`.
    #[must_use]
    pub fn effective_redact_titles(&self) -> bool {
        self.redact_titles.unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// MCP server tunables (v0.6.4)
// ---------------------------------------------------------------------------

/// `[mcp]` block in `config.toml` — v0.6.4 addition. Today this only
/// carries the named tool `profile`. v0.6.4 Track D will extend with
/// `[mcp.allowlist]` for per-agent capability gating.
///
/// Resolution for `profile`: CLI flag > `AI_MEMORY_PROFILE` env (both
/// merged by clap) > this config field > compiled default `"core"`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    /// Named tool profile. One of `core`, `graph`, `admin`, `power`,
    /// `full`, or a comma-separated custom list (e.g.,
    /// `core,graph,archive`). Default `core` (v0.6.4 default flip).
    pub profile: Option<String>,

    /// v0.6.4-008 — per-agent capability allowlist. Maps an agent_id
    /// pattern to the families that agent may request via
    /// `memory_capabilities --include-schema family=<f>`. Patterns
    /// resolve to a Vec<String> (the family names). The wildcard
    /// pattern `"*"` is the default for agents not otherwise listed.
    /// When the entire allowlist is absent (`mcp.allowlist = None`),
    /// the gate is disabled — every caller may expand any family
    /// (Tier-1 single-process semantics, profile flag rules).
    ///
    /// Example config.toml:
    /// ```toml
    /// [mcp.allowlist]
    /// "alice" = ["core", "graph"]
    /// "bob"   = ["full"]
    /// "*"     = ["core"]
    /// ```
    pub allowlist: Option<std::collections::HashMap<String, Vec<String>>>,
}

impl McpConfig {
    /// v0.6.4-008 — resolve the allowlist decision for an agent
    /// requesting a family.
    ///
    /// Returns:
    /// - `AllowlistDecision::Disabled` if the entire allowlist is
    ///   absent (Tier-1 default — gate is off).
    /// - `AllowlistDecision::Allow` if a matching pattern includes
    ///   the requested family (or `"full"`).
    /// - `AllowlistDecision::Deny` if a pattern matches but does
    ///   not list the family.
    /// - `AllowlistDecision::Deny` if no pattern matches and there
    ///   is no `"*"` wildcard.
    ///
    /// Pattern matching: exact match wins; otherwise the wildcard
    /// `"*"` is consulted. Multiple-pattern precedence follows
    /// longest-prefix order with stable tie-break by config order
    /// (since `HashMap` is unordered, we sort by key length
    /// descending for the comparison).
    #[must_use]
    pub fn allowlist_decision(&self, agent_id: Option<&str>, family: &str) -> AllowlistDecision {
        let table = match self.allowlist.as_ref() {
            Some(t) if !t.is_empty() => t,
            _ => return AllowlistDecision::Disabled,
        };
        // Tier-1: no agent_id → only the wildcard rule applies. Same
        // restrictive default as for an unknown agent.
        let aid = agent_id.unwrap_or("");
        // Exact match first.
        if let Some(families) = table.get(aid) {
            return decide(families, family);
        }
        // Longest-prefix match next (excluding `"*"`).
        let mut keys: Vec<&String> = table
            .keys()
            .filter(|k| k.as_str() != "*" && aid.starts_with(k.as_str()))
            .collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        if let Some(k) = keys.first() {
            if let Some(families) = table.get(*k) {
                return decide(families, family);
            }
        }
        // Wildcard fallback.
        if let Some(families) = table.get("*") {
            return decide(families, family);
        }
        AllowlistDecision::Deny
    }
}

fn decide(families: &[String], requested: &str) -> AllowlistDecision {
    if families.iter().any(|f| f == "full" || f == requested) {
        AllowlistDecision::Allow
    } else {
        AllowlistDecision::Deny
    }
}

/// v0.6.4-008 — outcome of an allowlist check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistDecision {
    /// Allowlist is not configured; no gate.
    Disabled,
    /// Pattern match grants access to the requested family.
    Allow,
    /// Pattern match denies (or no pattern matched).
    Deny,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditComplianceConfig {
    pub soc2: Option<CompliancePreset>,
    pub hipaa: Option<CompliancePreset>,
    pub gdpr: Option<CompliancePreset>,
    pub fedramp: Option<CompliancePreset>,
}

impl AuditComplianceConfig {
    /// Iterate over every preset whose `applied = true`.
    pub fn applied_presets(&self) -> impl Iterator<Item = &CompliancePreset> {
        [
            self.soc2.as_ref(),
            self.hipaa.as_ref(),
            self.gdpr.as_ref(),
            self.fedramp.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|p| p.applied.unwrap_or(false))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompliancePreset {
    pub applied: Option<bool>,
    pub retention_days: Option<u32>,
    pub redact_content: Option<bool>,
    pub attestation_cadence_minutes: Option<u32>,
    /// Reserved for compliance contexts that mandate at-rest crypto.
    /// HIPAA preset surfaces this so operators can pair audit with
    /// `--features sqlcipher` for end-to-end at-rest encryption.
    pub encrypt_at_rest: Option<bool>,
    /// GDPR-style actor pseudonymization toggle. Reserved for v0.7+.
    pub pseudonymize_actors: Option<bool>,
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

    /// v0.7.0 K3 — resolve the effective [`PermissionsMode`] consulted
    /// by [`crate::db::enforce_governance`].
    ///
    /// Resolution order:
    /// 1. `AI_MEMORY_PERMISSIONS_MODE` env var (`enforce` /
    ///    `advisory` / `off`, case-insensitive). Lets the integration
    ///    suite — which sets `AI_MEMORY_NO_CONFIG=1` and therefore
    ///    cannot use `[permissions]` from `config.toml` — flip the
    ///    gate to Enforce per scenario.
    /// 2. `[permissions].mode` from `config.toml`.
    /// 3. Compiled default ([`PermissionsMode::default`] = `advisory`).
    #[must_use]
    pub fn effective_permissions_mode(&self) -> PermissionsMode {
        if let Ok(raw) = std::env::var("AI_MEMORY_PERMISSIONS_MODE") {
            match raw.to_ascii_lowercase().as_str() {
                "enforce" => return PermissionsMode::Enforce,
                "advisory" => return PermissionsMode::Advisory,
                "off" => return PermissionsMode::Off,
                other => {
                    eprintln!(
                        "ai-memory: AI_MEMORY_PERMISSIONS_MODE={other:?} is not a valid mode \
                         (expected enforce / advisory / off); falling back to config.toml"
                    );
                }
            }
        }
        self.permissions
            .as_ref()
            .map_or_else(PermissionsMode::default, |p| p.mode)
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

    /// v0.6.4-001 — resolve the effective MCP tool profile.
    ///
    /// Resolution order:
    /// 1. `cli_or_env` (already merged by clap's `#[arg(env="AI_MEMORY_PROFILE")]`)
    /// 2. `[mcp].profile` config field
    /// 3. compiled default `"core"`
    ///
    /// # Errors
    ///
    /// Returns [`crate::profile::ProfileParseError`] if any layer's
    /// value is malformed (unknown family or mixed-case token).
    pub fn effective_profile(
        &self,
        cli_or_env: Option<&str>,
    ) -> Result<crate::profile::Profile, crate::profile::ProfileParseError> {
        let raw = cli_or_env
            .or_else(|| self.mcp.as_ref().and_then(|m| m.profile.as_deref()))
            .unwrap_or("core");
        crate::profile::Profile::parse(raw)
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

    /// Resolve the [`LoggingConfig`] block, returning a default
    /// (disabled) instance when the config file omits it.
    pub fn effective_logging(&self) -> LoggingConfig {
        self.logging.clone().unwrap_or_default()
    }

    /// Resolve the [`AuditConfig`] block, returning a default
    /// (disabled) instance when the config file omits it.
    pub fn effective_audit(&self) -> AuditConfig {
        self.audit.clone().unwrap_or_default()
    }

    /// Resolve the [`BootConfig`] block, returning a default
    /// (enabled, no redaction) instance when the config file omits
    /// it. v0.6.3.1 (PR-9h / issue #487 PR #497 req #73).
    pub fn effective_boot(&self) -> BootConfig {
        self.boot.clone().unwrap_or_default()
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

# v0.6.3.1 (PR-5 / issue #487) — operational logging facility.
# Default-OFF. Uncomment + set enabled = true to capture every
# `tracing::*` call site to a rotating on-disk log file. See
# `docs/security/audit-trail.md` §SIEM ingestion guide for Splunk /
# Datadog / Elastic / Loki recipes.
# [logging]
# enabled = false
# path = "~/.local/state/ai-memory/logs/"
# max_size_mb = 100
# max_files = 30
# retention_days = 90
# structured = false              # true = emit JSON lines for SIEM ingest
# level = "info"                  # tracing EnvFilter directive
# rotation = "daily"              # minutely | hourly | daily | never

# v0.6.3.1 (PR-5 / issue #487) — security audit trail. Default-OFF.
# When enabled, every memory mutation emits one hash-chained JSON
# line per event suitable for SOC2 / HIPAA / GDPR / FedRAMP evidence.
# `ai-memory audit verify` walks the chain; `ai-memory logs tail`
# streams events.
# [audit]
# enabled = false
# path = "~/.local/state/ai-memory/audit/"
# schema_version = 1
# redact_content = true            # v1 schema never emits content; reserved
# hash_chain = true
# attestation_cadence_minutes = 60
# append_only = true               # best-effort chflags(2) / FS_IOC_SETFLAGS

# Compliance presets. Set `applied = true` and the documented retention
# / cadence values override the defaults above. See
# `docs/security/audit-trail.md` §Compliance.
# [audit.compliance.soc2]
# applied = false
# retention_days = 730
# redact_content = true
# attestation_cadence_minutes = 60
#
# [audit.compliance.hipaa]
# applied = false
# retention_days = 2190
# redact_content = true
# encrypt_at_rest = true           # pair with --features sqlcipher
#
# [audit.compliance.gdpr]
# applied = false
# retention_days = 1095
# redact_content = true
# pseudonymize_actors = true       # reserved for v0.7+
#
# [audit.compliance.fedramp]
# applied = false
# retention_days = 1095
# redact_content = true
# attestation_cadence_minutes = 30

# v0.6.3.1 (PR-9h / issue #487 PR #497 req #73) — boot privacy controls.
# Default-ON (omit the section entirely for the historical pre-v0.6.3.1
# behavior). Two knobs:
#
# - `enabled = false` silences `ai-memory boot` entirely: empty stdout,
#   empty stderr, exit 0. The SessionStart hook injects nothing. Use on
#   privacy-sensitive hosts where memory titles must never enter CI
#   logs. The env var `AI_MEMORY_BOOT_ENABLED=0` takes precedence over
#   this config (same precedence pattern as PR-5's log-dir resolution).
#
# - `redact_titles = true` keeps the manifest header but replaces row
#   `title` fields with `<redacted>` — useful for compliance contexts
#   that need the audit-trail signal of "boot ran with N memories"
#   without exposing memory subjects.
# [boot]
# enabled = true
# redact_titles = false
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
        let _gate = lock_permissions_mode_for_test();
        // K3 default is `advisory` — clear any override that a
        // sibling test might have left behind so the
        // `permissions.mode` field reflects the documented zero-state.
        clear_permissions_mode_override_for_test();
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
        // v0.6.3.1 (P4, audit G1): inheritance posture surfaced.
        assert_eq!(val["permissions"]["inheritance"], "enforced");

        // hooks zero-state: 0 registered. `by_event` dropped from v2.
        assert_eq!(val["hooks"]["registered_count"], 0);
        assert!(
            val["hooks"].get("by_event").is_none(),
            "v2 honesty patch drops `hooks.by_event` (no event registry)"
        );

        // hooks zero-state: 0 registered, by_event dropped (P1 honesty)
        assert_eq!(val["hooks"]["registered_count"], 0);
        assert!(
            val["hooks"].get("by_event").is_none(),
            "v2 drops hooks.by_event (no event registry)"
        );
        // P5 (G9): webhook_events must always surface the canonical
        // five lifecycle events so integrators can pin a subscribe
        // filter against them.
        let events = val["hooks"]["webhook_events"].as_array().unwrap();
        assert_eq!(events.len(), 5);
        for expected in [
            "memory_store",
            "memory_promote",
            "memory_delete",
            "memory_link_created",
            "memory_consolidated",
        ] {
            assert!(
                events.iter().any(|v| v.as_str() == Some(expected)),
                "webhook_events missing {expected}"
            );
        }

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

        // v0.7 J1 — kg_backend zero-state: no SAL adapter wired yet,
        // so the field is None and elided from the JSON wire. Older
        // clients that don't know the field round-trip cleanly.
        assert!(
            val.get("kg_backend").is_none(),
            "kg_backend must be skipped from JSON when None (pre-J2 zero-state)"
        );

        // Round-trip back to a typed Capabilities and confirm field
        // identity (proves Deserialize works for all reshaped structs).
        let restored: Capabilities = serde_json::from_value(val).unwrap();
        assert_eq!(restored.schema_version, "2");
        assert_eq!(restored.permissions.mode, "advisory");
        assert!(restored.compaction.status.planned);
        assert!(restored.transcripts.status.planned);
        assert_eq!(restored.features.recall_mode_active, RecallMode::Disabled);
        assert_eq!(restored.features.reranker_active, RerankerMode::Off);
        assert!(restored.kg_backend.is_none());
    }

    /// v0.7 J1 — when a SAL adapter populates `kg_backend`, the wire
    /// shape must serialise the literal snake-case tag and round-trip
    /// cleanly. Operators read this through `ai-memory doctor` and
    /// `memory_capabilities` to verify which traversal path their
    /// daemon actually runs.
    #[test]
    fn capabilities_kg_backend_serialises_when_set() {
        let mut caps = FeatureTier::Keyword.config().capabilities();
        caps.kg_backend = Some("age".to_string());
        let val: serde_json::Value = serde_json::to_value(&caps).unwrap();
        assert_eq!(val["kg_backend"], "age");

        caps.kg_backend = Some("cte".to_string());
        let val: serde_json::Value = serde_json::to_value(&caps).unwrap();
        assert_eq!(val["kg_backend"], "cte");

        // Round-trip the populated field for Deserialize coverage.
        let restored: Capabilities = serde_json::from_value(val).unwrap();
        assert_eq!(restored.kg_backend.as_deref(), Some("cte"));
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

    // ---- v0.6.4-001 — `effective_profile` resolution tests.
    //
    // Resolution order: CLI/env > [mcp].profile config > "core" default.
    // Clap merges CLI and env into the same `Option<&str>` before this
    // function sees it, so the function only needs to test "explicit
    // override > config > default". Env-var precedence over CLI cannot
    // happen by design (clap precedence is CLI > env), so it is not
    // tested at this layer.

    #[test]
    fn effective_profile_cli_or_env_overrides_config() {
        let cfg = AppConfig {
            mcp: Some(McpConfig {
                profile: Some("graph".to_string()),
                allowlist: None,
            }),
            ..AppConfig::default()
        };
        // CLI/env value beats the config value.
        assert_eq!(
            cfg.effective_profile(Some("admin")).unwrap(),
            crate::profile::Profile::admin()
        );
        // No CLI/env → config used.
        assert_eq!(
            cfg.effective_profile(None).unwrap(),
            crate::profile::Profile::graph()
        );
    }

    #[test]
    fn effective_profile_falls_back_to_core_default() {
        let cfg = AppConfig::default();
        // No mcp config, no CLI → core (the v0.6.4 default flip).
        assert_eq!(
            cfg.effective_profile(None).unwrap(),
            crate::profile::Profile::core()
        );
    }

    #[test]
    fn effective_profile_surfaces_parse_error_for_unknown_family() {
        let cfg = AppConfig::default();
        assert!(matches!(
            cfg.effective_profile(Some("xyz")),
            Err(crate::profile::ProfileParseError::UnknownFamily(_))
        ));
    }

    #[test]
    fn effective_profile_surfaces_parse_error_for_mixed_case() {
        let cfg = AppConfig::default();
        assert!(matches!(
            cfg.effective_profile(Some("Core")),
            Err(crate::profile::ProfileParseError::CaseMismatch(_))
        ));
    }

    // ---- v0.6.4-008 — `[mcp.allowlist]` resolution tests.

    fn allowlist_table(rows: &[(&str, &[&str])]) -> McpConfig {
        let mut map = std::collections::HashMap::new();
        for (k, v) in rows {
            map.insert(
                (*k).to_string(),
                v.iter().map(|s| (*s).to_string()).collect(),
            );
        }
        McpConfig {
            profile: None,
            allowlist: Some(map),
        }
    }

    #[test]
    fn allowlist_disabled_when_table_absent() {
        let cfg = McpConfig::default();
        assert_eq!(
            cfg.allowlist_decision(Some("alice"), "graph"),
            AllowlistDecision::Disabled
        );
    }

    #[test]
    fn allowlist_disabled_when_table_empty() {
        let cfg = McpConfig {
            profile: None,
            allowlist: Some(std::collections::HashMap::new()),
        };
        assert_eq!(
            cfg.allowlist_decision(Some("alice"), "graph"),
            AllowlistDecision::Disabled
        );
    }

    #[test]
    fn allowlist_exact_match_grants_or_denies_per_family_set() {
        let cfg = allowlist_table(&[("alice", &["core", "graph"]), ("*", &["core"])]);
        assert_eq!(
            cfg.allowlist_decision(Some("alice"), "graph"),
            AllowlistDecision::Allow
        );
        assert_eq!(
            cfg.allowlist_decision(Some("alice"), "power"),
            AllowlistDecision::Deny
        );
    }

    #[test]
    fn allowlist_full_grants_every_family() {
        let cfg = allowlist_table(&[("bob", &["full"])]);
        assert_eq!(
            cfg.allowlist_decision(Some("bob"), "graph"),
            AllowlistDecision::Allow
        );
        assert_eq!(
            cfg.allowlist_decision(Some("bob"), "archive"),
            AllowlistDecision::Allow
        );
    }

    #[test]
    fn allowlist_wildcard_default_for_unknown_agents() {
        let cfg = allowlist_table(&[("alice", &["full"]), ("*", &["core"])]);
        assert_eq!(
            cfg.allowlist_decision(Some("eve"), "core"),
            AllowlistDecision::Allow
        );
        assert_eq!(
            cfg.allowlist_decision(Some("eve"), "graph"),
            AllowlistDecision::Deny
        );
    }

    #[test]
    fn allowlist_default_deny_when_no_wildcard() {
        let cfg = allowlist_table(&[("alice", &["full"])]);
        assert_eq!(
            cfg.allowlist_decision(Some("eve"), "core"),
            AllowlistDecision::Deny
        );
    }

    #[test]
    fn allowlist_longest_prefix_match_wins() {
        let cfg = allowlist_table(&[
            ("ai:", &["core"]),
            ("ai:claude-code", &["full"]),
            ("*", &["core"]),
        ]);
        // The longer prefix takes precedence over the shorter one.
        assert_eq!(
            cfg.allowlist_decision(Some("ai:claude-code@host"), "graph"),
            AllowlistDecision::Allow
        );
        // Shorter prefix still works for other ai:* agents.
        assert_eq!(
            cfg.allowlist_decision(Some("ai:codex@host"), "graph"),
            AllowlistDecision::Deny
        );
    }

    #[test]
    fn allowlist_no_agent_id_uses_wildcard() {
        // Tier-1 / anonymous: no agent_id provided → only the wildcard
        // rule is consulted.
        let cfg = allowlist_table(&[("alice", &["full"]), ("*", &["core"])]);
        assert_eq!(
            cfg.allowlist_decision(None, "core"),
            AllowlistDecision::Allow
        );
        assert_eq!(
            cfg.allowlist_decision(None, "graph"),
            AllowlistDecision::Deny
        );
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
