// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// v0.7.0 L1-8: `memory_reflect` approval gate. Queued when
    /// `GovernancePolicy::require_approval_above_depth` is set and the
    /// proposed reflection depth exceeds the threshold.
    Reflect,
}

impl GovernedAction {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Delete => "delete",
            Self::Promote => "promote",
            Self::Reflect => "reflect",
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
    /// v0.7.0 recursive-learning Task 2/8 (issue #655): per-namespace
    /// substrate-side cap on `Memory::reflection_depth` at the
    /// `memory_reflect` MCP write path (enforcement lands in Task 5/8).
    /// `None` → no override, fall back to the compiled default exposed
    /// by [`Self::effective_max_reflection_depth`]. `Some(0)` is the
    /// disable-all-reflections sentinel (see accessor doc-comment).
    /// Persisted inside the existing namespace standard's
    /// `metadata.governance` JSON blob; no SQL schema migration is
    /// required because the column is already a `TEXT`/`JSONB`
    /// payload on both SQLite and Postgres. Pre-v0.7.0 rows that
    /// omit this key deserialize as `None` via `#[serde(default)]`,
    /// and `skip_serializing_if` keeps the absent shape on the wire
    /// for fresh policies — matching how `NamespaceMetaEntry::parent_namespace`
    /// stays absent on the wire to keep replication / federation
    /// payloads byte-identical for legacy peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reflection_depth: Option<u32>,
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
            // v0.7.0 Task 2/8: `None` means "no per-namespace override",
            // and `effective_max_reflection_depth` resolves to the
            // compiled default of 3.
            max_reflection_depth: None,
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

    /// NHI-P4-T19 (v0.7.0 NHI testing): default policy for namespaces
    /// that have a standard set but no explicit `metadata.governance`.
    /// Differs from [`Default::default`] (write=Any) by tightening
    /// `write` to `Owner` — calling `memory_namespace_set_standard`
    /// implies the operator wants enforcement, not advisory-only.
    /// Operators who want write=Any must set it explicitly in the
    /// standard memory's metadata. Tested in
    /// `db::tests::namespace_set_standard_default_write_is_owner`.
    #[must_use]
    pub fn default_for_managed_namespace() -> Self {
        Self {
            write: GovernanceLevel::Owner,
            promote: default_promote_level(),
            delete: default_delete_level(),
            approver: default_approver(),
            inherit: default_inherit(),
            // v0.7.0 Task 2/8: managed-namespace bootstrap leaves
            // `max_reflection_depth` unset so operators get the
            // compiled-in default (3) until they explicitly override.
            max_reflection_depth: None,
        }
    }

    /// v0.7.0 recursive-learning Task 2/8 (issue #655): resolve the
    /// per-namespace reflection-depth cap. Returns the operator's
    /// override when present, otherwise the compiled-in default of
    /// `3`.
    ///
    /// **Why 3?** Bounds recursion (reflection-on-reflection-on-…)
    /// without strangling the legitimate "reflection-on-reflection"
    /// chains the v0.8.0 Pillar 2.5 curator mode will lean on.
    /// Operators who want a different global default should change
    /// the constant in this accessor; per-namespace overrides should
    /// stay in the JSON metadata blob.
    ///
    /// **`Some(0)` disables reflection entirely.** Task 5/8 enforces
    /// the rule `proposed_reflection_depth >= cap → refuse`, so a
    /// cap of `0` refuses every reflection (no depth `>= 0` passes
    /// the comparison). This is the documented kill-switch for a
    /// namespace that should never accept reflection writes.
    ///
    /// Ancestor inheritance is **not** walked here — that's the job
    /// of `db::resolve_governance_policy` (and the equivalent store
    /// trait method), which returns the most-specific policy via the
    /// leaf-first namespace chain walk. Callers at the
    /// `memory_reflect` MCP write path resolve the policy first,
    /// then call this accessor on the result.
    #[must_use]
    pub fn effective_max_reflection_depth(&self) -> u32 {
        self.max_reflection_depth.unwrap_or(3)
    }
}

/// Namespace reserved for agent registrations (Task 1.3).
pub const AGENTS_NAMESPACE: &str = "_agents";

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
