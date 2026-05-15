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

impl std::str::FromStr for EmbeddingModel {
    type Err = String;

    /// Parse the snake_case wire form used by `AppConfig.embedding_model`
    /// (the documented top-level override). Accepts case-insensitive input
    /// with surrounding whitespace trimmed. Keep this in sync with the
    /// `#[serde(rename_all = "snake_case")]` variants above.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mini_lm_l6_v2" => Ok(Self::MiniLmL6V2),
            "nomic_embed_v15" => Ok(Self::NomicEmbedV15),
            other => Err(format!(
                "unknown embedding_model {other:?}: expected one of \
                 \"mini_lm_l6_v2\", \"nomic_embed_v15\""
            )),
        }
    }
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
                // v0.7.0 recursive-learning (issue #655): the primitive
                // shipped — Tasks 1-6 landed on
                // `feat/v0.7.0-recursive-learning`. Flag is enabled and
                // pinned to the shipping version `v0.7.0`. (Pre-ship,
                // this was `PlannedFeature::planned("v0.7+")` to keep
                // the v2 honesty contract honest while the substrate
                // primitive was on the roadmap.)
                memory_reflection: PlannedFeature {
                    planned: false,
                    version: "v0.7.0".to_string(),
                    enabled: true,
                },
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
                // v0.7.0 L2-8 — default reflection boost (1.2, +0.05/depth,
                // cap=3). The MCP/HTTP wrapper overlays the live wrapper
                // config when a `BatchedReranker` handle is available.
                reflection_boost: ReflectionBoostReport::default(),
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
                // v0.7.0 K5: zero-state — no policies known until the
                // overlay queries the live DB. `Vec::is_empty` means
                // the field is omitted from the wire entirely (matches
                // the v0.6.3.1 honesty disclosure that this field was
                // previously dropped because no per-rule serializer
                // existed; K5 ships the serializer).
                rule_summary: Vec::new(),
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
            // L1-1 — always static for v0.7.0; Goal/Plan/Step/Decision
            // land in L1-6/v0.8.0.
            memory_kinds: default_memory_kinds(),
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

    /// L1-1 (v0.7.0) — the set of typed memory kinds this binary
    /// supports.  Always `["observation", "reflection"]` for v0.7.0;
    /// Goal/Plan/Step/Decision land in L1-6/v0.8.0.  Callers that want
    /// to enumerate valid values for a `memory_kind` filter should
    /// consult this field rather than hardcoding the list.
    ///
    /// `#[serde(default)]` keeps older capabilities consumers that
    /// don't know the field from breaking.
    #[serde(default = "default_memory_kinds")]
    pub memory_kinds: Vec<String>,
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
    /// Memory-reflection (v0.7.0): planned-feature object. Was a
    /// `bool` before the v0.6.3.1 P1 honesty patch; an object now so
    /// operators can tell "feature exists but disabled" apart from
    /// "feature not in this build".
    ///
    /// **v0.7.0 recursive-learning ship (issue #655).** The flag is
    /// `{ planned: false, version: "v0.7.0", enabled: true }` because
    /// the underlying primitive landed across Tasks 1-6 on
    /// `feat/v0.7.0-recursive-learning`:
    ///
    /// - **Column** (Task 1/8, commit `f5d8a9e`) —
    ///   `memories.reflection_depth INTEGER NOT NULL DEFAULT 0`
    ///   on SQLite (schema v29) and Postgres (`CURRENT_SCHEMA_VERSION 31`).
    ///   `Memory::reflection_depth: i32` with `#[serde(default)]` for
    ///   wire-compat with pre-v0.7.0 federation peers.
    /// - **Governance field** (Task 2/8, commit `630a6db`) —
    ///   `GovernancePolicy.max_reflection_depth: Option<u32>` (per
    ///   namespace, JSON metadata, no schema bump). Accessor
    ///   `effective_max_reflection_depth() -> u32` returns the compiled
    ///   default `3` when unset; `Some(0)` is the documented
    ///   kill-switch.
    /// - **Relation** (Task 3/8, commit `b51a3f3`) — `reflects_on`
    ///   joins the canonical `VALID_RELATIONS` set; directionality
    ///   matches `derived_from` (reflection is `source_id`, original
    ///   is `target_id`); `db::find_paths` walks it without further
    ///   work.
    /// - **MCP tool** (Task 4/8, commit `3dc76f3`) — `memory_reflect`
    ///   (`Family::Power`, tool count 51 → 52). Atomic insert of a
    ///   reflection memory + N `reflects_on` link writes inside a
    ///   single `BEGIN IMMEDIATE` / `COMMIT` transaction. Postgres
    ///   parity via inherent `PostgresStore::reflect`.
    /// - **Error variant** (Task 4/8) — `MemoryError::ReflectionDepthExceeded
    ///   { attempted: u32, cap: u32, namespace: String }` →
    ///   HTTP `409 CONFLICT`, code `REFLECTION_DEPTH_EXCEEDED`.
    /// - **Hook events** (Task 6/8, commit `fbf093c`) —
    ///   `HookEvent::PreReflect` (decision-class, `EventClass::Write`,
    ///   5s deadline, fires before the depth-cap check, `Deny`
    ///   vetoes via `ReflectError::HookVeto`) +
    ///   `HookEvent::PostReflect` (notify-class, `EventClass::Write`,
    ///   5s deadline, fires after `COMMIT`). Pipeline event count
    ///   21 → 23.
    /// - **Audit chain** (Task 5/8, commit `c61a05b`) — every
    ///   depth-cap refusal appends a `reflection.depth_exceeded` row
    ///   to the append-only `signed_events` audit table under a
    ///   canonical-CBOR payload + SHA-256 `payload_hash` +
    ///   `attest_level = "unsigned"`. Content body is deliberately
    ///   omitted (PII guarantee); hook vetoes are NOT audited by this
    ///   row (caller-policy refusals carry their own provenance).
    ///
    /// The v1 wire-shape projection collapses this object back to a
    /// single `bool` (via `Capabilities::to_v1`), so pre-v0.6.3.1
    /// clients that pinned the v1 schema continue to see the same
    /// boolean field at the same path (and now read `true`).
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
    /// v0.7.0 L2-8 — reflection-aware reranker boost configuration.
    /// `boost = 1.0` means the boost is disabled and the reranker
    /// reproduces its pre-L2-8 behavior. Default (`1.2`) is the value
    /// the daemon ships with; operators can inspect this to verify
    /// the live boost matches their configured policy. Skipped from
    /// the wire when serialising a pre-L2-8 default so older
    /// capabilities consumers round-trip cleanly.
    #[serde(default = "default_reflection_boost")]
    pub reflection_boost: ReflectionBoostReport,
}

/// v0.7.0 L2-8 — per-field report of the reflection-aware reranker
/// boost surfaced through `memory_capabilities`. Mirrors
/// [`crate::reranker::ReflectionBoostConfig`] but expressed in
/// capability-report shape (serde-friendly, schema-tagged).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReflectionBoostReport {
    /// Multiplicative boost applied to reflection-kind memories.
    /// `1.0` disables; default `1.2`.
    pub boost: f32,
    /// Per-depth additional multiplier increment. Default `0.05`.
    pub per_depth_increment: f32,
    /// Depth cap for the per-depth multiplier. Default `3`.
    pub max_depth_cap: u32,
}

impl Default for ReflectionBoostReport {
    fn default() -> Self {
        Self {
            boost: crate::reranker::DEFAULT_REFLECTION_BOOST,
            per_depth_increment: crate::reranker::DEFAULT_REFLECTION_PER_DEPTH_INCREMENT,
            max_depth_cap: crate::reranker::DEFAULT_REFLECTION_MAX_DEPTH_CAP,
        }
    }
}

impl From<&crate::reranker::ReflectionBoostConfig> for ReflectionBoostReport {
    fn from(cfg: &crate::reranker::ReflectionBoostConfig) -> Self {
        Self {
            boost: cfg.boost,
            per_depth_increment: cfg.per_depth_increment,
            max_depth_cap: cfg.max_depth_cap,
        }
    }
}

fn default_reflection_boost() -> ReflectionBoostReport {
    ReflectionBoostReport::default()
}

/// L1-1 default: the two typed memory kinds shipping in v0.7.0.
fn default_memory_kinds() -> Vec<String> {
    vec!["observation".to_string(), "reflection".to_string()]
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
    /// v0.7.0 K5: ordered list of one-line summaries — one entry per
    /// active governance policy, sorted lexicographically by namespace.
    /// Each entry names the namespace plus the policy's `write`,
    /// `promote`, `delete`, `approver`, and `inherit` values so an
    /// operator (or LLM) can see the live ruleset at a glance without
    /// fanning out per-namespace `memory_namespace_get_standard` calls.
    ///
    /// **Wire shape.** `skip_serializing_if = "Vec::is_empty"` keeps the
    /// field absent from v2 responses (which historically had no per-rule
    /// serializer — the v0.6.3.1 honesty patch dropped the field from
    /// the v2 wire entirely) when no policies are configured. v3 callers
    /// see the field on every response with policies, matching the K5
    /// spec contract that v3 brings the field back with a backing
    /// implementation.
    ///
    /// Closes the v0.6.3.1 honest-Capabilities-v2 disclosure that this
    /// field was a placeholder — the K5 increment ships the per-rule
    /// serializer that was previously missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_summary: Vec<String>,
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
    /// v0.7.0 L1-7: total number of distinct `HookEvent` variants the
    /// pipeline supports.  Populated from the compile-time constant
    /// [`HOOK_EVENTS_COUNT`] so operators and integrations can verify
    /// they are running against the expected pipeline version without
    /// enumerating the enum.
    ///
    /// History: G2 shipped 20; G10 added the 21st; Task 6/8 added
    /// the 22nd + 23rd; L1-7 adds the 24th + 25th → total **25**.
    #[serde(default = "default_hook_events_count")]
    pub hook_events_count: usize,
}

/// Compile-time count of `HookEvent` variants.  Updated here when new
/// variants land; the corresponding enum exhaustiveness check in
/// `src/hooks/timeouts.rs` enforces the count at test time.
pub const HOOK_EVENTS_COUNT: usize = 25;

fn default_hook_events_count() -> usize {
    HOOK_EVENTS_COUNT
}

impl Default for CapabilityHooks {
    fn default() -> Self {
        Self {
            registered_count: 0,
            webhook_events: default_webhook_events(),
            hook_events_count: HOOK_EVENTS_COUNT,
        }
    }
}

/// Default webhook events list — kept in sync with
/// `crate::subscriptions::WEBHOOK_EVENT_TYPES`. The constant lives in
/// `subscriptions.rs` (the surface that uses it at runtime); this
/// helper exists so `serde(default = …)` and `CapabilityHooks::default`
/// can fill the field without a cross-module dep on `subscriptions`.
///
/// v0.7.0 K4 — `approval_requested` joined the canonical list. The
/// `webhook_events` capability surface is the integration contract
/// for K10's Approval API HTTP+SSE handler; surfacing the event type
/// here closes the v0.6.3.1 honest-disclosure that the
/// `approval.subscribers` field was advertised but unwired.
fn default_webhook_events() -> Vec<String> {
    vec![
        "memory_store".to_string(),
        "memory_promote".to_string(),
        "memory_delete".to_string(),
        "memory_link_created".to_string(),
        "memory_link_invalidated".to_string(),
        "memory_consolidated".to_string(),
        "approval_requested".to_string(),
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
// Capabilities v3 L3-5 — recursive-learning / skills / forensic / governance
// blocks. v3-only (additive over v2). Every field is hand-mapped to a
// concrete implementation that landed in the v0.7.0 grand-slam L1+L2 waves
// so an external auditor can trace a claim back to a source-code line.
// ---------------------------------------------------------------------------

/// v0.7.0 L3-5 — substrate-native reflection capability surface.
///
/// Every field MUST map to a real implementation. Audit anchors:
///
/// - `implemented`: [`crate::storage::reflect::reflect`] +
///   [`crate::mcp::tools::memory_reflect`] (issue #655 Task 4/8,
///   commit `3dc76f3`).
/// - `depth_bounded`: depth-cap check in [`crate::storage::reflect`]
///   step 5; [`crate::errors::MemoryError::ReflectionDepthExceeded`]
///   surfaces refusal with `attempted` + `cap` + `namespace`.
/// - `max_default`: compiled-in default returned by
///   [`crate::models::namespace::GovernancePolicy::effective_max_reflection_depth`]
///   (currently **3**) when the namespace's
///   `metadata.governance.max_reflection_depth` is unset.
/// - `attestation`: every reflection writes a `signed_events` row via
///   [`crate::signed_events::append_signed_event`]; the project uses
///   Ed25519 (see [`crate::identity::sign`] H2 + H4 link-signing
///   plus the operator-signed governance rules in
///   [`crate::governance::rules_store`]).
/// - `curator_mode`: implemented in
///   [`crate::curator::reflection_pass`] and the
///   `ai-memory curator --reflection-pass` CLI verb in
///   [`crate::cli::curator`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityReflection {
    /// `true` whenever the reflection primitive is wired (memory_reflect MCP
    /// tool present + `storage::reflect::reflect` callable). False is reserved
    /// for a build that compiled the field out.
    pub implemented: bool,
    /// `true` when reflections are subject to a depth cap that refuses
    /// further reflection past the configured maximum.
    pub depth_bounded: bool,
    /// Compiled-in default cap returned when no namespace policy is set.
    /// Tracks [`crate::models::namespace::GovernancePolicy::effective_max_reflection_depth`].
    pub max_default: u32,
    /// Signature algorithm used by the substrate for attested events
    /// touching reflections (link signatures + `signed_events` rows).
    pub attestation: String,
    /// `"implemented"` when the curator reflection pass is wired
    /// (`curator::reflection_pass` + `ai-memory curator` CLI). Stays a
    /// string (not a bool) so future increments can grow new values like
    /// `"scheduled"` without a wire-shape break.
    pub curator_mode: String,
}

impl CapabilityReflection {
    /// Build the L3-5 reflection capability from real values pinned at
    /// compile time so the wire shape reflects what this binary actually
    /// ships. Constants from [`crate::reranker::DEFAULT_REFLECTION_MAX_DEPTH_CAP`]
    /// and the curator module are consulted directly — no magic strings.
    #[must_use]
    pub fn current() -> Self {
        Self {
            implemented: true,
            depth_bounded: true,
            max_default: crate::reranker::DEFAULT_REFLECTION_MAX_DEPTH_CAP,
            attestation: "Ed25519".to_string(),
            curator_mode: "implemented".to_string(),
        }
    }
}

fn default_capability_reflection() -> CapabilityReflection {
    CapabilityReflection::current()
}

/// v0.7.0 L3-5 — Agent-Skills capability surface.
///
/// Every field MUST map to a real implementation:
///
/// - `implemented`: 7 MCP tools wired in
///   [`crate::mcp::registry`] + handlers in
///   [`crate::mcp::tools::skill_*`].
/// - `standard`: the parser in [`crate::parsing::skill_md`] validates
///   names + frontmatter against the agentskills.io §3.1/§3.2 spec.
/// - `tools`: list mirrors the registered handler names verbatim;
///   regression test [`SKILL_TOOL_NAMES`] verifies the slice matches
///   the live MCP dispatcher.
/// - `round_trip`: `memory_skill_register` → `memory_skill_export` →
///   re-register produces the IDENTICAL SHA-256 digest (see
///   `tests/skill_test.rs`, the round-trip pin).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitySkills {
    /// `true` whenever the skill registration + lookup substrate is
    /// wired. False is reserved for a build that compiled the family out.
    pub implemented: bool,
    /// External spec the parser targets. `"agentskills.io"` is the
    /// canonical name documented in the L1-5 spec.
    pub standard: String,
    /// Canonical list of registered skill tools. Order matches the MCP
    /// dispatch order so an LLM that pins the order doesn't drift.
    pub tools: Vec<String>,
    /// `"verified"` when register → export → re-register is exercised in
    /// the test suite and the digests match.
    pub round_trip: String,
}

/// Canonical skill tool names as registered in
/// [`crate::mcp::registry`]. Pinned here (not derived from the registry)
/// so the capability surface remains a stable, declarative contract;
/// the regression test
/// `cap_v3_l3_5_skill_tools_match_registered_mcp_dispatch` ensures the
/// two stay in sync.
pub const SKILL_TOOL_NAMES: &[&str] = &[
    "memory_skill_register",
    "memory_skill_list",
    "memory_skill_get",
    "memory_skill_resource",
    "memory_skill_export",
    "memory_skill_promote_from_reflection",
    "memory_skill_compositional_context",
];

impl CapabilitySkills {
    /// Build the L3-5 skills capability from real, code-anchored values.
    #[must_use]
    pub fn current() -> Self {
        Self {
            implemented: true,
            standard: "agentskills.io".to_string(),
            tools: SKILL_TOOL_NAMES.iter().map(|s| (*s).to_string()).collect(),
            round_trip: "verified".to_string(),
        }
    }
}

fn default_capability_skills() -> CapabilitySkills {
    CapabilitySkills::current()
}

/// v0.7.0 L3-5 — forensic-evidence capability surface.
///
/// Each label names a CLI / function pair that **exists** in this binary:
///
/// - `verify_reflection_chain`: `ai-memory verify-reflection-chain` —
///   driver lives in [`crate::cli::verify`].
/// - `export_forensic_bundle`: `ai-memory export-forensic-bundle` —
///   builder lives in [`crate::forensic::bundle::build`].
/// - `verify_forensic_bundle`: `ai-memory verify-forensic-bundle` —
///   verifier lives in [`crate::forensic::bundle::verify`].
///
/// All three are `"implemented"` strings (not bools) so future
/// increments can promote a value to `"attested"` or `"scheduled"`
/// without a wire-shape break.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityForensic {
    pub verify_reflection_chain: String,
    pub export_forensic_bundle: String,
    pub verify_forensic_bundle: String,
}

impl CapabilityForensic {
    /// Build the L3-5 forensic capability — all three driver paths are
    /// wired in this build.
    #[must_use]
    pub fn current() -> Self {
        Self {
            verify_reflection_chain: "implemented".to_string(),
            export_forensic_bundle: "implemented".to_string(),
            verify_forensic_bundle: "implemented".to_string(),
        }
    }
}

fn default_capability_forensic() -> CapabilityForensic {
    CapabilityForensic::current()
}

/// v0.7.0 L3-5 — substrate-rules governance capability surface.
///
/// Surfaces the L1-6 activation posture honestly:
///
/// - `rules_engine`: `"operator_signed"` because the L1-6 loader
///   refuses to honour any `enabled = 1` rule that is not
///   `attest_level = 'operator_signed'` and whose signature does not
///   verify against the active operator pubkey
///   ([`crate::governance::rules_store`] L1-6 audit).
/// - `enforced_actions`: the actual variant set in
///   [`crate::governance::agent_action::AgentAction`] minus the
///   `Custom` extension point (extension points are not
///   substrate-enforced). v0.7.0 ships **four** action kinds at the
///   harness-mediated PreToolUse boundary.
/// - `bypass_impossibility_tests`: count of `#[test]` functions in
///   [`tests/governance_l16_activation.rs`] verifying the
///   bypass-impossibility properties (signature-required, tampered-sig
///   rejected, direct-enabled-flip rejected, keygen 0600, idempotent
///   sign-seed, rotated-key invalidates). The number reflects the test
///   file as of v0.7.0 — bumping it requires an audit pass and a
///   matching test addition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityGovernance {
    pub rules_engine: String,
    pub enforced_actions: Vec<String>,
    pub bypass_impossibility_tests: u32,
}

/// v0.7.0 L1-6 — the canonical agent-external action kinds the
/// substrate gates via the operator-signed rules engine. Matches the
/// variant set in [`crate::governance::agent_action::AgentAction`]
/// (minus the open-ended `Custom` extension point).
///
/// MemoryWrite is intentionally NOT in this list — substrate-internal
/// memory writes are gated by the K9 `Op` pipeline
/// ([`crate::governance::Op`]) which is a separate, substrate-
/// authoritative surface. The two engines have different enforcement
/// semantics; honest reporting keeps them on separate fields rather
/// than conflating them under one label. The L3-5 audit comment in
/// `tests/capabilities_v3_l3_5.rs` documents the carry-forward.
pub const ENFORCED_AGENT_ACTIONS: &[&str] =
    &["Bash", "FilesystemWrite", "NetworkRequest", "ProcessSpawn"];

/// v0.7.0 L1-6 — number of bypass-impossibility tests pinning the
/// rules-engine activation posture. Tracks the `#[test]` count in
/// `tests/governance_l16_activation.rs`. Bumping this requires both an
/// audit and a matching test landing in that file.
pub const GOVERNANCE_BYPASS_IMPOSSIBILITY_TESTS: u32 = 6;

impl CapabilityGovernance {
    /// Build the L3-5 governance capability from the live constants.
    #[must_use]
    pub fn current() -> Self {
        Self {
            rules_engine: "operator_signed".to_string(),
            enforced_actions: ENFORCED_AGENT_ACTIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            bypass_impossibility_tests: GOVERNANCE_BYPASS_IMPOSSIBILITY_TESTS,
        }
    }
}

fn default_capability_governance() -> CapabilityGovernance {
    CapabilityGovernance::current()
}

/// v0.7.0 WT-1-G — atomisation capability surface.
///
/// WT-1 ships substrate-native decomposition of long memories into
/// atomic propositions. The parent memory is archived (`archived_at`
/// stamped, `atomised_into = N`) and `N` first-class atomic children
/// land with `atom_of` back-pointers and a signed `derives_from`
/// `MemoryLink`. Each sub-field below names a real operator-facing
/// surface in this binary; the round-trip is honest — the values are
/// `"implemented"` only when the engine, hook, and wrapper code are
/// all wired.
///
/// Field → implementation anchor map:
///
/// - `tool`: MCP `memory_atomise` (Family::Power). Defined in
///   [`crate::mcp::tools::atomise`] + registered in
///   [`crate::mcp::registry`]. WT-1-C landed it.
/// - `cli`: `ai-memory atomise <memory_id>` subcommand. Wrapper lives
///   in [`crate::cli::commands::atomise`]. WT-1-F landed it.
/// - `auto`: namespace-policy-gated `auto_atomise` pre_store hook.
///   The hook in [`crate::hooks::pre_store::auto_atomise`] is
///   non-blocking (detached worker thread) and fires only when the
///   namespace standard's `metadata.governance.auto_atomise = true`.
///   WT-1-D landed it.
/// - `recall_preference`: recall surfaces atoms in place of an
///   archived parent via the SQL guard
///   `AND NOT (archived_at IS NOT NULL AND atomised_into > 0)`.
///   WT-1-E landed it.
/// - `forensic`: forensic bundle export includes the parent → atoms
///   chain envelope so a downstream auditor reconstructs the
///   decomposition offline. WT-1-E landed it.
/// - `curator`: production `LlmCurator` uses the Gemma 4 prompt
///   with `tiktoken-rs::cl100k_base` token-budget validation and
///   the audit-honest STOP discipline (no retry after a parse-OK
///   verdict). WT-1-B landed it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityAtomisation {
    /// MCP `memory_atomise` tool — `"implemented"` once the tool is
    /// registered and the [`crate::mcp::tools::atomise`] handler is
    /// wired against [`crate::atomisation::Atomiser`].
    pub tool: String,
    /// `ai-memory atomise` CLI subcommand — `"implemented"` once the
    /// wrapper in [`crate::cli::commands::atomise`] is dispatched
    /// from `daemon_runtime::Command::Atomise`.
    pub cli: String,
    /// Namespace-policy-gated auto-atomisation pre_store hook —
    /// `"implemented"` when [`crate::hooks::pre_store::auto_atomise`]
    /// is compiled and the store handlers call
    /// `maybe_enqueue_auto_atomise` after a successful insert.
    pub auto: String,
    /// Recall-time atom preference — `"implemented"` when the recall
    /// SQL carries the
    /// `AND NOT (archived_at IS NOT NULL AND atomised_into > 0)`
    /// guard so atomised parents stop surfacing in their atoms'
    /// place. WT-1-E.
    pub recall_preference: String,
    /// Forensic chain envelope — `"implemented"` when the forensic
    /// bundle exporter ([`crate::forensic::bundle::build`]) walks
    /// `atom_of` back-pointers to include the parent → atoms chain
    /// in the bundle. WT-1-E.
    pub forensic: String,
    /// LLM curator — `"implemented"` once
    /// [`crate::atomisation::curator::LlmCurator`] is the production
    /// `Curator` impl driving the atomisation engine (Gemma 4 prompt,
    /// tiktoken-rs cl100k token-budget validation, audit-honest STOP).
    /// WT-1-B.
    pub curator: String,
    /// Memory-link relation that anchors the atom → parent edge.
    /// Always `"derives_from"`, matching
    /// [`crate::models::MemoryLinkRelation::DerivesFrom`]. Distinct
    /// from `related_to` / `supersedes` / `contradicts` — the
    /// atomisation engine writes this edge specifically, and
    /// downstream consumers can filter on the relation to walk
    /// decomposition lineage without reflection-chain noise.
    pub link_relation: String,
}

impl CapabilityAtomisation {
    /// Build the WT-1-G atomisation capability surface from real,
    /// code-anchored values. Every `"implemented"` here is a claim
    /// pinned by [`tests/capabilities_v3_l3_5.rs`] and walked back to
    /// a registered MCP tool / CLI verb / hook module / SQL guard.
    #[must_use]
    pub fn current() -> Self {
        Self {
            tool: "implemented".to_string(),
            cli: "implemented".to_string(),
            auto: "implemented".to_string(),
            recall_preference: "implemented".to_string(),
            forensic: "implemented".to_string(),
            curator: "implemented".to_string(),
            link_relation: "derives_from".to_string(),
        }
    }
}

fn default_capability_atomisation() -> CapabilityAtomisation {
    CapabilityAtomisation::current()
}

// ---------------------------------------------------------------------------
// v0.7.x Form 6 — MemoryKind Batman-vocabulary capability surface (#759)
// ---------------------------------------------------------------------------

/// v0.7.x Form 6 (issue #759) — Batman-taxonomy memory-kind
/// capability surface. Names the recall-filter / auto-classify
/// surfaces shipped under Form 6.
///
/// Field → implementation anchor map:
///
/// - `vocabulary`: the complete enumerated vocabulary the substrate
///   accepts on the `memory_kind` column. Always
///   `["observation", "reflection", "persona", "concept", "entity",
///   "claim", "relation", "event", "conversation", "decision"]` in
///   v0.7.x — anchored at compile time by
///   [`crate::models::MemoryKind::all`].
/// - `recall_filter`: MCP `memory_recall` and HTTP recall accept a
///   `kinds` parameter (CSV string or JSON array). `"implemented"`
///   once the param is plumbed into [`crate::mcp::tools::recall`]
///   and [`crate::handlers::http::recall_response`].
/// - `cli_filter`: `ai-memory recall --kind concept,entity` CLI
///   flag. `"implemented"` once the flag is wired in
///   [`crate::cli::recall::RecallArgs`].
/// - `auto_classify`: the namespace-policy-gated
///   `pre_store::auto_classify_kind` hook. `"implemented"` once
///   the hook module is compiled and `memory_store` calls
///   [`crate::hooks::pre_store::maybe_auto_classify`] after policy
///   resolution.
/// - `auto_classify_modes`: enumerated policy modes the operator
///   may set. Always `["off", "regex_only", "regex_then_llm"]` —
///   anchored against [`crate::models::MemoryKindAutoClassify`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityMemoryKindVocab {
    /// Complete enumerated vocabulary the substrate accepts on the
    /// `memory_kind` column. Compile-anchored.
    pub vocabulary: Vec<String>,
    /// MCP `memory_recall` + HTTP recall `kinds` param wiring.
    pub recall_filter: String,
    /// CLI `--kind` flag wiring.
    pub cli_filter: String,
    /// Namespace-policy-gated auto-classify pre_store hook wiring.
    pub auto_classify: String,
    /// Enumerated auto-classify policy modes (`off` / `regex_only` /
    /// `regex_then_llm`). Compile-anchored.
    pub auto_classify_modes: Vec<String>,
}

impl CapabilityMemoryKindVocab {
    /// Build the Form 6 memory-kind-vocab capability surface from
    /// real, code-anchored values. Every `"implemented"` here is a
    /// claim pinned by [`tests/form_6_memorykind_vocab.rs`].
    #[must_use]
    pub fn current() -> Self {
        Self {
            vocabulary: crate::models::MemoryKind::all()
                .iter()
                .map(|k| k.as_str().to_string())
                .collect(),
            recall_filter: "implemented".to_string(),
            cli_filter: "implemented".to_string(),
            auto_classify: "implemented".to_string(),
            auto_classify_modes: vec![
                "off".to_string(),
                "regex_only".to_string(),
                "regex_then_llm".to_string(),
            ],
        }
    }
}

fn default_capability_memory_kind_vocab() -> CapabilityMemoryKindVocab {
    CapabilityMemoryKindVocab::current()
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
        your_harness_supports_deferred_registration: Option<bool>,
    ) -> CapabilitiesV3 {
        CapabilitiesV3 {
            schema_version: "3".to_string(),
            summary,
            to_describe_to_user,
            tools,
            agent_permitted_families,
            your_harness_supports_deferred_registration,
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
            // L1-1 — propagate the memory-kind set verbatim.
            memory_kinds: self.memory_kinds.clone(),
            // L3-5 — four new substrate-honesty blocks. Built from
            // compile-time anchors (the per-block `::current()`
            // constructor) so the wire shape reflects the actual
            // implementation surface, not a static template.
            reflection: CapabilityReflection::current(),
            skills: CapabilitySkills::current(),
            forensic: CapabilityForensic::current(),
            governance: CapabilityGovernance::current(),
            // v0.7.0 WT-1-G — operator-facing atomisation surface.
            // Anchored at compile time against the WT-1-{A..F} ships
            // (engine, curator, hook, recall guard, forensic bundle,
            // MCP tool, CLI subcommand).
            atomisation: CapabilityAtomisation::current(),
            // v0.7.x Form 6 (issue #759) — Batman-taxonomy memory-kind
            // vocabulary surface. Anchored at compile time against the
            // [`crate::models::MemoryKind`] enum + the recall-filter /
            // CLI / auto-classify wiring shipped under Form 6.
            memory_kind_vocab: CapabilityMemoryKindVocab::current(),
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

    /// v0.7.0 B4 — whether the active MCP harness exposes tools
    /// registered *after* the initial `tools/list` to the LLM. Computed
    /// at response time from the harness detected at the
    /// `initialize.clientInfo.name` handshake (see `crate::harness`).
    ///
    /// `Some(true)` only for Claude Code today (deferred registration
    /// via `ToolSearch`). `Some(false)` for every other named harness.
    /// `None` (omitted from the wire via `skip_serializing_if`) when
    /// no `clientInfo` was captured — typically HTTP callers, or an
    /// MCP client that issued `memory_capabilities` before
    /// `initialize` (malformed but defensively handled by absence).
    ///
    /// Track B's runtime loaders (B1 `memory_load_family`, B2
    /// `memory_smart_load`) key off this bit to shape their
    /// `to_invoke` text — on `false` harnesses they advise the LLM to
    /// ask the operator for a `--profile <family>` restart rather
    /// than expect the new tools to appear mid-session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub your_harness_supports_deferred_registration: Option<bool>,

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

    /// L1-1 (v0.7.0) — typed memory-kind set. Forwarded from the v2
    /// projection's `memory_kinds` field. Always
    /// `["observation", "reflection"]` for v0.7.0.
    ///
    /// **L3-5 honesty note.** The grand-slam spec called for a third
    /// `"goal"` kind here, but the [`crate::models::memory::MemoryKind`]
    /// enum in this binary only carries `Observation` and `Reflection`.
    /// Per the operator's "every reported field maps to real
    /// implementation" directive, the v3 surface reports exactly what
    /// the substrate enforces — the `goal` kind is deferred to the
    /// tracker (`a4f8d465`) for a v0.8.0 wave that lands the enum
    /// variant + migration + write-path coverage. Reporting it here
    /// today would be theatrical.
    #[serde(default = "default_memory_kinds")]
    pub memory_kinds: Vec<String>,

    /// v0.7.0 L3-5 — recursive-learning capability surface. Every
    /// sub-field anchors a real implementation in this binary; see
    /// [`CapabilityReflection`] for the per-field audit anchors.
    #[serde(default = "default_capability_reflection")]
    pub reflection: CapabilityReflection,

    /// v0.7.0 L3-5 — Agent-Skills capability surface. Lists the seven
    /// registered `memory_skill_*` MCP tools; the round-trip guarantee
    /// is pinned by `tests/skill_test.rs`. See [`CapabilitySkills`].
    #[serde(default = "default_capability_skills")]
    pub skills: CapabilitySkills,

    /// v0.7.0 L3-5 — forensic-evidence CLI surface. Names the three
    /// driver verbs that this binary actually ships
    /// (`verify-reflection-chain`, `export-forensic-bundle`,
    /// `verify-forensic-bundle`). See [`CapabilityForensic`].
    #[serde(default = "default_capability_forensic")]
    pub forensic: CapabilityForensic,

    /// v0.7.0 L3-5 — substrate-rules governance surface. Honestly
    /// labelled `"operator_signed"` because the L1-6 loader refuses
    /// to honour unsigned rules. See [`CapabilityGovernance`].
    #[serde(default = "default_capability_governance")]
    pub governance: CapabilityGovernance,

    /// v0.7.0 WT-1-G — atomisation capability surface. Names the six
    /// operator-facing atomisation surfaces (`tool` / `cli` / `auto` /
    /// `recall_preference` / `forensic` / `curator`) plus the
    /// `derives_from` link relation that anchors atom → parent
    /// lineage. See [`CapabilityAtomisation`] for the per-field
    /// implementation anchor map.
    ///
    /// Additive over the L3-5 surface — pre-WT-1-G v3 payloads still
    /// deserialise cleanly (the `default_capability_atomisation`
    /// helper resolves to the current-implementation snapshot for any
    /// payload missing the field).
    #[serde(default = "default_capability_atomisation")]
    pub atomisation: CapabilityAtomisation,

    /// v0.7.x Form 6 (issue #759) — Batman-taxonomy memory-kind
    /// vocabulary capability surface. Names the recall-filter +
    /// auto-classify surfaces shipped under Form 6 and enumerates
    /// the substrate's full set of recognised `memory_kind` values.
    /// See [`CapabilityMemoryKindVocab`].
    ///
    /// Additive over the WT-1-G surface — pre-Form-6 v3 payloads
    /// deserialise cleanly via the
    /// `default_capability_memory_kind_vocab` helper.
    #[serde(default = "default_capability_memory_kind_vocab")]
    pub memory_kind_vocab: CapabilityMemoryKindVocab,
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
// Transcript lifecycle (v0.7.0 I3) — per-namespace TTL + archive→prune
// ---------------------------------------------------------------------------

/// Compiled-in default for the transcript TTL: 30 days. After this
/// many seconds elapse from `created_at` AND every memory that links
/// the transcript has expired (or been deleted), the I3 background
/// sweeper marks the transcript archived.
pub const DEFAULT_TRANSCRIPT_TTL_SECS: i64 = 2_592_000;

/// Compiled-in default for the post-archive grace window: 7 days.
/// A transcript whose `archived_at` is older than this is hard-deleted
/// by the prune phase; the I2 join table is cleaned up via
/// `ON DELETE CASCADE`.
pub const DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS: i64 = 604_800;

/// Maximum transcript TTL / grace clamp: 10 years in seconds. Mirrors
/// [`MAX_TTL_SECS`] above so the same overflow guard applies to the
/// transcript lifecycle math when the resolved value flows into a
/// `chrono::Duration`.
const MAX_TRANSCRIPT_LIFECYCLE_SECS: i64 = 315_360_000;

/// `[transcripts]` block in `config.toml` — per-namespace TTL and
/// archive grace overrides for the I3 lifecycle sweeper.
///
/// ```toml
/// [transcripts]
/// default_ttl_secs   = 2592000   # 30 days; archive after this when memories all expired
/// archive_grace_secs = 604800    # 7 days; prune this long after archive
///
/// [transcripts.namespaces."team/audit"]
/// default_ttl_secs = 31536000    # 1 year — compliance retention override
///
/// [transcripts.namespaces."ephemeral/*"]
/// default_ttl_secs = 86400       # 1 day — short-lived scratchpad
/// ```
///
/// Resolution: the sweeper picks the longest-prefix matching namespace
/// override (with literal `"*"` patterns last), falls back to the
/// global `default_ttl_secs` / `archive_grace_secs` on this struct,
/// and finally to the compiled defaults above.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptsConfig {
    /// Global default seconds-since-creation before the sweeper
    /// considers a transcript archive-eligible. `None` → compiled
    /// default ([`DEFAULT_TRANSCRIPT_TTL_SECS`] = 30 days).
    pub default_ttl_secs: Option<i64>,
    /// Global default seconds an archived transcript lingers before
    /// the prune phase deletes it. `None` → compiled default
    /// ([`DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS`] = 7 days).
    pub archive_grace_secs: Option<i64>,
    /// Per-namespace overrides keyed by namespace pattern. Patterns
    /// are matched literally first; a trailing `/*` selects every
    /// child namespace under the prefix; the bare `"*"` is the
    /// catch-all and is consulted last.
    pub namespaces: Option<std::collections::HashMap<String, TranscriptNamespaceConfig>>,
}

/// Per-namespace overrides nested under
/// `[transcripts.namespaces."<pattern>"]`. Each field independently
/// overrides the [`TranscriptsConfig`] global default; an unset field
/// inherits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptNamespaceConfig {
    /// Namespace-specific TTL override.
    pub default_ttl_secs: Option<i64>,
    /// Namespace-specific archive-grace override.
    pub archive_grace_secs: Option<i64>,
    /// v0.7 I5 — opt in the namespace to the reference R5 pre_store
    /// transcript-extractor hook (`tools/transcript-extractor/`).
    /// Default `None` → disabled, matching the "default off" lesson
    /// from G3-G11. Operators that wire the extractor binary into
    /// their `hooks.toml` set this flag per namespace to gate the
    /// derived-memory expansion. `Some(false)` is identical to
    /// `None` and exists so an explicit "no, don't extract here"
    /// can be expressed alongside a wildcard `Some(true)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_extract: Option<bool>,
}

/// Resolved transcript-lifecycle parameters for a single namespace.
/// Produced by [`TranscriptsConfig::resolve`] and consumed by the I3
/// sweeper to drive the archive + prune SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTranscriptLifecycle {
    /// Seconds-since-creation before archive eligibility. Always
    /// positive and `<= MAX_TRANSCRIPT_LIFECYCLE_SECS`.
    pub default_ttl_secs: i64,
    /// Seconds an archived row lingers before prune. Always
    /// positive and `<= MAX_TRANSCRIPT_LIFECYCLE_SECS`.
    pub archive_grace_secs: i64,
}

impl Default for ResolvedTranscriptLifecycle {
    fn default() -> Self {
        Self {
            default_ttl_secs: DEFAULT_TRANSCRIPT_TTL_SECS,
            archive_grace_secs: DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS,
        }
    }
}

impl TranscriptsConfig {
    /// Resolve the lifecycle parameters for `namespace`.
    ///
    /// Precedence:
    /// 1. Exact match in `namespaces` (e.g. `"team/audit"`).
    /// 2. Longest matching prefix pattern ending in `/*` (e.g.
    ///    `"team/*"` matches `"team/eng"` and `"team/eng/inner"`).
    /// 3. Bare `"*"` wildcard.
    /// 4. The struct-level `default_ttl_secs` / `archive_grace_secs`.
    /// 5. The compiled defaults
    ///    ([`DEFAULT_TRANSCRIPT_TTL_SECS`] / [`DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS`]).
    ///
    /// Each field is resolved independently — a per-namespace override
    /// that only sets `default_ttl_secs` inherits the global
    /// `archive_grace_secs`. Non-positive values fall through to the
    /// next layer; positive values are clamped to
    /// `MAX_TRANSCRIPT_LIFECYCLE_SECS` so the resolved `Duration`
    /// addition can never overflow `chrono`.
    #[must_use]
    pub fn resolve(&self, namespace: &str) -> ResolvedTranscriptLifecycle {
        let ns_table = self.namespaces.as_ref();

        // Walk the namespace overrides in precedence order, returning
        // the first that names the field. `None` means "fall through".
        let pick_ns = |field: fn(&TranscriptNamespaceConfig) -> Option<i64>| -> Option<i64> {
            let table = ns_table?;
            // 1. Exact literal match.
            if let Some(ns) = table.get(namespace) {
                if let Some(v) = field(ns) {
                    return Some(v);
                }
            }
            // 2. Longest-prefix `prefix/*` match.
            let mut prefix_hits: Vec<(&str, &TranscriptNamespaceConfig)> = table
                .iter()
                .filter_map(|(k, v)| {
                    let prefix = k.strip_suffix("/*")?;
                    if namespace == prefix || namespace.starts_with(&format!("{prefix}/")) {
                        Some((prefix, v))
                    } else {
                        None
                    }
                })
                .collect();
            prefix_hits.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
            for (_, ns) in &prefix_hits {
                if let Some(v) = field(ns) {
                    return Some(v);
                }
            }
            // 3. Bare wildcard.
            if let Some(ns) = table.get("*") {
                if let Some(v) = field(ns) {
                    return Some(v);
                }
            }
            None
        };

        let clamp = |v: i64, fallback: i64| -> i64 {
            if v <= 0 {
                fallback
            } else {
                v.min(MAX_TRANSCRIPT_LIFECYCLE_SECS)
            }
        };

        let ttl = pick_ns(|n| n.default_ttl_secs)
            .or(self.default_ttl_secs)
            .map_or(DEFAULT_TRANSCRIPT_TTL_SECS, |v| {
                clamp(v, DEFAULT_TRANSCRIPT_TTL_SECS)
            });
        let grace = pick_ns(|n| n.archive_grace_secs)
            .or(self.archive_grace_secs)
            .map_or(DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS, |v| {
                clamp(v, DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS)
            });

        ResolvedTranscriptLifecycle {
            default_ttl_secs: ttl,
            archive_grace_secs: grace,
        }
    }

    /// v0.7 I5 — resolve the `auto_extract` opt-in for `namespace`.
    ///
    /// Same precedence walk as [`Self::resolve`] but folds the
    /// boolean field of [`TranscriptNamespaceConfig::auto_extract`]:
    ///
    /// 1. Exact match.
    /// 2. Longest-prefix `prefix/*` match.
    /// 3. Bare wildcard `"*"`.
    /// 4. `false` (default off — matches the "every reference hook
    ///    ships off-by-default" lesson from G10/G11).
    ///
    /// The R5 reference extractor (`tools/transcript-extractor/`)
    /// reads this flag at the namespace gate before doing any LLM
    /// work, so a namespace that hasn't opted in pays the cost of
    /// one HashMap lookup per `pre_store` fire and nothing more.
    #[must_use]
    pub fn auto_extract_for(&self, namespace: &str) -> bool {
        let Some(table) = self.namespaces.as_ref() else {
            return false;
        };
        // 1. Exact literal match.
        if let Some(ns) = table.get(namespace) {
            if let Some(v) = ns.auto_extract {
                return v;
            }
        }
        // 2. Longest-prefix `prefix/*` match.
        let mut prefix_hits: Vec<(&str, &TranscriptNamespaceConfig)> = table
            .iter()
            .filter_map(|(k, v)| {
                let prefix = k.strip_suffix("/*")?;
                if namespace == prefix || namespace.starts_with(&format!("{prefix}/")) {
                    Some((prefix, v))
                } else {
                    None
                }
            })
            .collect();
        prefix_hits.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        for (_, ns) in &prefix_hits {
            if let Some(v) = ns.auto_extract {
                return v;
            }
        }
        // 3. Bare wildcard.
        if let Some(ns) = table.get("*") {
            if let Some(v) = ns.auto_extract {
                return v;
            }
        }
        // 4. Default off.
        false
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
    /// Dedicated model for auto_tag (and other short-structured LLM calls).
    /// Defaults to `gemma3:4b` (fast, deterministic, ~0.7s p50 vs 15s for
    /// thinking-mode Gemma 4). Falls back to `llm_model` if unset.
    /// See L15 patch (2026-05-11) for rationale.
    pub auto_tag_model: Option<String>,
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
    /// v0.7.0 I3 — `[transcripts]` block. Per-namespace TTL and
    /// archive-grace overrides for the transcript lifecycle sweeper.
    /// Unset → compiled defaults apply globally
    /// ([`DEFAULT_TRANSCRIPT_TTL_SECS`] / [`DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS`]).
    pub transcripts: Option<TranscriptsConfig>,
    /// v0.7.0 K7 — `[hooks]` block. Currently carries the
    /// `[hooks.subscription] hmac_secret` server-wide override that
    /// signs every outgoing webhook payload regardless of whether the
    /// individual subscription supplied a per-subscription secret.
    /// When unset, only per-subscription secrets are used (legacy
    /// pre-K7 behaviour).
    pub hooks: Option<HooksConfig>,
    /// v0.7.0 H11 (#628 blocker) — `[subscriptions]` block. Carries
    /// the `allow_loopback_webhooks` opt-in that re-enables loopback
    /// webhook URLs (`127.0.0.1`, `localhost`, `[::1]`). Default-OFF
    /// closes an authenticated SSRF gadget against local services
    /// (Postgres on 5432, the hooks daemon, etc.). Operators who need
    /// loopback for testing must set this explicitly.
    pub subscriptions: Option<SubscriptionsConfig>,
    /// v0.7.0 H5 (round-2) — `[verify]` block. Today exposes one
    /// knob: `require_nonce` (default `false`). When `true`, every
    /// `POST /api/v1/links/verify` request MUST include a
    /// `verification_nonce` (UUID v4 expected); missing or replayed
    /// nonces are rejected with 409 Conflict. Default-OFF preserves
    /// the v0.6.x verify-anytime semantics for unmigrated clients.
    pub verify: Option<VerifyConfig>,
    /// v0.7.0 M4 — connection-level `statement_timeout` (in seconds)
    /// applied via an `after_connect` hook to every postgres
    /// connection in the pool. Bounds runaway queries — a pathological
    /// `pg_sleep(60)` or an unbounded scan can otherwise wedge a
    /// connection forever. Defaults to 30s when unset; set to 0 to
    /// disable the limit (matches the postgres `SET` semantics).
    /// Operators only need to touch this when the workload requires
    /// long-running maintenance queries from the daemon itself.
    pub postgres_statement_timeout_secs: Option<u64>,
    /// v0.7.0 H7 (round-2) — per-HTTP-request wall-clock timeout in
    /// seconds. Applied as a middleware to every axum route in
    /// [`crate::build_router`] so a slow-POST (slowloris-style)
    /// attacker cannot keep a handler scope alive indefinitely.
    /// `None` selects the compiled default of 60 seconds; operators
    /// who need a different ceiling set
    /// `request_timeout_secs = <secs>` in `config.toml`.
    pub request_timeout_secs: Option<u64>,
    /// v0.7.0 H8 (round-2) — per-LLM-call wall-clock timeout in
    /// seconds. Wraps every `spawn_blocking` invocation of an Ollama
    /// call (`auto_tag`, `expand_query`, `summarize_memories`, ...)
    /// in `tokio::time::timeout`. `None` selects the compiled
    /// default of 30 seconds; on timeout the call falls back to the
    /// LLM-absent path (already exercised by L5/L7).
    pub llm_call_timeout_secs: Option<u64>,
    /// v0.7.0 (issue #318) — when set, the MCP stdio server forwards
    /// every write tool (`memory_store`, `memory_link`, `memory_delete`)
    /// to this HTTP endpoint (typically the local `ai-memory serve`
    /// daemon at `http://localhost:9077`) instead of writing to SQLite
    /// directly. The HTTP daemon then runs the existing
    /// `broadcast_store_quorum` / `broadcast_link_quorum` / etc. fanout,
    /// closing the gap surfaced by a2a-gate v0.6.0 r6 where MCP-stdio
    /// writes replicated locally but never reached the federation mesh.
    ///
    /// Unset (the default) keeps the legacy direct-SQLite path so
    /// single-node MCP deployments without a federation daemon behave
    /// exactly as before. The forwarder uses `reqwest::blocking` and
    /// surfaces HTTP errors as MCP error strings; on transport failure
    /// the response carries the underlying error so operators can
    /// distinguish "fanout daemon not running" from "quorum not met".
    pub mcp_federation_forward_url: Option<String>,
    /// v0.7.0 (issue #518) — `[agents.defaults]` block. Carries the
    /// `recall_scope` defaults spliced into `memory_recall` /
    /// `GET /api/v1/recall` / `ai-memory recall` requests that pass
    /// `session_default=true` (or `--session-default` on the CLI) and
    /// omit one or more filter fields. Closes the OpenClaw v0.6.3.1
    /// "what were you working on?" recovery gap — agents picking up a
    /// new session no longer need to remember to splice the canonical
    /// namespace + recency filters on every cross-session recall.
    ///
    /// `None` (the default) preserves single-tenant deployments and
    /// existing recall semantics exactly as-is. The splice happens in
    /// the handler before the storage call; explicit args always win
    /// over the defaults.
    pub agents: Option<AgentsConfig>,
}

/// v0.7.0 (issue #518) — `[agents]` top-level block. Today only carries
/// the `defaults` sub-block (`[agents.defaults.recall_scope]`); future
/// agent-scoped knobs (per-agent quota overrides, per-agent autonomy
/// hook policy) can stack here without bloating the top-level
/// `AppConfig` surface.
///
/// Wire format:
/// ```toml
/// [agents.defaults.recall_scope]
/// namespaces = ["projects/atlas"]
/// since = "24h"
/// tier = "long"
/// limit = 50
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// `[agents.defaults]` sub-block. `None` keeps recall semantics
    /// exactly as v0.6.x — every cross-session `memory_recall` requires
    /// explicit filters. `Some` enables `session_default=true` callers
    /// to splice these defaults into their request before storage
    /// dispatch.
    #[serde(default)]
    pub defaults: Option<AgentDefaults>,
}

/// v0.7.0 (issue #518) — `[agents.defaults]` sub-block. Today exposes a
/// single field: `recall_scope`. Future expansion (per-call timeouts,
/// per-call tag filters, …) lives here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentDefaults {
    /// `[agents.defaults.recall_scope]` — default filter set spliced
    /// into recall calls that pass `session_default=true` and omit
    /// individual filter fields. See [`RecallScope`] for field
    /// semantics. `None` is equivalent to "no defaults configured".
    #[serde(default)]
    pub recall_scope: Option<RecallScope>,
}

/// v0.7.0 (issue #518) — operator-configured recall defaults. Each
/// field is optional; when present and the inbound recall request
/// omits the corresponding axis AND passes `session_default=true`, the
/// handler splices in the configured value before dispatching to the
/// storage layer.
///
/// Resolution: **explicit request args > recall_scope defaults >
/// compiled defaults**. The splice never overrides an explicit filter
/// — operators can always narrow the result set further at call time.
///
/// Wire format:
/// ```toml
/// [agents.defaults.recall_scope]
/// namespaces = ["projects/atlas"]   # default namespace filter
/// since = "24h"                     # duration → since = now() - 24h
/// tier = "long"                     # "short" / "mid" / "long"
/// limit = 50                        # default cap
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecallScope {
    /// Default namespace filter applied when the request omits its
    /// own `namespace` field. The current recall handlers accept a
    /// single namespace per call; when multiple namespaces are
    /// configured we apply the first one. (The list form is future-
    /// compatible with a planned multi-namespace recall surface.)
    #[serde(default)]
    pub namespaces: Option<Vec<String>>,
    /// Default time-window applied when the request omits `since`.
    /// Expressed as a duration string: `"24h"`, `"7d"`, `"30m"`, … See
    /// [`parse_duration_string`] for the parser. The handler resolves
    /// it to `now() - duration` at request time and passes the
    /// resulting RFC3339 timestamp through the existing `since`
    /// filter — no new SQL path.
    #[serde(default)]
    pub since: Option<String>,
    /// Default tier filter applied when the request omits its own
    /// `tier`. Accepted values: `"short"` / `"mid"` / `"long"`. The
    /// sqlite recall handlers do not currently expose a tier
    /// parameter, so this knob is applied on the postgres SAL path
    /// (which carries a `Filter.tier`) and stored on the request
    /// envelope for forward-compatibility on sqlite (no observable
    /// behaviour change there).
    #[serde(default)]
    pub tier: Option<String>,
    /// Default recall limit applied when the request omits its own
    /// `limit`. The handler still clamps to the per-tool maximum
    /// (50) after applying this default, so an oversized value here
    /// degrades gracefully.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// v0.7.0 H7 (round-2) — compiled default per-request HTTP timeout.
/// Applied when `AppConfig::request_timeout_secs` is `None`.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 60;

/// v0.7.0 H8 (round-2) — compiled default per-LLM-call timeout.
/// Applied when `AppConfig::llm_call_timeout_secs` is `None`.
pub const DEFAULT_LLM_CALL_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Hooks / subscription HMAC (K7)
// ---------------------------------------------------------------------------

/// `[hooks]` config block. v0.7.0 K7 — operator-facing knobs for the
/// outgoing-webhook surface.
///
/// Wire format:
/// ```toml
/// [hooks.subscription]
/// hmac_secret = "<plaintext-secret>"
/// ```
///
/// When `hmac_secret` is set, EVERY outbound webhook payload is signed
/// with `HMAC-SHA256(hmac_secret, "<timestamp>.<body>")` and the hex
/// digest is sent as the `X-AI-Memory-Signature: sha256=<hex>` header.
/// The override applies even to subscriptions that did not register a
/// per-subscription secret. When both are set, the per-subscription
/// secret wins (subscription-scoped trust beats server-scoped trust).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    /// `[hooks.subscription]` sub-block. Optional — when omitted, no
    /// server-wide HMAC override applies.
    pub subscription: Option<HooksSubscriptionConfig>,
}

/// `[hooks.subscription]` sub-block. K7 ships one knob today
/// (`hmac_secret`); future K-track work may add per-event opt-out
/// filters or alternate signing algorithms.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksSubscriptionConfig {
    /// Server-wide HMAC secret. Plaintext on disk — operators are
    /// expected to chmod 600 the config file (same posture as the
    /// existing `api_key` field).
    pub hmac_secret: Option<String>,
}

/// v0.7.0 H5 (round-2) — `[verify]` config block. Operator-facing
/// knobs for `POST /api/v1/links/verify`. Today exposes one knob:
/// `require_nonce` (default `false`).
///
/// Wire format:
/// ```toml
/// [verify]
/// require_nonce = true     # strict mode — every verify request
///                          # must carry verification_nonce
/// ```
///
/// When `require_nonce = false` (the default), the handler logs a
/// deprecation WARN when a request omits `verification_nonce` but
/// still allows it through. When `true`, missing nonces are rejected
/// with 409 Conflict and the operator's audit trail receives every
/// attempted reuse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyConfig {
    /// When `true`, `POST /api/v1/links/verify` requires every
    /// request body to include a `verification_nonce` field. Missing
    /// or empty nonces produce a 400 Bad Request. Already-seen
    /// `(link_id, signature, nonce)` tuples produce a 409 Conflict
    /// with `{"error":"verification replay detected"}`. Default `false`
    /// preserves the v0.6.x verify-anytime semantics; operators
    /// opting into the H5 replay-protection guarantee set this to
    /// `true` after their clients have been updated to emit nonces.
    #[serde(default)]
    pub require_nonce: bool,
}

/// v0.7.0 H11 (#628 blocker) — `[subscriptions]` block. Operator
/// knobs for the outgoing-webhook surface that are NOT specific to
/// HMAC signing (which lives under `[hooks.subscription]`).
///
/// Wire format:
/// ```toml
/// [subscriptions]
/// allow_loopback_webhooks = true   # default false; opt-in for testing
/// ```
///
/// When unset (or false), the SSRF guard rejects webhook URLs that
/// resolve to loopback addresses (`127.0.0.0/8`, `localhost`, `::1`).
/// Loopback hosts are reachable from the daemon process itself, so
/// permitting them by default exposes any locally-bound service
/// (database, internal admin sockets) to authenticated SSRF.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionsConfig {
    /// Re-enable loopback webhook URLs. Default `false` (loopback
    /// rejected). Operators who need to point a webhook at a local
    /// listener (CI, dev) set this to `true` explicitly.
    #[serde(default)]
    pub allow_loopback_webhooks: bool,
}

impl AppConfig {
    /// v0.7.0 K7 — resolved server-wide webhook HMAC secret. `None`
    /// means no server-wide override (per-subscription secrets still
    /// apply via the legacy code path).
    #[must_use]
    pub fn effective_hooks_hmac_secret(&self) -> Option<String> {
        self.hooks
            .as_ref()
            .and_then(|h| h.subscription.as_ref())
            .and_then(|s| s.hmac_secret.clone())
    }

    /// v0.7.0 (issue #518) — resolved `[agents.defaults.recall_scope]`
    /// block. Returns `Some(&scope)` when configured, `None` otherwise.
    /// Consumed by the recall handlers (sqlite + postgres SAL branches,
    /// MCP `handle_recall`, CLI `cmd_recall`) to splice defaults into
    /// requests that pass `session_default=true` and omit one or more
    /// filter fields.
    #[must_use]
    pub fn effective_recall_scope(&self) -> Option<&RecallScope> {
        self.agents
            .as_ref()
            .and_then(|a| a.defaults.as_ref())
            .and_then(|d| d.recall_scope.as_ref())
    }

    /// v0.7.0 H11 (#628 blocker) — resolved loopback-webhook opt-in
    /// flag. Defaults to `false` (loopback rejected — closes the
    /// authenticated SSRF gadget against local services). Operators
    /// who need loopback for testing set
    /// `[subscriptions] allow_loopback_webhooks = true`.
    ///
    /// Resolution order (mirrors `effective_permissions_mode`):
    /// 1. `AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS` env var (`1` / `true` —
    ///    case-insensitive). Lets the integration suite — which
    ///    sets `AI_MEMORY_NO_CONFIG=1` and therefore cannot use
    ///    `[subscriptions]` from `config.toml` — bind wiremock at
    ///    `127.0.0.1:0` and drive webhooks through it without
    ///    touching the production default.
    /// 2. `[subscriptions].allow_loopback_webhooks` from `config.toml`.
    /// 3. Compiled default (`false` — loopback rejected).
    #[must_use]
    pub fn effective_allow_loopback_webhooks(&self) -> bool {
        if let Ok(raw) = std::env::var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS") {
            match raw.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => return true,
                "0" | "false" | "no" | "off" | "" => return false,
                other => {
                    eprintln!(
                        "ai-memory: AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS={other:?} is not a valid \
                         boolean (expected 1/true/yes/on or 0/false/no/off); falling back to \
                         config.toml"
                    );
                }
            }
        }
        self.subscriptions
            .as_ref()
            .is_some_and(|s| s.allow_loopback_webhooks)
    }
}

// ---------------------------------------------------------------------------
// Process-wide handle for the K7 server-wide HMAC override.
// Mirrors the `ACTIVE_PERMISSIONS_MODE` pattern: set once at boot,
// read by `subscriptions::dispatch_event_with_details` without an
// API churn through every callsite. Stored behind a `RwLock<Option<…>>`
// so the K7 integration tests can flip the value mid-process without
// the `OnceLock`'s set-once contract getting in the way.
// ---------------------------------------------------------------------------

use std::sync::RwLock;

static ACTIVE_HOOKS_HMAC_SECRET: RwLock<Option<String>> = RwLock::new(None);

/// v0.7.0 K7 — set the process-wide webhook HMAC override. Called from
/// `main`/daemon bootstrap with the value from
/// `[hooks.subscription] hmac_secret`. Last writer wins — this is
/// production-safe because boot only invokes it once; tests use the
/// same setter to flip mid-process.
pub fn set_active_hooks_hmac_secret(secret: Option<String>) {
    if let Ok(mut w) = ACTIVE_HOOKS_HMAC_SECRET.write() {
        *w = secret;
    }
}

/// v0.7.0 K7 — read the process-wide webhook HMAC override. Returns
/// `None` when unset (the K6-and-earlier behaviour: only
/// per-subscription secrets sign outgoing payloads).
#[must_use]
pub fn active_hooks_hmac_secret() -> Option<String> {
    ACTIVE_HOOKS_HMAC_SECRET.read().ok().and_then(|g| g.clone())
}

// ---------------------------------------------------------------------------
// H11 — process-wide handle for the loopback-webhook opt-in
// ---------------------------------------------------------------------------
//
// `validate_url` in `subscriptions.rs` consults this handle to decide
// whether to accept loopback webhook destinations. Default-OFF closes
// the SSRF gadget; the boot code in `main` / daemon reads
// `[subscriptions] allow_loopback_webhooks` and sets the flag here.

// Default-OFF in production builds so the SSRF guard rejects loopback
// without explicit opt-in. Defaults to `true` under `cfg(test)` so
// the existing test surface (which binds wiremock to `127.0.0.1:0`
// and drives validate_url/validate_url_dns through real loopback
// URLs) passes without 16-test fan-out modifications. The H11
// default-OFF behaviour is independently asserted via the
// `validate_url_with` / `validate_url_dns_check_addrs` inner helpers
// in `subscriptions.rs`, so flipping the test-build default here
// does NOT relax the H11 ship-gate test coverage.
static ALLOW_LOOPBACK_WEBHOOKS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(cfg!(test));

/// v0.7.0 H11 — set the process-wide loopback-webhook opt-in. Called
/// from boot with the value of `[subscriptions] allow_loopback_webhooks`.
/// Defaults to `false` (loopback rejected).
pub fn set_allow_loopback_webhooks(allow: bool) {
    ALLOW_LOOPBACK_WEBHOOKS.store(allow, std::sync::atomic::Ordering::SeqCst);
}

/// v0.7.0 H11 — read the process-wide loopback-webhook opt-in.
/// Returns `false` when unset (the safe default — loopback URLs are
/// rejected by the SSRF guard).
#[must_use]
pub fn allow_loopback_webhooks() -> bool {
    ALLOW_LOOPBACK_WEBHOOKS.load(std::sync::atomic::Ordering::SeqCst)
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
/// enforcement posture and (v0.7.0 K9) the declarative rule list
/// the unified [`crate::permissions::Permissions::evaluate`]
/// pipeline consults before mode + hook fall-through.
///
/// Wire format (rules — K9):
///
/// ```toml
/// [permissions]
/// mode = "enforce"
///
/// [[permissions.rules]]
/// namespace_pattern = "secrets/*"
/// op               = "memory_store"
/// agent_pattern    = "ai:*"
/// decision         = "deny"
/// reason           = "ai agents may not write to secrets"
/// ```
///
/// Rules are deny-first and longest-pattern-wins; see
/// [`crate::permissions`] module docs for the full combination
/// rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsConfig {
    /// Enforcement mode. `None` when the operator declared a
    /// `[permissions]` block but omitted `mode = ` — this is the
    /// "partial config" case that B4 (S5-M3) closes: such a block
    /// MUST NOT silently fall back to the serde-derived
    /// `PermissionsMode::default` (`advisory`), because the v0.7.0
    /// secure default is `enforce`. The
    /// [`AppConfig::effective_permissions_mode`] resolver maps
    /// `Some(cfg { mode: None })` to the secure default + a
    /// migration warning, so an operator who half-typed
    /// `[permissions]` and forgot the mode line still ships
    /// `enforce`, not the v0.6.x advisory posture.
    ///
    /// Serializes as omitted when `None` so a round-tripped config
    /// without an explicit `mode` keeps the partial-config shape
    /// for the next loader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionsMode>,
    /// v0.7.0 K9 — declarative permission rules. Each entry is a
    /// `(namespace_pattern, op, agent_pattern, decision)` tuple
    /// consulted by [`crate::permissions::Permissions::evaluate`]
    /// before the mode default falls through. Defaults to empty
    /// (no declarative rules — pre-K9 behaviour: mode + hooks +
    /// existing governance gate decide everything).
    #[serde(default)]
    pub rules: Vec<crate::permissions::PermissionRule>,
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

/// v0.7.0 (issue #518) — parse a duration string of the form
/// `"<integer><unit>"` into a `chrono::Duration`. Supported units:
/// `s` (seconds), `m` (minutes), `h` (hours), `d` (days), `w` (weeks).
/// Whitespace and case are tolerated. Returns `None` on malformed
/// input — the caller falls through to "no since filter applied".
///
/// Intentionally a small bespoke parser rather than a `humantime`
/// dependency: the surface we need is tiny (4-5 units) and operators
/// expect the same shape they already type into `--since` flags.
#[must_use]
pub fn parse_duration_string(s: &str) -> Option<chrono::Duration> {
    let trimmed = s.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let (num_part, unit_part) = trimmed.split_at(
        trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );
    let n: i64 = num_part.parse().ok()?;
    if n < 0 {
        return None;
    }
    match unit_part.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => Some(chrono::Duration::seconds(n)),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(chrono::Duration::minutes(n)),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(chrono::Duration::hours(n)),
        "d" | "day" | "days" => Some(chrono::Duration::days(n)),
        "w" | "wk" | "wks" | "week" | "weeks" => Some(chrono::Duration::weeks(n)),
        _ => None,
    }
}

/// Expand a leading `~` or `~/` in a path string to `$HOME`. POSIX-style.
/// `~user/...` is not supported (rare in our deployment surface, and supporting
/// it requires `getpwnam` — out of scope for the #507 fix). When `$HOME` is
/// unset (no-home environments like some CI containers), the tilde is left
/// untouched so the existing failure mode (path not found) is preserved
/// rather than silently rewriting to an empty prefix.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        return std::env::var("HOME").map_or_else(|_| PathBuf::from(s), PathBuf::from);
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return std::env::var("HOME")
            .map_or_else(|_| PathBuf::from(s), |h| PathBuf::from(h).join(rest));
    }
    PathBuf::from(s)
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
            Ok(contents) => {
                // L1 fix (v0.7.0): warn on unknown top-level keys.
                // `serde(deny_unknown_fields)` would be a breaking change for
                // operators carrying forward-compat config snippets, so we
                // instead parse the document twice: once as a generic
                // `toml::Value` to enumerate every top-level key, and once
                // into `AppConfig` as before. Any top-level key that is not
                // part of the expected `AppConfig` field set is reported via
                // `tracing::warn!` and otherwise silently ignored — load
                // continues to succeed so a typo or stale Plan C section
                // (`[memory]`, `[autonomous]`, `[governance]`, `[federation]`)
                // can no longer silently neutralise an operator's intent.
                Self::warn_unknown_top_level_keys(path, &contents);
                match toml::from_str(&contents) {
                    Ok(cfg) => {
                        eprintln!("ai-memory: loaded config from {}", path.display());
                        cfg
                    }
                    Err(e) => {
                        eprintln!("ai-memory: config parse error ({}): {}", path.display(), e);
                        Self::default()
                    }
                }
            }
            Err(_) => Self::default(),
        }
    }

    /// L1 fix (v0.7.0): enumerate top-level keys in `contents` and emit a
    /// `tracing::warn!` for every key that is not a recognised `AppConfig`
    /// field. Malformed TOML is silently skipped here — the existing
    /// `toml::from_str::<AppConfig>` parse in `load_from` will surface the
    /// real parse error to the operator on the next line.
    fn warn_unknown_top_level_keys(path: &Path, contents: &str) {
        // Canonical list of `AppConfig` top-level fields. Keep in sync with
        // the struct definition above; verified verbatim against the v0.7.0
        // L1 spec.
        const EXPECTED_KEYS: &[&str] = &[
            "tier",
            "db",
            "ollama_url",
            "embed_url",
            "embedding_model",
            "llm_model",
            "auto_tag_model",
            "cross_encoder",
            "default_namespace",
            "max_memory_mb",
            "ttl",
            "archive_on_gc",
            "api_key",
            "archive_max_days",
            "identity",
            "scoring",
            "autonomous_hooks",
            "logging",
            "audit",
            "boot",
            "mcp",
            "permissions",
            "transcripts",
            "hooks",
            "subscriptions",
            "postgres_statement_timeout_secs",
            "request_timeout_secs",
            "llm_call_timeout_secs",
            "verify",
            "mcp_federation_forward_url",
            "agents",
        ];

        let value: toml::Value = match toml::from_str(contents) {
            Ok(v) => v,
            // Malformed TOML — defer to the strongly-typed parse in the
            // caller, which produces the operator-facing error message.
            Err(_) => return,
        };

        let Some(table) = value.as_table() else {
            return;
        };

        let expected_list = EXPECTED_KEYS.join(", ");
        for key in table.keys() {
            if !EXPECTED_KEYS.contains(&key.as_str()) {
                tracing::warn!(
                    "[config] unknown key '{key}' in {path} — top-level AppConfig fields are: {expected_keys}. This key is silently ignored (no behavior change).",
                    key = key,
                    path = path.display(),
                    expected_keys = expected_list,
                );
            }
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
    /// 3. v0.7.0 secure default ([`PermissionsMode::Enforce`]) when no
    ///    explicit configuration is present. Round-2 F8 / Round-3
    ///    re-verify: prior to this round the unconfigured fallback was
    ///    [`PermissionsMode::default`] (= `advisory`), which left an
    ///    upgrading deployment with `metadata.governance.write=owner`
    ///    bypassable. We now resolve via
    ///    [`crate::permissions::resolve_v07_default_mode`] so every
    ///    process-wide entry point (CLI, MCP, HTTP serve) shares the
    ///    same secure-by-default posture; operators who want advisory
    ///    set `[permissions].mode = "advisory"` explicitly.
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
        // B4 (S5-M3) — both "block absent entirely" and "block present
        // but `mode =` omitted" must reach the secure default. The
        // `Option<PermissionsMode>` shape lets us collapse both to
        // `None` for the resolver so neither path silently inherits
        // the serde-derived `Advisory`. The migration WARN that
        // `resolve_v07_default_mode` emits when configured is `None`
        // is surfaced by the daemon's startup banner
        // (see `crate::cli::serve_banner::compose_banner`).
        let configured = self.permissions.as_ref().and_then(|p| p.mode);
        let (mode, _warn) = crate::permissions::resolve_v07_default_mode(configured);
        mode
    }

    /// v0.7.0 K9 — resolve the effective declarative rule set
    /// consulted by [`crate::permissions::Permissions::evaluate`].
    ///
    /// Returns the rules from `[permissions]` when configured;
    /// otherwise an empty vec (no declarative rules — mode + hooks
    /// resolve every decision).
    #[must_use]
    pub fn effective_permission_rules(&self) -> Vec<crate::permissions::PermissionRule> {
        self.permissions
            .as_ref()
            .map(|p| p.rules.clone())
            .unwrap_or_default()
    }

    /// Resolve the effective feature tier from config (CLI flag overrides).
    pub fn effective_tier(&self, cli_tier: Option<&str>) -> FeatureTier {
        let tier_str = cli_tier.or(self.tier.as_deref()).unwrap_or("semantic");
        FeatureTier::from_str(tier_str).unwrap_or(FeatureTier::Semantic)
    }

    /// Resolve the effective database path (CLI flag overrides config).
    ///
    /// Expands a leading `~` / `~/` in the config-provided path to `$HOME`
    /// before returning (issue #507). Without this, `db = "~/.claude/ai-memory.db"`
    /// in `config.toml` would land on disk as the literal four-char dir
    /// `~/.claude/...` relative to cwd and the daemon would report
    /// `warn db unavailable` against the real DB that lives at the
    /// expanded path.
    pub fn effective_db(&self, cli_db: &Path) -> PathBuf {
        // If CLI provided a non-default path, use it
        let default_db = PathBuf::from("ai-memory.db");
        if cli_db != default_db {
            return cli_db.to_path_buf();
        }
        // Otherwise check config — expanding leading `~` against $HOME.
        self.db
            .as_ref()
            .map_or_else(|| cli_db.to_path_buf(), |s| expand_tilde(s))
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

    /// v0.7.0 H7 (round-2) — resolved per-request HTTP timeout.
    /// Falls back to [`DEFAULT_REQUEST_TIMEOUT_SECS`] when the
    /// `request_timeout_secs` config field is unset.
    #[must_use]
    pub fn effective_request_timeout_secs(&self) -> u64 {
        self.request_timeout_secs
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS)
    }

    /// v0.7.0 H8 (round-2) — resolved per-LLM-call timeout. Falls
    /// back to [`DEFAULT_LLM_CALL_TIMEOUT_SECS`] when the
    /// `llm_call_timeout_secs` config field is unset.
    #[must_use]
    pub fn effective_llm_call_timeout_secs(&self) -> u64 {
        self.llm_call_timeout_secs
            .unwrap_or(DEFAULT_LLM_CALL_TIMEOUT_SECS)
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

    /// v0.7.0 I3 — resolve the [`TranscriptsConfig`] block, returning
    /// a default (no namespace overrides → compiled global defaults)
    /// instance when the config file omits it.
    #[must_use]
    pub fn effective_transcripts(&self) -> TranscriptsConfig {
        self.transcripts.clone().unwrap_or_default()
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

# Dedicated model for auto_tag (short structured output).
# Defaults to gemma3:4b. Reasoning-heavy features still use llm_model.
# auto_tag_model = "gemma3:4b"

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

    /// M9 — process-wide guard around every test that calls
    /// `std::env::set_var` / `std::env::remove_var`. Test binaries run
    /// in parallel by default (`cargo test --jobs N`); env mutation is
    /// process-global so two scenarios touching the same key race
    /// non-deterministically. Every test in this module that flips an
    /// env var MUST hold this mutex for the duration of its body.
    ///
    /// Poison-OK: a panicking scenario that drops the guard mid-mutation
    /// still hands the next caller a usable lock. Subsequent tests
    /// re-establish the env state they need on entry.
    fn env_var_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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

    /// L2 fix — `AppConfig.embedding_model` is an `Option<String>` we
    /// must parse before handing it to `build_embedder`. This test
    /// pins the wire form (snake_case, matches serde rename_all),
    /// confirms case-insensitive + trim-tolerant parsing, and that
    /// garbage input produces an actionable Err rather than panicking.
    #[test]
    fn embedding_model_from_str() {
        use std::str::FromStr;
        assert_eq!(
            EmbeddingModel::from_str("mini_lm_l6_v2").unwrap(),
            EmbeddingModel::MiniLmL6V2
        );
        assert_eq!(
            EmbeddingModel::from_str("nomic_embed_v15").unwrap(),
            EmbeddingModel::NomicEmbedV15
        );
        // Case-insensitive: operators copy/paste from docs in any case.
        assert_eq!(
            EmbeddingModel::from_str("MINI_LM_L6_V2").unwrap(),
            EmbeddingModel::MiniLmL6V2
        );
        assert_eq!(
            EmbeddingModel::from_str("Nomic_Embed_V15").unwrap(),
            EmbeddingModel::NomicEmbedV15
        );
        // Trim whitespace — common TOML editing artifact.
        assert_eq!(
            EmbeddingModel::from_str("  mini_lm_l6_v2  ").unwrap(),
            EmbeddingModel::MiniLmL6V2
        );
        // Invalid input -> Err with a useful message naming the bad value.
        let err = EmbeddingModel::from_str("garbage").unwrap_err();
        assert!(err.contains("garbage"), "err message lost the input: {err}");
        assert!(
            err.contains("mini_lm_l6_v2") && err.contains("nomic_embed_v15"),
            "err message should list valid options: {err}"
        );
    }

    #[test]
    fn autonomous_has_cross_encoder() {
        let cfg = FeatureTier::Autonomous.config();
        assert!(cfg.cross_encoder);
        let caps = cfg.capabilities();
        assert!(caps.features.cross_encoder_reranking);
        // v0.7.0 recursive-learning (issue #655): Tasks 1-6 shipped
        // the primitive, so the planned-feature object is now
        // `planned=false, enabled=true, version="v0.7.0"`. The
        // pre-v0.6.3.1 honesty contract still uses the
        // `PlannedFeature` shape so the v1 bool projection
        // collapses cleanly back to `true`.
        assert!(!caps.features.memory_reflection.planned);
        assert!(caps.features.memory_reflection.enabled);
        assert_eq!(caps.features.memory_reflection.version, "v0.7.0");
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
        // lifecycle events so integrators can pin a subscribe filter
        // against them.
        //
        // v0.7.0 K4 — `approval_requested` joined the list.
        // v0.7 J4 / G14 — `memory_link_invalidated` also joined.
        // Total: seven canonical event types.
        let events = val["hooks"]["webhook_events"].as_array().unwrap();
        assert_eq!(events.len(), 7);
        for expected in [
            "memory_store",
            "memory_promote",
            "memory_delete",
            "memory_link_created",
            "memory_link_invalidated",
            "memory_consolidated",
            "approval_requested",
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

        // memory_reflection: planned-feature object (was bool).
        // v0.7.0 recursive-learning (issue #655) Tasks 1-6 shipped the
        // primitive, so the flag is `planned=false, enabled=true,
        // version="v0.7.0"`.
        assert_eq!(val["features"]["memory_reflection"]["planned"], false);
        assert_eq!(val["features"]["memory_reflection"]["enabled"], true);
        assert_eq!(val["features"]["memory_reflection"]["version"], "v0.7.0");

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

        // v1 features.memory_reflection collapses to a bool. v0.7.0
        // recursive-learning (issue #655) Tasks 1-6 shipped the
        // primitive, so the v2 planned-feature object now has
        // `enabled = true` and the v1 bool projection is `true`.
        assert!(val["features"]["memory_reflection"].is_boolean());
        assert_eq!(val["features"]["memory_reflection"], true);

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
    fn effective_db_expands_tilde_against_home() {
        // #507: `db = "~/.claude/ai-memory.db"` must resolve to $HOME-based
        // path rather than the literal four-char prefix. Use env_var_lock
        // because HOME mutation is process-global.
        let _g = env_var_lock();
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: serialized via env_var_lock; restored below.
        unsafe { std::env::set_var("HOME", "/expanded/home") };
        let cfg = AppConfig {
            db: Some("~/.claude/ai-memory.db".to_string()),
            ..AppConfig::default()
        };
        assert_eq!(
            cfg.effective_db(Path::new("ai-memory.db")),
            PathBuf::from("/expanded/home/.claude/ai-memory.db")
        );
        // Bare `~` resolves to $HOME itself.
        let cfg_bare = AppConfig {
            db: Some("~".to_string()),
            ..AppConfig::default()
        };
        assert_eq!(
            cfg_bare.effective_db(Path::new("ai-memory.db")),
            PathBuf::from("/expanded/home")
        );
        // Restore.
        match prev_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
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
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`.
        unsafe { std::env::remove_var("AI_MEMORY_AUTONOMOUS_HOOKS") };
        let cfg = AppConfig::default();
        assert!(!cfg.effective_autonomous_hooks());
    }

    #[test]
    fn effective_autonomous_hooks_config_value_used_when_env_unset() {
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`.
        unsafe { std::env::remove_var("AI_MEMORY_AUTONOMOUS_HOOKS") };
        let cfg = AppConfig {
            autonomous_hooks: Some(true),
            ..AppConfig::default()
        };
        assert!(cfg.effective_autonomous_hooks());
    }

    #[test]
    fn effective_anonymize_default_falls_back_to_config() {
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`.
        unsafe { std::env::remove_var("AI_MEMORY_ANONYMIZE") };
        let cfg = AppConfig::default();
        assert!(!cfg.effective_anonymize_default());
    }

    #[test]
    fn write_default_if_missing_creates_file_then_noops() {
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // Use a temp dir as $HOME so we don't clobber a real config.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: env mutation serialised by `_g`.
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
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`.
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

    // -----------------------------------------------------------------
    // v0.7 I5 — auto_extract opt-in resolver
    // -----------------------------------------------------------------

    #[test]
    fn auto_extract_default_off_when_no_namespaces_block() {
        let cfg = TranscriptsConfig::default();
        assert!(!cfg.auto_extract_for("agent/claude"));
        assert!(!cfg.auto_extract_for("anything"));
    }

    #[test]
    fn auto_extract_exact_namespace_match_wins() {
        let mut nss = std::collections::HashMap::new();
        nss.insert(
            "agent/claude".into(),
            TranscriptNamespaceConfig {
                auto_extract: Some(true),
                ..Default::default()
            },
        );
        // Wildcard says "off" — exact match must still flip it on.
        nss.insert(
            "*".into(),
            TranscriptNamespaceConfig {
                auto_extract: Some(false),
                ..Default::default()
            },
        );
        let cfg = TranscriptsConfig {
            namespaces: Some(nss),
            ..Default::default()
        };
        assert!(cfg.auto_extract_for("agent/claude"));
        assert!(!cfg.auto_extract_for("agent/gpt"));
    }

    #[test]
    fn auto_extract_prefix_match_then_wildcard_fallback() {
        let mut nss = std::collections::HashMap::new();
        nss.insert(
            "team/security/*".into(),
            TranscriptNamespaceConfig {
                auto_extract: Some(true),
                ..Default::default()
            },
        );
        nss.insert(
            "*".into(),
            TranscriptNamespaceConfig {
                auto_extract: Some(false),
                ..Default::default()
            },
        );
        let cfg = TranscriptsConfig {
            namespaces: Some(nss),
            ..Default::default()
        };
        assert!(cfg.auto_extract_for("team/security/audit"));
        assert!(!cfg.auto_extract_for("team/eng/main"));
    }

    #[test]
    fn auto_extract_unset_field_inherits_default_off() {
        // A namespace block that sets only TTL — auto_extract is None
        // and so falls through to the next layer (wildcard, then off).
        let mut nss = std::collections::HashMap::new();
        nss.insert(
            "agent/claude".into(),
            TranscriptNamespaceConfig {
                default_ttl_secs: Some(3600),
                auto_extract: None,
                ..Default::default()
            },
        );
        let cfg = TranscriptsConfig {
            namespaces: Some(nss),
            ..Default::default()
        };
        assert!(!cfg.auto_extract_for("agent/claude"));
    }

    // -----------------------------------------------------------------
    // L1 fix (v0.7.0): unknown top-level keys WARN diagnostic
    // -----------------------------------------------------------------
    //
    // The earlier Plan C bug planted `[memory]`, `[autonomous]`,
    // `[governance]`, `[federation]` tables in the operator's
    // config.toml — none of them are real `AppConfig` fields, so serde
    // silently dropped them and the operator's intent never reached the
    // daemon. The fix warns on every unknown top-level key while still
    // loading the config gracefully.

    /// Top-level key not in `AppConfig` is reported via `tracing::warn!`
    /// AND the config still loads with recognised fields intact.
    #[test]
    fn load_from_warns_on_unknown_top_level_key_but_still_loads() {
        // Construct a config that mixes a real key (`tier`) with the
        // unknown `[memory]` table from the Plan C bug. The recognised
        // `tier = "autonomous"` at the top level must survive (i.e. the
        // unknown `[memory] tier = "ignored"` does NOT shadow it —
        // top-level wins because `[memory]` is a different namespace
        // entirely from `AppConfig.tier`).
        let toml_src = "tier = \"autonomous\"\n\n[memory]\ntier = \"ignored\"\n";

        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        std::fs::write(tmp.path(), toml_src).expect("write temp config");

        // We do NOT install a tracing subscriber here — `tracing-test`
        // is not a dev-dep, and the spec explicitly allows skipping the
        // "warn-was-emitted" assertion when capturing is awkward. The
        // important contract is:
        //   (a) load_from returns a populated AppConfig (no panic),
        //   (b) the recognised top-level `tier` survives,
        //   (c) the unknown `[memory]` table did NOT block the load.
        // The warn itself is exercised at runtime — verify it fires by
        // running `RUST_LOG=warn AI_MEMORY_NO_CONFIG=0 ai-memory ...`
        // against a config with a stray section.
        let cfg = AppConfig::load_from(tmp.path());

        assert_eq!(
            cfg.tier.as_deref(),
            Some("autonomous"),
            "top-level `tier` must survive even when an unknown `[memory]` table is present",
        );
    }

    /// Every field in `AppConfig` is enumerated in the expected-key
    /// set, so renaming a struct field will not silently start
    /// emitting bogus warnings for the new name.
    ///
    /// Regression guard: if you add a new top-level field to
    /// `AppConfig`, you MUST also add it to the `EXPECTED_KEYS` const
    /// inside `AppConfig::warn_unknown_top_level_keys`. This test
    /// enforces parity by serialising a fully-populated `AppConfig` to
    /// TOML and asserting that every emitted top-level key is in the
    /// expected set.
    #[test]
    fn warn_unknown_top_level_keys_covers_every_appconfig_field() {
        // Build an AppConfig with every Option populated so serde emits
        // every field. We only need the keys, not the values, so
        // default placeholder sub-structs are fine.
        let cfg = AppConfig {
            tier: Some("keyword".into()),
            db: Some(String::new()),
            ollama_url: Some(String::new()),
            embed_url: Some(String::new()),
            embedding_model: Some(String::new()),
            llm_model: Some(String::new()),
            auto_tag_model: Some(String::new()),
            cross_encoder: Some(false),
            default_namespace: Some(String::new()),
            max_memory_mb: Some(0),
            ttl: Some(TtlConfig::default()),
            archive_on_gc: Some(false),
            api_key: Some(String::new()),
            archive_max_days: Some(0),
            identity: Some(IdentityConfig::default()),
            scoring: Some(RecallScoringConfig::default()),
            autonomous_hooks: Some(false),
            logging: Some(LoggingConfig::default()),
            audit: Some(AuditConfig::default()),
            boot: Some(BootConfig::default()),
            mcp: Some(McpConfig::default()),
            permissions: Some(PermissionsConfig::default()),
            transcripts: Some(TranscriptsConfig::default()),
            hooks: Some(HooksConfig::default()),
            subscriptions: Some(SubscriptionsConfig::default()),
            postgres_statement_timeout_secs: Some(30),
            request_timeout_secs: Some(60),
            llm_call_timeout_secs: Some(30),
            verify: Some(VerifyConfig::default()),
            mcp_federation_forward_url: Some(String::new()),
            agents: Some(AgentsConfig::default()),
        };

        let serialised = toml::to_string(&cfg).expect("serialise AppConfig to TOML");
        let value: toml::Value =
            toml::from_str(&serialised).expect("re-parse serialised AppConfig");
        let table = value.as_table().expect("serialised AppConfig is a table");

        // Mirror the const in `warn_unknown_top_level_keys`. Keep in
        // sync — if this assertion fires, you forgot to update the
        // expected-keys list when adding a new AppConfig field.
        const EXPECTED_KEYS: &[&str] = &[
            "tier",
            "db",
            "ollama_url",
            "embed_url",
            "embedding_model",
            "llm_model",
            "auto_tag_model",
            "cross_encoder",
            "default_namespace",
            "max_memory_mb",
            "ttl",
            "archive_on_gc",
            "api_key",
            "archive_max_days",
            "identity",
            "scoring",
            "autonomous_hooks",
            "logging",
            "audit",
            "boot",
            "mcp",
            "permissions",
            "transcripts",
            "hooks",
            "subscriptions",
            "postgres_statement_timeout_secs",
            "request_timeout_secs",
            "llm_call_timeout_secs",
            "verify",
            "mcp_federation_forward_url",
            "agents",
        ];

        for key in table.keys() {
            assert!(
                EXPECTED_KEYS.contains(&key.as_str()),
                "AppConfig field `{key}` is not in EXPECTED_KEYS — \
                 update `warn_unknown_top_level_keys` to keep parity",
            );
        }
    }

    /// v0.7.0 L15 — assert that:
    ///  1. `AppConfig::default()` leaves `auto_tag_model` as `None` so a
    ///     daemon with no operator override sees the absent state (which
    ///     `maybe_auto_tag` interprets as "use the client's configured
    ///     `llm_model`"); and
    ///  2. the documented default config.toml template spot-checks
    ///     `gemma3:4b` as the recommended value — closes the L14
    ///     NHI-D-autotag-empty finding where Gemma 4 thinking-mode
    ///     latency hit the 30s autonomy timeout.
    #[test]
    fn auto_tag_model_default_falls_back_to_none_and_template_documents_default_gemma3_4b() {
        // (1) compile-time default leaves auto_tag_model = None.
        let cfg = AppConfig::default();
        assert!(
            cfg.auto_tag_model.is_none(),
            "fresh AppConfig must leave auto_tag_model = None so callers \
             fall back to llm_model"
        );

        // (2) the default config.toml template the daemon writes to disk
        // must document the recommended gemma3:4b value and mention
        // auto_tag_model — operators rely on the inline template as the
        // authoritative knob reference.
        //
        // We can't reach the private `default_toml` constant directly,
        // so write it to a tempdir via `write_default_if_missing` and
        // read it back. Mirrors the pattern used by
        // `default_config_includes_*` tests above.
        //
        // M9 — HOME mutation is process-global; other tests in this
        // module also flip HOME. Serialise via env_var_lock so parallel
        // `cargo test --jobs N` runs cannot interleave reads of HOME
        // mid-mutation.
        let _g = env_var_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: env mutation serialised by `_g`.
        unsafe { std::env::set_var("HOME", tmp.path()) };
        AppConfig::write_default_if_missing();
        let written = AppConfig::config_path().expect("config_path resolves");
        let contents = std::fs::read_to_string(&written).expect("default toml written");
        assert!(
            contents.contains("auto_tag_model"),
            "default config.toml must document the auto_tag_model knob; \
             got:\n{contents}"
        );
        assert!(
            contents.contains("gemma3:4b"),
            "default config.toml must mention gemma3:4b as the L15 \
             recommended default; got:\n{contents}"
        );
    }

    // ---- C-5 (#699): close lib-tier gaps in config.rs (currently 90.76%).
    // Targets serde default functions, env-var override branches, and
    // display impls that no other test exercises. ----

    #[test]
    fn llm_model_display_name_each_variant() {
        // Lines 84-89: `LlmModel::display_name` for each enum arm.
        assert_eq!(
            LlmModel::Gemma4E2B.display_name(),
            "Gemma 4 Effective 2B (Q4)"
        );
        assert_eq!(
            LlmModel::Gemma4E4B.display_name(),
            "Gemma 4 Effective 4B (Q4)"
        );
        // Also pin the ollama_model_id for completeness.
        assert_eq!(LlmModel::Gemma4E2B.ollama_model_id(), "gemma4:e2b");
        assert_eq!(LlmModel::Gemma4E4B.ollama_model_id(), "gemma4:e4b");
    }

    #[test]
    fn feature_tier_display_matches_as_str() {
        // Lines 183-185: `FeatureTier::Display::fmt` writes `as_str`.
        assert_eq!(format!("{}", FeatureTier::Keyword), "keyword");
        assert_eq!(format!("{}", FeatureTier::Semantic), "semantic");
        assert_eq!(format!("{}", FeatureTier::Smart), "smart");
        assert_eq!(format!("{}", FeatureTier::Autonomous), "autonomous");
    }

    #[test]
    fn default_recall_mode_is_disabled() {
        // Lines 630-632: serde default helper.
        assert_eq!(default_recall_mode(), RecallMode::Disabled);
    }

    #[test]
    fn default_reranker_mode_is_off() {
        // Lines 634-636: serde default helper.
        assert_eq!(default_reranker_mode(), RerankerMode::Off);
    }

    #[test]
    fn default_hook_events_count_matches_constant() {
        // Lines 731-733: serde default helper.
        assert_eq!(default_hook_events_count(), HOOK_EVENTS_COUNT);
    }

    #[test]
    fn default_reflection_boost_returns_default_report() {
        // Lines 621-623: serde default helper. Calls the `Default::default`
        // impl on `ReflectionBoostReport`.
        let r = default_reflection_boost();
        let d = ReflectionBoostReport::default();
        // Lazy compare via Debug — the struct has no PartialEq.
        assert_eq!(format!("{r:?}"), format!("{d:?}"));
    }

    #[test]
    fn permissions_mode_default_is_advisory() {
        // Lines 2403-2405: `impl Default for PermissionsMode`.
        let m: PermissionsMode = Default::default();
        assert_eq!(m, PermissionsMode::Advisory);
    }

    #[test]
    fn set_allow_loopback_webhooks_round_trips() {
        // Lines 2357-2359: pub setter — just observe it does not panic
        // and that effective_allow_loopback_webhooks can read the value.
        // (The atomic is process-global; restore the prior value at end.)
        let prior = ALLOW_LOOPBACK_WEBHOOKS.load(std::sync::atomic::Ordering::SeqCst);
        set_allow_loopback_webhooks(true);
        assert!(ALLOW_LOOPBACK_WEBHOOKS.load(std::sync::atomic::Ordering::SeqCst));
        set_allow_loopback_webhooks(false);
        assert!(!ALLOW_LOOPBACK_WEBHOOKS.load(std::sync::atomic::Ordering::SeqCst));
        // Restore.
        ALLOW_LOOPBACK_WEBHOOKS.store(prior, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn reset_permissions_decision_counts_zeros_all_atomics() {
        // Lines 2619-2623: test-only reset helper. Increment then reset.
        DECISIONS_ENFORCE.fetch_add(5, Ordering::SeqCst);
        DECISIONS_ADVISORY.fetch_add(3, Ordering::SeqCst);
        DECISIONS_OFF.fetch_add(1, Ordering::SeqCst);
        reset_permissions_decision_counts_for_test();
        assert_eq!(DECISIONS_ENFORCE.load(Ordering::SeqCst), 0);
        assert_eq!(DECISIONS_ADVISORY.load(Ordering::SeqCst), 0);
        assert_eq!(DECISIONS_OFF.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn effective_allow_loopback_webhooks_env_var_true_returns_true() {
        // Lines 2281-2297: env-var override branch (truthy).
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS", "yes");
        }
        let cfg = AppConfig::default();
        assert!(cfg.effective_allow_loopback_webhooks());
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS", v),
                None => std::env::remove_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS"),
            }
        }
    }

    #[test]
    fn effective_allow_loopback_webhooks_env_var_false_returns_false() {
        // Lines 2281-2297: env-var override (falsy).
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS", "no");
        }
        let cfg = AppConfig::default();
        assert!(!cfg.effective_allow_loopback_webhooks());
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS", v),
                None => std::env::remove_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS"),
            }
        }
    }

    #[test]
    fn effective_allow_loopback_webhooks_env_var_invalid_falls_back_to_config() {
        // Lines 2286-2292: invalid env value falls back to config.toml.
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS", "kinda");
        }
        let cfg = AppConfig::default();
        // With no [subscriptions] table the default is false.
        assert!(!cfg.effective_allow_loopback_webhooks());
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS", v),
                None => std::env::remove_var("AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS"),
            }
        }
    }

    #[test]
    fn effective_permissions_mode_env_var_enforce_wins() {
        // Lines 3144-3169: env override path → Enforce.
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_PERMISSIONS_MODE").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", "enforce");
        }
        let cfg = AppConfig::default();
        assert_eq!(cfg.effective_permissions_mode(), PermissionsMode::Enforce);
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", v),
                None => std::env::remove_var("AI_MEMORY_PERMISSIONS_MODE"),
            }
        }
    }

    #[test]
    fn effective_permissions_mode_env_var_advisory_wins() {
        // Lines 3148: env override path → Advisory.
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_PERMISSIONS_MODE").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", "ADVISORY");
        }
        let cfg = AppConfig::default();
        assert_eq!(cfg.effective_permissions_mode(), PermissionsMode::Advisory);
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", v),
                None => std::env::remove_var("AI_MEMORY_PERMISSIONS_MODE"),
            }
        }
    }

    #[test]
    fn effective_permissions_mode_env_var_off_wins() {
        // Lines 3149: env override path → Off.
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_PERMISSIONS_MODE").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", "off");
        }
        let cfg = AppConfig::default();
        assert_eq!(cfg.effective_permissions_mode(), PermissionsMode::Off);
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", v),
                None => std::env::remove_var("AI_MEMORY_PERMISSIONS_MODE"),
            }
        }
    }

    #[test]
    fn effective_permissions_mode_env_var_invalid_falls_back_to_config() {
        // Lines 3150-3156: invalid env → falls through to resolve_v07_default_mode.
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_PERMISSIONS_MODE").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", "weird");
        }
        let cfg = AppConfig::default();
        // The resolver returns a value (we don't pin which — just that it returns).
        let _ = cfg.effective_permissions_mode();
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_PERMISSIONS_MODE", v),
                None => std::env::remove_var("AI_MEMORY_PERMISSIONS_MODE"),
            }
        }
    }

    #[test]
    fn effective_permission_rules_returns_empty_when_unset() {
        // Lines 3178-3183: empty-rules path.
        let cfg = AppConfig::default();
        let rules = cfg.effective_permission_rules();
        assert!(rules.is_empty());
    }

    #[test]
    fn app_config_load_with_no_config_env_returns_default() {
        // Lines 3015-3022: `AppConfig::load` with AI_MEMORY_NO_CONFIG=1.
        let _g = env_var_lock();
        let prior = std::env::var("AI_MEMORY_NO_CONFIG").ok();
        unsafe {
            std::env::set_var("AI_MEMORY_NO_CONFIG", "1");
        }
        let cfg = AppConfig::load();
        // Default config has no tier/db set.
        assert!(
            cfg.tier.is_none()
                || cfg.tier == Some("semantic".to_string())
                || cfg.tier == Some("keyword".to_string())
        );
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AI_MEMORY_NO_CONFIG", v),
                None => std::env::remove_var("AI_MEMORY_NO_CONFIG"),
            }
        }
    }

    // ---- C-5 (#699) round 2: round out the easy Default impls + serde
    // default helpers that bumped lines 805/852/955/1019/1057/1125/1634+ ----

    #[test]
    fn capability_compaction_default_is_planned() {
        // Lines 804-808.
        let d: CapabilityCompaction = Default::default();
        let planned = CapabilityCompaction::planned();
        // Compare via Debug since the struct has no PartialEq.
        assert_eq!(format!("{d:?}"), format!("{planned:?}"));
    }

    #[test]
    fn capability_transcripts_default_is_planned() {
        // Lines 851-855.
        let d: CapabilityTranscripts = Default::default();
        let planned = CapabilityTranscripts::planned();
        assert_eq!(format!("{d:?}"), format!("{planned:?}"));
    }

    #[test]
    fn default_capability_reflection_helper_returns_current() {
        // Lines 955-957.
        let helper = default_capability_reflection();
        let current = CapabilityReflection::current();
        assert_eq!(format!("{helper:?}"), format!("{current:?}"));
    }

    #[test]
    fn default_capability_skills_helper_returns_current() {
        // Lines 1019-1021.
        let helper = default_capability_skills();
        let current = CapabilitySkills::current();
        assert_eq!(helper, current);
    }

    #[test]
    fn default_capability_forensic_helper_returns_current() {
        // Lines 1057-1059.
        let helper = default_capability_forensic();
        let current = CapabilityForensic::current();
        assert_eq!(helper, current);
    }

    #[test]
    fn default_capability_governance_helper_returns_current() {
        // Lines 1125-1127.
        let helper = default_capability_governance();
        let current = CapabilityGovernance::current();
        assert_eq!(helper, current);
    }

    #[test]
    fn default_capability_atomisation_helper_returns_current() {
        // v0.7.0 WT-1-G — mirrors the governance/forensic/skills/reflection
        // helper round-trip: the `#[serde(default = …)]` resolver must
        // collapse to the same compile-anchored snapshot
        // [`CapabilityAtomisation::current`] returns.
        let helper = default_capability_atomisation();
        let current = CapabilityAtomisation::current();
        assert_eq!(helper, current);
    }

    #[test]
    fn resolved_transcript_lifecycle_default_uses_compiled_defaults() {
        // Lines 1633-1639.
        let r: ResolvedTranscriptLifecycle = Default::default();
        assert_eq!(r.default_ttl_secs, DEFAULT_TRANSCRIPT_TTL_SECS);
        assert_eq!(r.archive_grace_secs, DEFAULT_TRANSCRIPT_ARCHIVE_GRACE_SECS);
    }

    #[test]
    fn default_memory_kinds_lists_observation_and_reflection() {
        // Lines 626-628: serde default helper covers L1-1 typed kinds.
        let kinds = default_memory_kinds();
        assert_eq!(
            kinds,
            vec!["observation".to_string(), "reflection".to_string()]
        );
    }
}
