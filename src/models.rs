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
    /// v0.7 H3 — optional 64-byte Ed25519 signature carried over the
    /// federation wire. `None` for legacy peers (pre-v0.7) that do not
    /// sign outbound links; receivers in that case land the row with
    /// `attest_level = "unsigned"`. When `Some`, it is verified against
    /// the public key associated with `observed_by` before insert.
    /// `skip_serializing_if` keeps the wire shape byte-identical to
    /// pre-H3 for unsigned rows so v0.6.x peers continue to deserialize
    /// without surprise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Vec<u8>>,
    /// v0.7 H3 — agent_id that asserts this link. Mirrors the H2
    /// `SignableLink.observed_by` field. Required when `signature` is
    /// `Some` (it is the lookup key for the verifying public key);
    /// `None` is treated as "no claim" and short-circuits to unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_by: Option<String>,
    /// v0.7 H3 — RFC3339 instant the link became true (matches the
    /// homonymous column in `memory_links`). Part of the signed bundle;
    /// must round-trip byte-identical with what the sender signed for
    /// verification to succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// v0.7 H3 — RFC3339 instant the link was invalidated, or `None` if
    /// still valid. Part of the signed bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<String>,
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
    /// Task 1.11 — context-budget-aware recall. When set, return the
    /// top-scored memories whose cumulative estimated tokens fit within
    /// this budget.
    #[serde(default)]
    pub budget_tokens: Option<usize>,
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
    /// Task 1.11 — context-budget-aware recall.
    #[serde(default)]
    pub budget_tokens: Option<usize>,
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
pub struct TierCount {
    pub tier: String,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct NamespaceCount {
    pub namespace: String,
    pub count: usize,
}

/// One node of the hierarchical namespace tree returned by
/// `memory_get_taxonomy` (Pillar 1 / Stream A).
///
/// `count` is the number of memories at *exactly* this namespace;
/// `subtree_count` is the count of memories at this node plus every
/// descendant the depth limit allowed us to expand. Children are sorted
/// alphabetically by `name` so callers get a stable rendering order.
#[derive(Debug, Clone, Serialize)]
pub struct TaxonomyNode {
    /// Full namespace path of this node. Empty string for the synthetic
    /// root when no `namespace_prefix` is supplied.
    pub namespace: String,
    /// Last `/`-delimited segment of `namespace` (display label). Empty
    /// for the synthetic root.
    pub name: String,
    /// Memories whose namespace equals this node's `namespace`.
    pub count: usize,
    /// Memories at this node plus all descendants visible within the
    /// requested `depth`. Memories beneath the depth cutoff still
    /// contribute to the `subtree_count` of the boundary ancestor.
    pub subtree_count: usize,
    /// Direct child nodes, sorted alphabetically by `name`.
    pub children: Vec<TaxonomyNode>,
}

/// Result envelope returned by `db::get_taxonomy`.
///
/// `total_count` is the global memory count for the prefix (independent
/// of `depth`/`limit` truncation) so callers can render an honest
/// "X memories in N namespaces" header even when the tree was
/// truncated. `truncated` is set when the `limit` parameter forced us
/// to drop input rows when assembling the tree.
#[derive(Debug, Clone, Serialize)]
pub struct Taxonomy {
    pub tree: TaxonomyNode,
    pub total_count: usize,
    pub truncated: bool,
}

/// One nearest-neighbor result from a `memory_check_duplicate` lookup
/// (Pillar 2 / Stream D). `similarity` is the cosine similarity in
/// `[-1.0, 1.0]`, rounded to three decimals at the response layer.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateMatch {
    pub id: String,
    pub title: String,
    pub namespace: String,
    pub similarity: f32,
}

/// Result envelope returned by `db::check_duplicate`.
///
/// `is_duplicate` is `nearest.similarity >= threshold`. `nearest` is
/// `None` only when the candidate pool is empty (no embedded, live
/// memories matched the namespace filter). When `is_duplicate` is true,
/// `nearest.id` doubles as the suggested merge target — we surface it
/// under that name in the JSON response so the contract stays explicit.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateCheck {
    pub is_duplicate: bool,
    pub threshold: f32,
    pub nearest: Option<DuplicateMatch>,
    pub candidates_scanned: usize,
}

/// Namespace reserved for agent registrations (Task 1.3).
pub const AGENTS_NAMESPACE: &str = "_agents";

/// Tag stamped on entity-typed memories so `(title, namespace)` can be
/// shared across regular memories and entities without ambiguity (Pillar
/// 2 / Stream B).
pub const ENTITY_TAG: &str = "entity";

/// Marker written to `metadata.kind` on entity-typed memories. The
/// db layer keys entity lookups off this field so the alias resolver
/// never returns a regular memory that happens to share a title with an
/// entity registered later.
pub const ENTITY_KIND: &str = "entity";

/// Resolved entity record returned by `db::entity_get_by_alias` and
/// embedded in the `db::entity_register` response (Pillar 2 / Stream B).
/// `aliases` is the full alias set for the entity, ordered by
/// `created_at ASC, alias ASC` for stable display.
#[derive(Debug, Clone, Serialize)]
pub struct EntityRecord {
    pub entity_id: String,
    pub canonical_name: String,
    pub namespace: String,
    pub aliases: Vec<String>,
}

/// Outcome of `db::entity_register`. `created` is `true` when a new
/// entity memory was inserted, `false` when an existing entity was
/// reused (idempotent re-registration that just merged new aliases into
/// the existing record).
#[derive(Debug, Clone, Serialize)]
pub struct EntityRegistration {
    pub entity_id: String,
    pub canonical_name: String,
    pub namespace: String,
    pub aliases: Vec<String>,
    pub created: bool,
}

/// Single row returned by `db::kg_timeline` (Pillar 2 / Stream C).
///
/// Captures one outbound assertion from a source memory: the
/// `target_id` and its `relation`, the temporal-validity window
/// (`valid_from` / `valid_until`), the agent that observed it
/// (`observed_by`), and the target's display fields (`title`,
/// `target_namespace`) for caller convenience. `valid_from` is the
/// authoritative ordering key — events with NULL `valid_from` are
/// excluded from the timeline by the query.
#[derive(Debug, Clone, Serialize)]
pub struct KgTimelineEvent {
    pub target_id: String,
    pub relation: String,
    pub valid_from: String,
    pub valid_until: Option<String>,
    pub observed_by: Option<String>,
    pub title: String,
    pub target_namespace: String,
}

/// One node returned by `db::kg_query` (Pillar 2 / Stream C —
/// `memory_kg_query`). Each node represents a memory reachable from the
/// query's source through one outbound link, carrying the link's
/// temporal-validity columns plus the target memory's display fields and
/// the traversal path. `depth` is the actual number of hops from the
/// source (1..=`KG_QUERY_MAX_SUPPORTED_DEPTH`); `path` is the
/// `src->mid->target` chain as discovered by the recursive CTE.
#[derive(Debug, Clone, Serialize)]
pub struct KgQueryNode {
    pub target_id: String,
    pub relation: String,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub observed_by: Option<String>,
    pub title: String,
    pub target_namespace: String,
    pub depth: usize,
    pub path: String,
}

// ---------------------------------------------------------------------------
// Task 1.9 — Governance Enforcement
// ---------------------------------------------------------------------------

/// The outcome of a governance check. Callers MAY execute on `Allow`,
/// MUST reject on `Deny`, and SHOULD queue + return the `pending_id` on
/// `Pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernanceDecision {
    /// Allowed; proceed with the action.
    Allow,
    /// Denied; surface the reason to the caller.
    Deny(String),
    /// Queued for approval; the caller receives the new `pending_id`.
    Pending(String),
}

/// Actions that governance gates. Used as the `action_type` column value in
/// `pending_actions` and as the discriminator for enforcement calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernedAction {
    Store,
    Delete,
    Promote,
}

impl GovernedAction {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Delete => "delete",
            Self::Promote => "promote",
        }
    }
}

/// A single approval vote recorded on a consensus-gated pending action (Task 1.10).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Approval {
    pub agent_id: String,
    pub approved_at: String,
}

/// Row returned by `db::list_pending_actions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAction {
    pub id: String,
    pub action_type: String,
    pub memory_id: Option<String>,
    pub namespace: String,
    pub payload: Value,
    pub requested_by: String,
    pub requested_at: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    /// Task 1.10: consensus vote log. Empty for Human/Agent paths.
    #[serde(default)]
    pub approvals: Vec<Approval>,
}

/// v0.6.2 (S34): a pending-action decision (approve / reject) the originating
/// node wants propagated to peers so callers on any peer see consistent state
/// (approve/reject on node-2 → decision must reach node-1 etc.).
///
/// Shipped as an additive `sync_push.pending_decisions` field. Peers apply
/// via `db::decide_pending_action`; already-decided rows are a no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDecision {
    pub id: String,
    pub approved: bool,
    pub decider: String,
}

/// v0.6.2 (S35): a namespace-standard metadata row the originating node wants
/// propagated to peers. `set_namespace_standard` writes to `namespace_meta`
/// locally; without federation, a peer sees the standard memory (fanned out
/// via `broadcast_store_quorum`) but not the `(namespace, standard_id,
/// parent_namespace)` tuple, so inheritance-chain walks on the peer fall
/// back to `auto_detect_parent` and can miss an explicit parent link.
///
/// Shipped as an additive `sync_push.namespace_meta` field. Peers apply
/// via `db::set_namespace_standard(conn, namespace, standard_id,
/// parent_namespace.as_deref())`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceMetaEntry {
    pub namespace: String,
    pub standard_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_namespace: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}

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
/// `{ write: Any, promote: Any, delete: Owner, approver: Human, inherit: true }`.
///
/// v0.6.2 (S34 defensive): `promote`, `delete`, and `approver` carry
/// `#[serde(default)]` so partial-policy payloads (a common shape for
/// operator CLIs / test harnesses that only care about `write`) round-trip
/// instead of 400-ing out on missing fields. `write` remains required —
/// it's the core knob a policy is attempting to set.
///
/// v0.6.3.1 (P4, audit G1): `inherit` controls whether parent-namespace
/// policies bubble up. Default `true` matches the architecture page T2
/// promise of "Hierarchical policy inheritance (default at `org/`,
/// overridable at `org/team/`)". Setting `inherit: false` on a child
/// stops the leaf-first walk in `resolve_governance_policy`, providing
/// an explicit opt-out path for scoped overrides (e.g. an audit
/// sandbox under a fully-governed parent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernancePolicy {
    pub write: GovernanceLevel,
    #[serde(default = "default_promote_level")]
    pub promote: GovernanceLevel,
    #[serde(default = "default_delete_level")]
    pub delete: GovernanceLevel,
    #[serde(default = "default_approver")]
    pub approver: ApproverType,
    /// v0.6.3.1 (P4, G1): when `true` (default), missing policy at a
    /// child namespace falls through to the parent in the chain. When
    /// `false`, the walk stops at this level — child operations are
    /// gated by THIS policy and parents are not consulted. Backfilled
    /// to `true` on existing rows by migration `0012_governance_inherit`
    /// to preserve the architecturally-promised semantics.
    #[serde(default = "default_inherit")]
    pub inherit: bool,
}

fn default_promote_level() -> GovernanceLevel {
    GovernanceLevel::Any
}

fn default_delete_level() -> GovernanceLevel {
    GovernanceLevel::Owner
}

fn default_approver() -> ApproverType {
    ApproverType::Human
}

/// v0.6.3.1 (P4): default for `GovernancePolicy::inherit`. Inheritance
/// is the documented default — see architecture page T2 and audit G1.
fn default_inherit() -> bool {
    true
}

impl Default for GovernancePolicy {
    fn default() -> Self {
        Self {
            write: GovernanceLevel::Any,
            promote: default_promote_level(),
            delete: default_delete_level(),
            approver: default_approver(),
            inherit: default_inherit(),
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

/// Phase 3 foundation (issue #224): vector clock tracking the latest
/// `updated_at` this peer has seen from each known remote peer.
///
/// Entries are populated lazily — both on HTTP `/sync/push` (receiver
/// records the sender's latest `updated_at`) and on HTTP `/sync/since`
/// (sender advances `last_pulled_at`). Full CRDT-lite merge rules using
/// the clock are **not** in the v0.6.0 GA foundation; they land in a
/// follow-up PR under issue #224 Task 3a.1. The foundation ships the
/// wire format so adding the merge semantics later does not force a
/// schema migration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VectorClock {
    /// Map of peer `agent_id` -> latest RFC3339 `updated_at` seen from
    /// that peer. A peer absent from the map is equivalent to
    /// "never-seen-anything." Encoded as a JSON object on the wire.
    #[serde(default)]
    pub entries: std::collections::BTreeMap<String, String>,
}

impl VectorClock {
    /// Advance this clock to include `peer_id`'s latest seen timestamp.
    /// Monotonic — an older timestamp never overwrites a newer one.
    #[allow(dead_code)] // Consumed by Task 3a.1 CRDT-lite merge (issue #224).
    pub fn observe(&mut self, peer_id: &str, at: &str) {
        self.entries
            .entry(peer_id.to_string())
            .and_modify(|existing| {
                if at > existing.as_str() {
                    *existing = at.to_string();
                }
            })
            .or_insert_with(|| at.to_string());
    }

    /// Look up the latest timestamp this clock has from `peer_id`.
    #[must_use]
    #[allow(dead_code)] // Consumed by Task 3a.1 CRDT-lite merge (issue #224).
    pub fn latest_from(&self, peer_id: &str) -> Option<&str> {
        self.entries.get(peer_id).map(String::as_str)
    }
}

/// Phase 3 foundation: one row of the `sync_state` table serialised for
/// diagnostic / API responses.
#[allow(dead_code)] // Consumed by Task 3b.2 sync diagnostics API (issue #224).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStateEntry {
    pub agent_id: String,
    pub peer_id: String,
    pub last_seen_at: String,
    pub last_pulled_at: String,
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
        const _: () = assert!(MAX_CONTENT_SIZE > 0);
        const _: () = assert!(PROMOTION_THRESHOLD > 0);
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
        // v0.6.3.1 (P4, G1): inheritance is the documented default. Existing
        // rows are backfilled to true by migration 0012; new rows that omit
        // the field deserialize as true via #[serde(default)].
        assert!(p.inherit);
    }

    #[test]
    fn governance_inherit_field_defaults_true_on_partial_payload() {
        // P4 (G1): a partial-policy payload that omits `inherit` must
        // default to true so legacy callers don't accidentally opt out
        // of parent inheritance the moment they write a child policy.
        let json = r#"{"write":"approve"}"#;
        let p: GovernancePolicy = serde_json::from_str(json).unwrap();
        assert_eq!(p.write, GovernanceLevel::Approve);
        assert!(p.inherit, "missing `inherit` must deserialize as true");
    }

    #[test]
    fn governance_inherit_field_explicit_false_round_trip() {
        // P4 (G1): when an operator explicitly opts a subtree out of
        // inheritance, the false value must round-trip and serialize.
        let json = r#"{"write":"any","inherit":false}"#;
        let p: GovernancePolicy = serde_json::from_str(json).unwrap();
        assert!(!p.inherit);
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back["inherit"], false);
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
            inherit: true,
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

    // v0.6.2 (S34 defense): partial policy payloads fall back to the
    // `Default for GovernancePolicy` values for any field the caller omitted.
    // `write` remains required — it's the core knob the policy expresses.

    #[test]
    fn governance_partial_policy_write_only_uses_defaults() {
        let json = serde_json::json!({"write": "owner"});
        let parsed: GovernancePolicy = serde_json::from_value(json).expect("write-only parses");
        assert_eq!(parsed.write, GovernanceLevel::Owner);
        assert_eq!(parsed.promote, GovernanceLevel::Any);
        assert_eq!(parsed.delete, GovernanceLevel::Owner);
        assert_eq!(parsed.approver, ApproverType::Human);
    }

    #[test]
    fn governance_partial_policy_write_and_promote() {
        let json = serde_json::json!({"write": "any", "promote": "registered"});
        let parsed: GovernancePolicy = serde_json::from_value(json).expect("parses");
        assert_eq!(parsed.promote, GovernanceLevel::Registered);
        // Absent fields still take defaults.
        assert_eq!(parsed.delete, GovernanceLevel::Owner);
        assert_eq!(parsed.approver, ApproverType::Human);
    }

    #[test]
    fn governance_missing_write_still_errors() {
        // `write` is the core policy knob — must remain required to avoid
        // silently accepting an empty object as "any writes allowed".
        let json = serde_json::json!({"promote": "owner"});
        let err = serde_json::from_value::<GovernancePolicy>(json);
        assert!(err.is_err(), "missing write must fail deserialize");
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

    // -----------------------------------------------------------------
    // W12-H — additional small-module pinning
    // -----------------------------------------------------------------

    #[test]
    fn default_metadata_is_empty_object() {
        let v = default_metadata();
        assert!(v.is_object());
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn governed_action_as_str_pinned() {
        assert_eq!(GovernedAction::Store.as_str(), "store");
        assert_eq!(GovernedAction::Delete.as_str(), "delete");
        assert_eq!(GovernedAction::Promote.as_str(), "promote");
    }

    #[test]
    fn governance_decision_equality() {
        assert_eq!(GovernanceDecision::Allow, GovernanceDecision::Allow);
        assert_ne!(
            GovernanceDecision::Deny("a".into()),
            GovernanceDecision::Deny("b".into()),
        );
        assert_eq!(
            GovernanceDecision::Pending("p1".into()),
            GovernanceDecision::Pending("p1".into())
        );
    }

    #[test]
    fn vector_clock_observe_monotonic() {
        let mut vc = VectorClock::default();
        vc.observe("peer-a", "2026-04-01T00:00:00+00:00");
        vc.observe("peer-a", "2026-05-01T00:00:00+00:00");
        // Older never overwrites newer.
        vc.observe("peer-a", "2026-03-01T00:00:00+00:00");
        assert_eq!(vc.latest_from("peer-a"), Some("2026-05-01T00:00:00+00:00"));
    }

    #[test]
    fn vector_clock_latest_from_unknown_is_none() {
        let vc = VectorClock::default();
        assert!(vc.latest_from("never-seen").is_none());
    }

    #[test]
    fn vector_clock_serde_roundtrip() {
        let mut vc = VectorClock::default();
        vc.observe("p1", "2026-04-01T00:00:00+00:00");
        vc.observe("p2", "2026-04-02T00:00:00+00:00");
        let json = serde_json::to_string(&vc).unwrap();
        let back: VectorClock = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back, vc);
    }

    #[test]
    fn namespace_parent_with_trailing_slash() {
        // "a/" splits to parent="a" and tail="". The function returns the
        // parent regardless of whether the final segment is empty.
        assert_eq!(namespace_parent("a/"), Some("a".to_string()));
    }

    #[test]
    fn namespace_depth_skips_empty_segments() {
        // Multiple slashes do not inflate the depth count.
        assert_eq!(namespace_depth("a//b"), 2);
        assert_eq!(namespace_depth("/a"), 1);
        assert_eq!(namespace_depth("a/"), 1);
    }

    #[test]
    fn namespace_ancestors_two_levels() {
        // Two-level namespace produces self + parent.
        assert_eq!(
            namespace_ancestors("a/b"),
            vec!["a/b".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn memory_serde_roundtrip_minimal() {
        let m = Memory {
            id: "abc".into(),
            tier: Tier::Mid,
            namespace: "global".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec!["x".into()],
            priority: 5,
            confidence: 0.9,
            source: "api".into(),
            access_count: 0,
            created_at: "2026-04-01T00:00:00+00:00".into(),
            updated_at: "2026-04-01T00:00:00+00:00".into(),
            last_accessed_at: None,
            expires_at: None,
            metadata: default_metadata(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, m.id);
        assert_eq!(back.tier, Tier::Mid);
    }

    #[test]
    fn approver_type_kind_for_each_variant() {
        // Hits all three discriminant arms. Mirrors the existing test but
        // ensures we cover a Consensus(0) which is the lower edge.
        assert_eq!(ApproverType::Human.kind(), "human");
        assert_eq!(ApproverType::Agent(String::new()).kind(), "agent");
        assert_eq!(ApproverType::Consensus(0).kind(), "consensus");
    }

    #[test]
    fn governance_partial_policy_with_approver() {
        // Partial policy with `approver` set and other fields defaulted.
        let json = serde_json::json!({
            "write": "owner",
            "approver": {"agent": "alice"}
        });
        let parsed: GovernancePolicy = serde_json::from_value(json).expect("parses");
        assert_eq!(parsed.write, GovernanceLevel::Owner);
        assert_eq!(parsed.approver, ApproverType::Agent("alice".to_string()));
        assert_eq!(parsed.promote, GovernanceLevel::Any);
        assert_eq!(parsed.delete, GovernanceLevel::Owner);
    }
}
