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
}
