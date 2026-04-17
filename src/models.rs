// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLink {
    pub source_id: String,
    pub target_id: String,
    pub relation: String, // "related_to", "supersedes", "contradicts", "derived_from"
    pub created_at: String,
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
pub fn default_metadata() -> Value {
    Value::Object(serde_json::Map::new())
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
}

#[allow(clippy::unnecessary_wraps)]
fn default_recall_limit() -> Option<usize> {
    Some(10)
}

#[derive(Debug, Deserialize)]
pub struct RecallBody {
    pub context: String,
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
}

#[derive(Debug, Deserialize)]
pub struct LinkBody {
    pub source_id: String,
    pub target_id: String,
    #[serde(default = "default_relation")]
    pub relation: String,
}

fn default_relation() -> String {
    "related_to".to_string()
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

#[derive(Debug, Serialize)]
pub struct Stats {
    pub total: usize,
    pub by_tier: Vec<TierCount>,
    pub by_namespace: Vec<NamespaceCount>,
    pub expiring_soon: usize,
    pub links_count: usize,
    pub db_size_bytes: u64,
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

/// Namespace reserved for agent registrations (Task 1.3).
pub const AGENTS_NAMESPACE: &str = "_agents";

// ---------------------------------------------------------------------------
// Task 1.8 — Governance Metadata
// ---------------------------------------------------------------------------

/// Who is permitted to perform a governed action.
///
/// Stored inside a namespace standard's `metadata.governance` and consulted
/// by Task 1.9 (enforcement) + Task 1.10 (approver types). Task 1.8 only
/// defines the shape + validation — no runtime enforcement yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GovernanceLevel {
    /// Any caller may perform the action (no gate).
    Any,
    /// Caller must be a registered agent (see Task 1.3 `_agents` namespace).
    Registered,
    /// Only the memory's original `metadata.agent_id` owner may perform the action.
    Owner,
    /// Action requires explicit approval by an `ApproverType` (handled in 1.9 + 1.10).
    Approve,
}

impl GovernanceLevel {
    /// Human-readable tag used by logs and error messages.
    /// Consumed by Task 1.9 enforcement path.
    #[allow(dead_code)]
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Registered => "registered",
            Self::Owner => "owner",
            Self::Approve => "approve",
        }
    }
}

/// Who approves actions gated by [`GovernanceLevel::Approve`].
///
/// Serialized representation (externally-tagged, `snake_case`):
///
/// - [`Self::Human`] → `"human"`
/// - [`Self::Agent`] → `{"agent": "alice"}`
/// - [`Self::Consensus`] → `{"consensus": 3}`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApproverType {
    /// Human approval required (interactive or out-of-band).
    Human,
    /// Specific registered agent must approve, identified by `agent_id`.
    Agent(String),
    /// Consensus of N approvers (any mix of human/agent registrations).
    Consensus(u32),
}

impl ApproverType {
    /// Discriminator tag for logs / telemetry.
    /// Consumed by Task 1.10 approver-types path.
    #[allow(dead_code)]
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent(_) => "agent",
            Self::Consensus(_) => "consensus",
        }
    }
}

/// Governance policy attached to a namespace's standard memory
/// (stored in `metadata.governance`).
///
/// Default policy when a standard has no `metadata.governance`:
/// `{ write: Any, promote: Any, delete: Owner, approver: Human }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernancePolicy {
    pub write: GovernanceLevel,
    pub promote: GovernanceLevel,
    pub delete: GovernanceLevel,
    pub approver: ApproverType,
}

impl Default for GovernancePolicy {
    fn default() -> Self {
        Self {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
        }
    }
}

impl GovernancePolicy {
    /// Parse a policy out of a `metadata.governance` JSON value. Returns
    /// `None` when the field is missing/null. Parse errors propagate so
    /// callers can surface them to the user instead of silently defaulting.
    pub fn from_metadata(metadata: &Value) -> Option<Result<Self, serde_json::Error>> {
        let gov = metadata.get("governance")?;
        if gov.is_null() {
            return None;
        }
        Some(serde_json::from_value(gov.clone()))
    }
}

/// Closed set of visibility scopes stamped into `metadata.scope` (Task 1.5).
/// Controls which agents can see a memory via hierarchical namespace matching.
/// Memories without a `scope` field are treated as `private` by the query layer.
pub const VALID_SCOPES: &[&str] = &["private", "team", "unit", "org", "collective"];

/// Closed set of agent types. Extend carefully — values are persisted.
pub const VALID_AGENT_TYPES: &[&str] = &[
    "ai:claude-opus-4.6",
    "ai:claude-opus-4.7",
    "ai:codex-5.4",
    "ai:grok-4.2",
    "human",
    "system",
];

#[derive(Debug, Deserialize)]
pub struct RegisterAgentBody {
    pub agent_id: String,
    pub agent_type: String,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct AgentRegistration {
    pub agent_id: String,
    pub agent_type: String,
    pub capabilities: Vec<String>,
    pub registered_at: String,
    pub last_seen_at: String,
}

pub const MAX_CONTENT_SIZE: usize = 65_536;

/// Maximum number of path segments in a hierarchical namespace (Task 1.4).
/// `alphaone/engineering/platform/team/squad/pod/role/agent` = 8 levels.
pub const MAX_NAMESPACE_DEPTH: usize = 8;

/// Number of `/`-delimited segments in a namespace path.
///
/// Flat namespaces (`"global"`, `"ai-memory"`) return `1`. An empty string
/// returns `0`.
///
/// # Examples
/// ```
/// # use ai_memory::models::namespace_depth;
/// assert_eq!(namespace_depth("global"), 1);
/// assert_eq!(namespace_depth("alphaone/engineering"), 2);
/// assert_eq!(namespace_depth("alphaone/engineering/platform"), 3);
/// ```
#[must_use]
pub fn namespace_depth(ns: &str) -> usize {
    if ns.is_empty() {
        return 0;
    }
    ns.split('/').filter(|s| !s.is_empty()).count()
}

/// Parent of a hierarchical namespace, or `None` for flat / empty inputs.
///
/// Part of the Task 1.4 hierarchical-namespace API. Consumed by Tasks 1.5
/// (visibility rules), 1.6 (N-level inheritance), 1.7 (vertical promotion),
/// and 1.12 (hierarchy-aware recall).
#[allow(dead_code)]
///
/// Parent of `"a/b/c"` is `"a/b"`. Parent of `"flat"` is `None` (a flat
/// namespace has no parent). Parent of `""` is `None`.
///
/// # Examples
/// ```
/// # use ai_memory::models::namespace_parent;
/// assert_eq!(namespace_parent("alphaone/engineering/platform"), Some("alphaone/engineering".to_string()));
/// assert_eq!(namespace_parent("alphaone"), None);
/// assert_eq!(namespace_parent(""), None);
/// ```
#[must_use]
pub fn namespace_parent(ns: &str) -> Option<String> {
    ns.rsplit_once('/').map(|(parent, _)| parent.to_string())
}

/// Ancestors of a namespace, ordered most-specific-first (including the
/// namespace itself as the first element).
///
/// Part of the Task 1.4 hierarchical-namespace API. Consumed by Tasks 1.6
/// (N-level rule inheritance) and 1.12 (hierarchy-aware recall scoring).
#[allow(dead_code)]
///
/// For `"a/b/c"` returns `["a/b/c", "a/b", "a"]`. For a flat namespace
/// returns a single-element vec containing the namespace. For an empty
/// input returns an empty vec.
///
/// # Examples
/// ```
/// # use ai_memory::models::namespace_ancestors;
/// assert_eq!(
///     namespace_ancestors("alphaone/engineering/platform"),
///     vec!["alphaone/engineering/platform", "alphaone/engineering", "alphaone"]
/// );
/// assert_eq!(namespace_ancestors("global"), vec!["global"]);
/// assert!(namespace_ancestors("").is_empty());
/// ```
#[must_use]
pub fn namespace_ancestors(ns: &str) -> Vec<String> {
    if ns.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(namespace_depth(ns));
    let mut current = ns.to_string();
    loop {
        out.push(current.clone());
        match namespace_parent(&current) {
            Some(p) if !p.is_empty() => current = p,
            _ => break,
        }
    }
    out
}
pub const PROMOTION_THRESHOLD: i64 = 5;
/// How much to extend TTL on access (1 hour for short, 1 day for mid)
pub const SHORT_TTL_EXTEND_SECS: i64 = 3600;
pub const MID_TTL_EXTEND_SECS: i64 = 86400;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_from_str_valid() {
        assert_eq!(Tier::from_str("short"), Some(Tier::Short));
        assert_eq!(Tier::from_str("mid"), Some(Tier::Mid));
        assert_eq!(Tier::from_str("long"), Some(Tier::Long));
    }

    #[test]
    fn tier_from_str_invalid() {
        assert_eq!(Tier::from_str("invalid"), None);
        assert_eq!(Tier::from_str(""), None);
        assert_eq!(Tier::from_str("SHORT"), None); // case-sensitive
    }

    #[test]
    fn tier_as_str_roundtrip() {
        for tier in [Tier::Short, Tier::Mid, Tier::Long] {
            let s = tier.as_str();
            assert_eq!(Tier::from_str(s), Some(tier));
        }
    }

    #[test]
    fn tier_default_ttl() {
        assert_eq!(Tier::Short.default_ttl_secs(), Some(6 * 3600));
        assert_eq!(Tier::Mid.default_ttl_secs(), Some(7 * 24 * 3600));
        assert_eq!(Tier::Long.default_ttl_secs(), None);
    }

    #[test]
    fn tier_display() {
        assert_eq!(format!("{}", Tier::Short), "short");
        assert_eq!(format!("{}", Tier::Mid), "mid");
        assert_eq!(format!("{}", Tier::Long), "long");
    }

    #[test]
    fn constants_valid() {
        assert!(MAX_CONTENT_SIZE > 0);
        assert!(PROMOTION_THRESHOLD > 0);
        assert_eq!(SHORT_TTL_EXTEND_SECS, 3600);
        assert_eq!(MID_TTL_EXTEND_SECS, 86400);
    }

    #[test]
    fn tier_rank_ordering() {
        assert!(Tier::Short.rank() < Tier::Mid.rank());
        assert!(Tier::Mid.rank() < Tier::Long.rank());
        assert_eq!(Tier::Short.rank(), 0);
        assert_eq!(Tier::Mid.rank(), 1);
        assert_eq!(Tier::Long.rank(), 2);
    }

    // Task 1.4 — hierarchical namespace helpers --------------------------------

    #[test]
    fn depth_flat_namespace() {
        assert_eq!(namespace_depth("global"), 1);
        assert_eq!(namespace_depth("ai-memory"), 1);
        assert_eq!(namespace_depth("under_score"), 1);
    }

    #[test]
    fn depth_hierarchical() {
        assert_eq!(namespace_depth("a/b"), 2);
        assert_eq!(namespace_depth("alphaone/engineering"), 2);
        assert_eq!(namespace_depth("alphaone/engineering/platform"), 3);
        assert_eq!(
            namespace_depth("a/b/c/d/e/f/g/h"),
            8,
            "max depth of 8 counts each segment"
        );
    }

    #[test]
    fn depth_empty_is_zero() {
        assert_eq!(namespace_depth(""), 0);
    }

    #[test]
    fn parent_hierarchical() {
        assert_eq!(
            namespace_parent("alphaone/engineering/platform"),
            Some("alphaone/engineering".to_string())
        );
        assert_eq!(
            namespace_parent("alphaone/engineering"),
            Some("alphaone".to_string())
        );
    }

    #[test]
    fn parent_flat_is_none() {
        assert_eq!(namespace_parent("global"), None);
        assert_eq!(namespace_parent("ai-memory"), None);
        assert_eq!(namespace_parent(""), None);
    }

    #[test]
    fn ancestors_three_levels() {
        let a = namespace_ancestors("alphaone/engineering/platform");
        assert_eq!(
            a,
            vec![
                "alphaone/engineering/platform".to_string(),
                "alphaone/engineering".to_string(),
                "alphaone".to_string(),
            ],
            "ancestors ordered most-specific-first"
        );
    }

    #[test]
    fn ancestors_flat_namespace() {
        assert_eq!(namespace_ancestors("global"), vec!["global".to_string()]);
        assert_eq!(
            namespace_ancestors("ai-memory"),
            vec!["ai-memory".to_string()]
        );
    }

    #[test]
    fn ancestors_empty_input() {
        assert!(namespace_ancestors("").is_empty());
    }

    #[test]
    fn ancestors_single_level() {
        assert_eq!(namespace_ancestors("a"), vec!["a".to_string()]);
    }

    #[test]
    fn ancestors_max_depth() {
        let a = namespace_ancestors("a/b/c/d/e/f/g/h");
        assert_eq!(a.len(), 8);
        assert_eq!(a[0], "a/b/c/d/e/f/g/h");
        assert_eq!(a[7], "a");
    }

    // Task 1.8 — governance types ---------------------------------------

    #[test]
    fn governance_default_policy() {
        let p = GovernancePolicy::default();
        assert_eq!(p.write, GovernanceLevel::Any);
        assert_eq!(p.promote, GovernanceLevel::Any);
        assert_eq!(p.delete, GovernanceLevel::Owner);
        assert_eq!(p.approver, ApproverType::Human);
    }

    #[test]
    fn governance_level_serde_snake_case() {
        // Serialize each level as a lowercase JSON string
        for (level, expected) in [
            (GovernanceLevel::Any, "any"),
            (GovernanceLevel::Registered, "registered"),
            (GovernanceLevel::Owner, "owner"),
            (GovernanceLevel::Approve, "approve"),
        ] {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            // Roundtrip
            let back: GovernanceLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
    }

    #[test]
    fn approver_type_serde_shapes() {
        // Human → unit variant serializes as bare string
        let json = serde_json::to_string(&ApproverType::Human).unwrap();
        assert_eq!(json, "\"human\"");

        // Agent(s) → externally tagged
        let a = ApproverType::Agent("alice".to_string());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#"{"agent":"alice"}"#);
        let back: ApproverType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);

        // Consensus(n) → externally tagged, numeric payload
        let c = ApproverType::Consensus(3);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, r#"{"consensus":3}"#);
        let back: ApproverType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn governance_policy_full_roundtrip() {
        let p = GovernancePolicy {
            write: GovernanceLevel::Registered,
            promote: GovernanceLevel::Approve,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Agent("maintainer".to_string()),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: GovernancePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn governance_from_metadata_missing() {
        let meta = serde_json::json!({"agent_id": "alice"});
        assert!(GovernancePolicy::from_metadata(&meta).is_none());
    }

    #[test]
    fn governance_from_metadata_null() {
        let meta = serde_json::json!({"governance": null});
        assert!(GovernancePolicy::from_metadata(&meta).is_none());
    }

    #[test]
    fn governance_from_metadata_default_shape() {
        let default = GovernancePolicy::default();
        let meta = serde_json::json!({"governance": serde_json::to_value(&default).unwrap()});
        let parsed = GovernancePolicy::from_metadata(&meta)
            .expect("present")
            .expect("valid");
        assert_eq!(parsed, default);
    }

    #[test]
    fn governance_from_metadata_invalid_returns_err() {
        let meta = serde_json::json!({
            "governance": {"write": "bogus", "promote": "any", "delete": "any", "approver": "human"}
        });
        let result = GovernancePolicy::from_metadata(&meta).expect("present");
        assert!(result.is_err(), "unknown enum value must fail deserialize");
    }

    #[test]
    fn governance_level_as_str_tags() {
        assert_eq!(GovernanceLevel::Any.as_str(), "any");
        assert_eq!(GovernanceLevel::Registered.as_str(), "registered");
        assert_eq!(GovernanceLevel::Owner.as_str(), "owner");
        assert_eq!(GovernanceLevel::Approve.as_str(), "approve");
    }

    #[test]
    fn approver_type_kind_tags() {
        assert_eq!(ApproverType::Human.kind(), "human");
        assert_eq!(ApproverType::Agent("a".into()).kind(), "agent");
        assert_eq!(ApproverType::Consensus(3).kind(), "consensus");
    }
}
