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
    /// - `"lexical"` — operator opted for the lexical variant, or the
    ///   tier never asked for a neural cross-encoder.
    /// - `"degraded_lexical"` — v0.7.0 R3-S2 — a configured neural
    ///   cross-encoder failed to initialise or errored mid-flight and
    ///   the runtime fell back. Distinct from `"lexical"` so clients
    ///   can detect the silent downgrade *in band* (previously this
    ///   was only a `tracing::warn!` event, which the G8 closure
    ///   claim overstated as "fail loud").
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

// -----------------------------------------------------------------
// L0.7-2 Tier A — memory.rs unit coverage
// Covers serde defaults (default_tier/default_namespace/etc.), Tier
// ↔ string round-trips, Memory::default, Tier::default_ttl_secs,
// RecallBody::resolved_query precedence.
// -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_round_trips_strings() {
        for (s, v) in [
            ("short", Tier::Short),
            ("mid", Tier::Mid),
            ("long", Tier::Long),
        ] {
            assert_eq!(Tier::from_str(s), Some(v.clone()));
            assert_eq!(v.as_str(), s);
            assert_eq!(format!("{v}"), s);
        }
    }

    #[test]
    fn tier_from_str_returns_none_for_unknown() {
        assert_eq!(Tier::from_str("unknown"), None);
        assert_eq!(Tier::from_str(""), None);
        assert_eq!(Tier::from_str("SHORT"), None); // case-sensitive
    }

    #[test]
    fn tier_default_ttl_secs_short_is_six_hours() {
        assert_eq!(Tier::Short.default_ttl_secs(), Some(6 * 3600));
    }

    #[test]
    fn tier_default_ttl_secs_mid_is_seven_days() {
        assert_eq!(Tier::Mid.default_ttl_secs(), Some(7 * 24 * 3600));
    }

    #[test]
    fn tier_default_ttl_secs_long_is_none() {
        assert_eq!(Tier::Long.default_ttl_secs(), None);
    }

    #[test]
    fn tier_rank_orders_short_mid_long() {
        assert!(Tier::Short.rank() < Tier::Mid.rank());
        assert!(Tier::Mid.rank() < Tier::Long.rank());
    }

    #[test]
    fn tier_serializes_to_snake_case() {
        let v = serde_json::to_value(Tier::Short).unwrap();
        assert_eq!(v, serde_json::Value::String("short".to_string()));
        let v = serde_json::to_value(Tier::Mid).unwrap();
        assert_eq!(v, serde_json::Value::String("mid".to_string()));
        let v = serde_json::to_value(Tier::Long).unwrap();
        assert_eq!(v, serde_json::Value::String("long".to_string()));
    }

    #[test]
    fn memory_default_uses_mid_tier_and_global_namespace() {
        let m = Memory::default();
        assert_eq!(m.tier, Tier::Mid);
        assert_eq!(m.namespace, "global");
        assert_eq!(m.priority, 5);
        assert!((m.confidence - 1.0).abs() < f64::EPSILON);
        assert_eq!(m.source, "api");
        assert_eq!(m.access_count, 0);
        assert_eq!(m.reflection_depth, 0);
        assert!(m.last_accessed_at.is_none());
        assert!(m.expires_at.is_none());
    }

    #[test]
    fn memory_round_trips_through_serde_with_reflection_depth() {
        let mut m = Memory::default();
        m.id = "mem-1".to_string();
        m.title = "test".to_string();
        m.content = "body".to_string();
        m.created_at = "2026-01-01T00:00:00Z".to_string();
        m.updated_at = "2026-01-01T00:00:00Z".to_string();
        m.reflection_depth = 3;
        let s = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "mem-1");
        assert_eq!(back.reflection_depth, 3);
    }

    #[test]
    fn memory_deserialises_pre_v070_payload_without_reflection_depth() {
        // Pre-v0.7.0 payloads have no reflection_depth field. serde
        // default must populate it as 0.
        let json = serde_json::json!({
            "id": "old-mem",
            "tier": "mid",
            "namespace": "ns",
            "title": "t",
            "content": "c",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "api",
            "access_count": 0,
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "metadata": {},
        });
        let m: Memory = serde_json::from_value(json).unwrap();
        assert_eq!(m.reflection_depth, 0);
    }

    fn cm_minimal() -> serde_json::Value {
        serde_json::json!({
            "title": "t",
            "content": "c",
        })
    }

    #[test]
    fn create_memory_defaults_tier_to_mid() {
        // Lines 175-177: default_tier returns Tier::Mid via #[serde(default)].
        let cm: CreateMemory = serde_json::from_value(cm_minimal()).unwrap();
        assert_eq!(cm.tier, Tier::Mid);
    }

    #[test]
    fn create_memory_defaults_namespace_to_global() {
        // Lines 178-180.
        let cm: CreateMemory = serde_json::from_value(cm_minimal()).unwrap();
        assert_eq!(cm.namespace, "global");
    }

    #[test]
    fn create_memory_defaults_priority_to_5() {
        // Lines 181-183.
        let cm: CreateMemory = serde_json::from_value(cm_minimal()).unwrap();
        assert_eq!(cm.priority, 5);
    }

    #[test]
    fn create_memory_defaults_confidence_to_one() {
        // Lines 184-186.
        let cm: CreateMemory = serde_json::from_value(cm_minimal()).unwrap();
        assert!((cm.confidence - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn create_memory_defaults_source_to_api() {
        // Lines 187-189.
        let cm: CreateMemory = serde_json::from_value(cm_minimal()).unwrap();
        assert_eq!(cm.source, "api");
    }

    #[test]
    fn create_memory_defaults_metadata_to_empty_object() {
        let cm: CreateMemory = serde_json::from_value(cm_minimal()).unwrap();
        assert_eq!(cm.metadata, serde_json::json!({}));
    }

    #[test]
    fn recall_body_resolved_query_prefers_context() {
        let body: RecallBody = serde_json::from_value(serde_json::json!({
            "context": "c-value",
            "query": "q-value",
            "q": "qq-value",
        }))
        .unwrap();
        assert_eq!(body.resolved_query(), "c-value");
    }

    #[test]
    fn recall_body_resolved_query_falls_back_to_query_then_q() {
        let body: RecallBody =
            serde_json::from_value(serde_json::json!({"query": "q-value", "q": "qq"})).unwrap();
        assert_eq!(body.resolved_query(), "q-value");
        let body: RecallBody = serde_json::from_value(serde_json::json!({"q": "qq"})).unwrap();
        assert_eq!(body.resolved_query(), "qq");
    }

    #[test]
    fn recall_body_resolved_query_empty_when_all_absent() {
        let body: RecallBody = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(body.resolved_query(), "");
    }

    #[test]
    fn recall_body_resolved_query_trims_whitespace() {
        let body: RecallBody =
            serde_json::from_value(serde_json::json!({"context": "  spaced  "})).unwrap();
        assert_eq!(body.resolved_query(), "spaced");
    }

    #[test]
    fn search_query_defaults_limit_to_20() {
        // default_limit() returns Some(20)
        let q: SearchQuery = serde_json::from_value(serde_json::json!({"q": "x"})).unwrap();
        assert_eq!(q.limit, Some(20));
    }

    #[test]
    fn recall_query_defaults_limit_to_10() {
        // default_recall_limit() returns Some(10)
        let q: RecallQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(q.limit, Some(10));
    }

    #[test]
    fn list_query_defaults_limit_to_20() {
        let q: ListQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(q.limit, Some(20));
    }
}
