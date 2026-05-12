// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::default_metadata;

/// Memory tier — mirrors human memory systems.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Short,
    Mid,
    Long,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Mid => "mid",
            Self::Long => "long",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "short" => Some(Self::Short),
            "mid" => Some(Self::Mid),
            "long" => Some(Self::Long),
            _ => None,
        }
    }

    /// Numeric rank for tier comparison: Short=0, Mid=1, Long=2.
    #[cfg(test)]
    pub fn rank(&self) -> u8 {
        match self {
            Self::Short => 0,
            Self::Mid => 1,
            Self::Long => 2,
        }
    }

    pub fn default_ttl_secs(&self) -> Option<i64> {
        match self {
            Self::Short => Some(6 * 3600),
            Self::Mid => Some(7 * 24 * 3600),
            Self::Long => None,
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub tier: Tier,
    pub namespace: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub priority: i32,
    /// 0.0-1.0 — how certain is this memory
    pub confidence: f64,
    /// Who/what created this: "user", "claude", "hook", "api", "import"
    pub source: String,
    pub access_count: i64,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default = "default_metadata")]
    pub metadata: Value,
    /// v0.7.0 Task 1/8 (recursive learning) — depth in the substrate-native
    /// reflection recursion tree. `0` for memories minted directly from a
    /// caller (or any pre-v0.7.0 row), positive for memories synthesised by
    /// the reflection pass over lower-depth peers. Operators can cap recursion
    /// depth at write time; readers can filter / sort by it.
    ///
    /// `#[serde(default)]` lets pre-v0.7.0 JSON payloads (and older federation
    /// peers) deserialize cleanly — missing → 0, which matches the SQL
    /// `DEFAULT 0` on the column added in schema v29 (SQLite) / v31 (Postgres).
    #[serde(default)]
    pub reflection_depth: i32,
}

impl Default for Memory {
    /// All-zero / empty defaults. Useful as a base for ad-hoc test fixtures
    /// — `Memory { id: ..., title: ..., ..Default::default() }` — and for
    /// `#[serde(default)]` deserialisation of partial JSON. Tier defaults to
    /// `Mid` to match the API-layer default in [`CreateMemory`].
    fn default() -> Self {
        Self {
            id: String::new(),
            tier: Tier::Mid,
            namespace: "global".to_string(),
            title: String::new(),
            content: String::new(),
            tags: Vec::new(),
            priority: 5,
            confidence: 1.0,
            source: "api".to_string(),
            access_count: 0,
            created_at: String::new(),
            updated_at: String::new(),
            last_accessed_at: None,
            expires_at: None,
            metadata: default_metadata(),
            reflection_depth: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateMemory {
    #[serde(default = "default_tier")]
    pub tier: Tier,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    #[serde(default = "default_metadata")]
    pub metadata: Value,
    /// Optional agent identifier. When unset, the server resolves a default
    /// via `crate::identity` (NHI-hardened precedence chain).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional visibility scope (Task 1.5). One of `VALID_SCOPES`. When
    /// unset, treated as `private` by the query layer.
    #[serde(default)]
    pub scope: Option<String>,
    /// v0.6.3.1 P2 (G6) — collision policy when (title, namespace) already
    /// exists. One of `error` | `merge` | `version`. When unset, the
    /// daemon defaults to `error` for HTTP callers (HTTP is not legacy
    /// like MCP v1; clients that want the legacy silent-merge contract
    /// must opt in explicitly).
    #[serde(default)]
    pub on_conflict: Option<String>,
    /// v0.7.0 (issue #519) — when `Some(true)`, run a proactive
    /// `detect_contradiction` LLM probe against same-namespace memories
    /// BEFORE returning 201, regardless of `autonomous_hooks`. When
    /// `Some(false)`, force-disable detection even if `autonomous_hooks`
    /// is on. When `None`, defer to `autonomous_hooks`.
    ///
    /// Surface: the 201 response body grows a `conflicts: [{...}]` array
    /// listing every same-namespace candidate the LLM flags as
    /// contradictory. Each entry carries the candidate id, title, and
    /// (when LLM produces one) a `suggested_merge` content string the
    /// caller can pass to a follow-up `memory_consolidate`.
    #[serde(default)]
    pub detect_conflicts: Option<bool>,
}

fn default_tier() -> Tier {
    Tier::Mid
}
fn default_namespace() -> String {
    "global".to_string()
}
fn default_priority() -> i32 {
    5
}
fn default_confidence() -> f64 {
    1.0
}
fn default_source() -> String {
    "api".to_string()
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemory {
    pub title: Option<String>,
    pub content: Option<String>,
    pub tier: Option<Tier>,
    pub namespace: Option<String>,
    pub tags: Option<Vec<String>>,
    pub priority: Option<i32>,
    pub confidence: Option<f64>,
    pub expires_at: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(default = "default_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub min_priority: Option<i32>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub tags: Option<String>, // comma-separated
    /// Filter by `metadata.agent_id` (exact match).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Task 1.5 visibility: the querying agent's namespace position.
    /// When set, results are filtered per `metadata.scope` rules.
    #[serde(default)]
    pub as_agent: Option<String>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_limit() -> Option<usize> {
    Some(20)
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(default = "default_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub min_priority: Option<i32>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    /// Filter by `metadata.agent_id` (exact match).
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RecallQuery {
    pub context: Option<String>,
    /// `query` alias for `context` — the cert harness (S79) uses
    /// `?query=…`. Both forms route to the same code path; `context`
    /// wins when both are supplied.
    #[serde(default)]
    pub query: Option<String>,
    /// `q` alias for `context`/`query` — matches the search-style API
    /// surface (`/api/v1/memories?q=…`) so callers can use the same
    /// query token field across both endpoints.
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_recall_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    /// Task 1.5 visibility filtering.
    #[serde(default)]
    pub as_agent: Option<String>,
    /// Task 1.11 — context-budget-aware recall. When set, return the
    /// top-scored memories whose cumulative estimated tokens fit within
    /// this budget.
    #[serde(default)]
    pub budget_tokens: Option<usize>,
    /// v0.7.0 (issue #518) — when `true`, splice defaults from
    /// `[agents.defaults.recall_scope]` in `config.toml` for any
    /// filter field not explicitly set on this request. Resolution:
    /// explicit args > recall_scope defaults > compiled defaults.
    /// Default `false` preserves v0.6.x recall semantics exactly.
    #[serde(default)]
    pub session_default: Option<bool>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_recall_limit() -> Option<usize> {
    Some(10)
}

#[derive(Debug, Deserialize)]
pub struct RecallBody {
    /// Recall context. Accepts either `context` (canonical), `query`
    /// (cert harness alias used by S79), or `q` (matches the
    /// search-style API surface). At least one must be present and
    /// non-empty.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_recall_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    /// Task 1.5 visibility filtering.
    #[serde(default)]
    pub as_agent: Option<String>,
    /// Task 1.11 — context-budget-aware recall.
    #[serde(default)]
    pub budget_tokens: Option<usize>,
    /// v0.7.0 (issue #518) — when `true`, splice defaults from
    /// `[agents.defaults.recall_scope]` in `config.toml` for any
    /// filter field not explicitly set on this request body.
    /// Resolution: explicit args > recall_scope defaults > compiled
    /// defaults. Default `false` preserves v0.6.x recall semantics.
    #[serde(default)]
    pub session_default: Option<bool>,
}

impl RecallBody {
    /// Resolve the recall query string from `context`, `query`, or `q`.
    /// Returns the trimmed value, or an empty string when all three are
    /// absent — the caller is expected to reject empty.
    #[must_use]
    pub fn resolved_query(&self) -> String {
        self.context
            .as_deref()
            .or(self.query.as_deref())
            .or(self.q.as_deref())
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

#[derive(Debug, Deserialize)]
pub struct ForgetQuery {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>, // FTS pattern
    #[serde(default)]
    pub tier: Option<Tier>,
}

/// v0.6.3.1 (P3): per-request observability for the recall pipeline.
///
/// Surfaces *which* recall path actually ran, *which* reranker was active,
/// the candidate pool sizes coming out of FTS and HNSW (before fusion), and
/// the blend weight applied to the semantic component. Always present in
/// `memory_recall` responses; older clients ignore unknown fields per the
/// JSON-RPC convention.
///
/// Closes G2/G8/G11 from the v0.6.3 audit by making every silent-degrade
/// path observable at request time. The capabilities surface (P1) reports
/// the same state at startup; this struct is the per-call mirror.
#[derive(Debug, Clone, Serialize)]
pub struct RecallMeta {
    /// Which recall path executed.
    /// - `"hybrid"` — embedder + FTS, blended (G11 happy path).
    /// - `"keyword_only"` — embedder unavailable or query-embed failed,
    ///   keyword-only recall served (G11 silent-degrade now visible).
    pub recall_mode: String,
    /// Which reranker scored the final ordering.
    /// - `"neural"` — BERT cross-encoder (autonomous tier, model loaded).
    /// - `"lexical"` — Jaccard/TF-IDF/bigram fallback (G8 silent-degrade
    ///   now visible).
    /// - `"none"` — reranking disabled at this tier.
    pub reranker_used: String,
    /// Candidate-pool sizes coming out of each retrieval stage *before*
    /// fusion. Useful for spotting empty-FTS or empty-HNSW degradations.
    pub candidate_counts: CandidateCounts,
    /// Semantic blend weight applied during fusion. `0.0` for
    /// `keyword_only` mode; otherwise the average semantic weight across
    /// the returned candidates (varies 0.50→0.15 with content length).
    pub blend_weight: f64,
}

/// v0.6.3.1 (P3): retrieval-stage candidate counts feeding `RecallMeta`.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateCounts {
    /// Number of candidates retrieved by FTS5 keyword scoring.
    pub fts: usize,
    /// Number of candidates retrieved by HNSW (or linear-scan fallback)
    /// semantic search. `0` in keyword-only mode.
    pub hnsw: usize,
}

/// v0.6.3.1 (P3): internal telemetry returned alongside recall results.
///
/// Plumbed from `db::recall_hybrid_with_telemetry` /
/// `db::recall_with_telemetry` up to `mcp::handle_recall`, which uses it
/// to populate `RecallMeta`. Not serialized — `RecallMeta` is the public
/// shape.
#[derive(Debug, Clone, Default)]
pub struct RecallTelemetry {
    /// Candidates returned by the FTS5 stage before fusion.
    pub fts_candidates: usize,
    /// Candidates returned by the HNSW (or linear-scan fallback) stage
    /// before fusion. `0` for keyword-only recall.
    pub hnsw_candidates: usize,
    /// Average semantic blend weight applied across the returned set.
    /// `0.0` for keyword-only recall.
    pub blend_weight_avg: f64,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub total: usize,
    pub by_tier: Vec<TierCount>,
    pub by_namespace: Vec<NamespaceCount>,
    pub expiring_soon: usize,
    pub links_count: usize,
    pub db_size_bytes: u64,
    /// v0.6.3.1 P2 (G4) — count of rows whose stored `embedding_dim`
    /// disagrees with the BLOB length (or whose column is missing while
    /// a BLOB exists). 0 on a fresh database; non-zero indicates legacy
    /// rows the operator should re-embed. Consumed by the P7 doctor.
    #[serde(default)]
    pub dim_violations: u64,
    /// v0.6.3.1 (P3, G2): cumulative HNSW oldest-eviction count since this
    /// process started. Non-zero indicates the in-memory vector index has
    /// hit its `MAX_ENTRIES` cap and silently dropped older embeddings —
    /// recall quality may have degraded for evicted ids. Process-local
    /// (not persisted) because the index itself is process-local.
    #[serde(default)]
    pub index_evictions_total: u64,
}

#[derive(Debug, Serialize)]
pub struct TierCount {
    pub tier: String,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct NamespaceCount {
    pub namespace: String,
    pub count: usize,
}
