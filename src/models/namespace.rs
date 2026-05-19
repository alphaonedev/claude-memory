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
///
/// # #880 / #793 PR-3 — decomposition (2026-05-18)
///
/// Pre-#880 the struct carried 20 flat fields. Adding any new field
/// forced a 50-site struct-literal cascade across `src/` + `tests/`
/// (the surface this issue closes). Post-#880 the same 20 fields are
/// grouped into 7 per-concern sub-structs and re-attached to the
/// parent via `#[serde(flatten)]`. The composite still carries every
/// field, so the wire-format / TOML / `metadata.governance` JSON
/// shape is unchanged (pinned by
/// `tests/governance_policy_wire_compat.rs`). Each existing field
/// is still reachable via the new `policy.core.write`,
/// `policy.atomisation.auto_atomise`, etc. paths, and every
/// `effective_*` accessor on the parent struct delegates to the
/// matching sub-struct so the rest of the codebase that calls
/// `policy.effective_max_reflection_depth()` is unchanged.
///
/// Adding a new policy knob now means:
/// 1. Pick the right sub-struct under [`CorePolicy`] /
///    [`AtomisationPolicy`] / etc.
/// 2. Add the field (with `#[serde(default, skip_serializing_if = "Option::is_none")]`).
/// 3. Add the field to the sub-struct's `Default` impl.
///
/// No literal-site cascade. The `..Default::default()` pattern used
/// at every construction site picks up the new field automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernancePolicy {
    /// Access-control + inheritance + reflection-depth — the
    /// load-bearing K9/K10 governance knobs. See [`CorePolicy`].
    #[serde(flatten)]
    pub core: CorePolicy,
    /// WT-1-D + Form 2 atomisation knobs. See [`AtomisationPolicy`].
    #[serde(flatten)]
    pub atomisation: AtomisationPolicy,
    /// Form 1 synthesis curator knobs + legacy per-pair opt-in. See
    /// [`SynthesisPolicy`].
    #[serde(flatten)]
    pub synthesis: SynthesisPolicy,
    /// Form 3 multistep-ingest prompt sizing knobs. See
    /// [`MultistepPolicy`].
    #[serde(flatten)]
    pub multistep: MultistepPolicy,
    /// Form 6 memory-kind auto-classifier knobs. See
    /// [`KindClassificationPolicy`].
    #[serde(flatten)]
    pub kind_class: KindClassificationPolicy,
    /// QW-2 persona auto-regeneration cadence + file-backed export
    /// knobs. See [`PersonaPolicy`].
    #[serde(flatten)]
    pub persona: PersonaPolicy,
    /// QW-1 reflection-export knob. See [`ExportPolicy`].
    #[serde(flatten)]
    pub export: ExportPolicy,
}

/// #880 — access-control + inheritance + reflection-depth sub-struct
/// of [`GovernancePolicy`]. Every field is flattened back into the
/// parent on the wire so `metadata.governance` JSON / TOML configs
/// remain byte-identical to the pre-#880 flat layout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorePolicy {
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
    /// by [`GovernancePolicy::effective_max_reflection_depth`].
    /// `Some(0)` is the disable-all-reflections sentinel (see accessor
    /// doc-comment). Persisted inside the existing namespace standard's
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

impl Default for CorePolicy {
    fn default() -> Self {
        Self {
            write: GovernanceLevel::Any,
            promote: default_promote_level(),
            delete: default_delete_level(),
            approver: default_approver(),
            inherit: default_inherit(),
            max_reflection_depth: None,
        }
    }
}
/// #880 — QW-1 reflection-export sub-struct of [`GovernancePolicy`].
/// Single-field cluster preserved as its own sub-struct so future
/// reflection-side knobs (e.g. a v0.8 retention sweep) land here
/// without churning literal sites.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportPolicy {
    /// v0.7.0 QW-1 — when `Some(true)`, the `post_reflect` substrate
    /// hook deferred-spawns a filesystem write of the reflection
    /// markdown to `~/.ai-memory/reflections/<namespace>/<id>.md` so
    /// operators can `cat` the reflection chain without learning SQL.
    /// Inherits via the same leaf-first ancestor walk as every other
    /// field on this struct (G1 governance). `None` / `Some(false)`
    /// keeps the substrate quiet — the canonical reflection is the
    /// SQL row, never the file. `skip_serializing_if = "Option::is_none"`
    /// keeps the absent shape on the wire for pre-QW-1 federation
    /// peers (no payload-byte drift, no replication regressions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_export_reflections_to_filesystem: Option<bool>,
}

/// #880 — WT-1-D + Form 2 atomisation sub-struct of
/// [`GovernancePolicy`]. Groups the five atomisation knobs so a new
/// Form 2 / Cluster-F knob lands on this struct without cascading
/// through every literal site.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtomisationPolicy {
    /// v0.7.0 WT-1-D — when `Some(true)`, the `pre_store` substrate
    /// hook (`AutoAtomisationHook`) deferred-enqueues a curator pass
    /// on the stored memory if its body exceeds
    /// `auto_atomise_threshold_cl100k`. Inherits leaf-first via the
    /// namespace chain (same walk as every other field). `None` /
    /// `Some(false)` keeps the substrate quiet; the operator opts in
    /// per-namespace by setting this to `Some(true)` on the namespace
    /// standard's `metadata.governance` blob. `skip_serializing_if`
    /// keeps absent-on-wire for pre-WT-1-D federation peers (zero
    /// replication drift).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_atomise: Option<bool>,
    /// v0.7.0 WT-1-D — cl100k_base token threshold over which a
    /// `memory_store` triggers the auto-atomisation curator pass.
    /// `None` defers to the compiled default (500). Resolved via the
    /// same leaf-first inheritance walk; a child `None` inherits the
    /// nearest ancestor's explicit `Some(n)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_atomise_threshold_cl100k: Option<u32>,
    /// v0.7.0 WT-1-D — per-atom token budget passed to the curator
    /// when the auto-atomisation hook fires. `None` defers to the
    /// compiled default (200, matching `AtomiserConfig::default_max_atom_tokens`).
    /// Resolved via the same leaf-first inheritance walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_atomise_max_atom_tokens: Option<u32>,
    /// v0.7.0 Cluster-F (issue #767, PERF-5) — per-namespace override
    /// for the curator retry budget used by the
    /// **Synchronous** `pre_store` auto-atomise path. `None` defers to
    /// the compiled default `AtomiserConfig::sync_curator_max_retries`
    /// (1 — chosen to keep the operator's `memory_store` latency
    /// envelope tight; the deferred path keeps the full 3-retry
    /// budget because it runs on a detached worker thread).
    ///
    /// Operators who need higher resilience on a specific
    /// Synchronous-mode namespace (at the cost of a longer
    /// worst-case envelope) raise this explicitly. Resolved via the
    /// same leaf-first inheritance walk as every other field on this
    /// struct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_atomise_max_retries: Option<u32>,
    /// v0.7.x Form 2 (Batman framework) — atomisation execution mode.
    ///
    /// - `None` / `Some(Off)` → no atomisation occurs (overrides any
    ///   `auto_atomise` flag).
    /// - `Some(Deferred)` → legacy WT-1-D behaviour: curator runs on a
    ///   detached worker thread AFTER `memory_store` returns. Source
    ///   is embedded as one blob before the curator round-trip lands.
    /// - `Some(Synchronous)` → Form 2 alignment: SKIP source embedding,
    ///   run the curator synchronously inside `memory_store`, atoms get
    ///   their normal embed-on-insert path, source is archived with
    ///   `atomised_into > 0` BEFORE the response returns.
    ///
    /// Backward compatibility: when this field is absent and
    /// `auto_atomise = Some(true)` is set, the resolver implicitly maps
    /// to `Some(Deferred)` so v0.7.0 pre-Form-2 deployments keep their
    /// existing behaviour. See
    /// [`GovernancePolicy::effective_auto_atomise_mode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_atomise_mode: Option<AutoAtomiseMode>,
}

/// #880 — QW-2 persona auto-regeneration + file-backed export
/// sub-struct of [`GovernancePolicy`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaPolicy {
    /// v0.7.0 QW-2 — auto-regenerate the Persona artefact for an
    /// entity every N writes to a same-entity Reflection memory.
    /// `None` (default) disables the cadence — operators trigger
    /// regeneration explicitly via `memory_persona_generate` or
    /// `ai-memory persona <entity_id> --regenerate`. Inherits via
    /// the same leaf-first ancestor walk as every other field on
    /// this struct (G1 governance). `skip_serializing_if` keeps
    /// the absent shape on the wire for pre-QW-2 federation peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_persona_trigger_every_n_memories: Option<u32>,
    /// v0.7.0 QW-2 companion to
    /// `auto_export_reflections_to_filesystem` — when `Some(true)`,
    /// the substrate writes generated Personas to
    /// `~/.ai-memory/personas/<namespace>/<entity_id>.md` so
    /// operators can `cat` the persona without learning SQL. The
    /// canonical persona is the SQL row; the file is a derived
    /// artefact. `None` / `Some(false)` keeps the substrate quiet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_export_personas_to_filesystem: Option<bool>,
}

/// #880 — Form 1 synthesis curator + legacy per-pair classifier
/// sub-struct of [`GovernancePolicy`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SynthesisPolicy {
    /// v0.7.x Form 1 (Batman framework) — opt-IN to the legacy per-pair
    /// yes/no contradiction classifier on the store path. Default
    /// (`None` / `Some(false)`) routes through the new single-batch
    /// action-emitting synthesiser. Operators who depend on the old
    /// metadata-only `confirmed_contradictions` behaviour set this to
    /// `Some(true)` per-namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_per_pair_classifier: Option<bool>,
    /// v0.7.0 Cluster-B (issue #767) — per-namespace knob controlling
    /// what happens when the Form 1 synthesis curator call fails (LLM
    /// down, malformed JSON, validation failure, etc.).
    ///
    /// * `None` / `Some(FallThrough)` (default) — preserve the v0.7.0
    ///   pre-cluster-B behaviour: log a warning, swallow the error,
    ///   continue with the legacy dedup-merge / insert path. Backward
    ///   compatible.
    /// * `Some(BlockWrite)` — refuse the write with a typed error so
    ///   the caller knows the synthesis layer failed and the substrate
    ///   did not silently fall through to a different code path. Use
    ///   on namespaces where the synthesis verdict is operationally
    ///   load-bearing (e.g. a fact-base where duplicate writes are
    ///   not tolerable).
    ///
    /// Synthesis is a QUALITY gate, not a SECURITY gate — the K9 / K10
    /// governance pipeline remains the security surface even under
    /// `BlockWrite`. This knob simply lets operators choose whether a
    /// curator outage degrades silently or surfaces loudly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis_failure_mode: Option<SynthesisFailureMode>,
    /// v0.7.0 Cluster-B (issue #767, SEC-1) — per-namespace cap on the
    /// number of `delete` verdicts a single synthesis batch may apply
    /// without an explicit K10 approval flow.
    ///
    /// Default `None` resolves to **1**, matching the principle of
    /// least authority: a single LLM round-trip should not be able to
    /// purge many candidates from the namespace in a silent batch. A
    /// verdict exceeding the cap is refused at the substrate boundary;
    /// the audit-honest event `synthesis.refused_unbounded_delete`
    /// fires at WARN level.
    ///
    /// Operators who need a higher cap (e.g. a corpus where mass
    /// dedupe is a normal substrate task) raise this explicitly. The
    /// security pipeline (K9 per-delete recheck) still runs regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis_max_deletes_per_call: Option<u32>,
    /// v0.7.0 Cluster-B (issue #767, PERF-7) — per-candidate cap on
    /// the number of characters of `content` inlined into the
    /// synthesis prompt. A huge candidate (e.g. a 50KB note) otherwise
    /// inflates the prompt unboundedly and inflates LLM cost.
    ///
    /// Default `None` resolves to **1500** characters (~400 tokens at
    /// the cl100k average). The truncation only affects what the LLM
    /// sees; the stored row is untouched. A truncation event records
    /// the byte budget in the `synthesis_prompt_size_chars` telemetry
    /// counter so operators can observe whether the cap matters in
    /// production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis_max_candidate_chars: Option<u32>,
}

/// #880 — Form 6 memory-kind auto-classifier sub-struct of
/// [`GovernancePolicy`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KindClassificationPolicy {
    /// v0.7.x Form 6 (issue #759) — auto-classify a stored memory's
    /// `MemoryKind` from its content via the substrate-side
    /// `pre_store::auto_classify_kind` hook. One of:
    ///   * `Off` (default) — keeps the substrate quiet; the
    ///     caller-supplied kind (or the SQL `DEFAULT 'observation'`)
    ///     stands.
    ///   * `RegexOnly` — deterministic regex heuristics (e.g.
    ///     "is_a" → Concept; "happened on" → Event;
    ///     "X says:" → Conversation). No LLM round-trip; tens of
    ///     microseconds per call.
    ///   * `RegexThenLlm` — regex first; if low-confidence (no
    ///     heuristic fired or multiple fired with conflict), fall
    ///     through to a single-shot LLM classifier. Opt-in only;
    ///     the substrate never spawns an LLM round-trip on a
    ///     namespace whose policy is `Off`.
    /// Caller-supplied `memory_kind` always wins — the hook only
    /// fills in `Observation` (the default) when no kind was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_classify_kind: Option<MemoryKindAutoClassify>,
}

/// #880 — Form 3 multistep-ingest prompt sizing sub-struct of
/// [`GovernancePolicy`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MultistepPolicy {
    /// v0.7.0 Cluster v0.7-polish (issue #782, PERF-11) — per-namespace
    /// cap on the number of characters of `content` inlined into a Form
    /// 3 multistep-ingest LLM-stage prompt. Form 3's deterministic
    /// helper stages already receive the content by **borrow**, so the
    /// cap only affects LLM stages where the content is actually
    /// templated into the prompt body.
    ///
    /// Default `None` resolves to **1500** characters (~400 tokens at
    /// the cl100k average) — the same cap Cluster B settled on for the
    /// synthesis prompt cap (PERF-7). The two caps are independent
    /// knobs so operators can tune the synthesis and multistep paths
    /// separately, but the shared default keeps reasoning about prompt
    /// budgets straightforward.
    ///
    /// The truncation only affects what the LLM sees; the helper
    /// payloads, helper-stage inputs, and the caller-visible final
    /// output are untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multistep_max_content_chars: Option<u32>,
}

/// v0.7.x Form 2 — atomisation execution mode. Stored inside
/// [`GovernancePolicy::auto_atomise_mode`].
///
/// The mode interacts with `auto_atomise` (the boolean enable flag)
/// during resolution:
///
/// | `auto_atomise` | `auto_atomise_mode` | Effective behaviour |
/// |----------------|---------------------|---------------------|
/// | `None` / `false` | any              | Off (no atomisation) |
/// | `Some(true)`     | `None`           | Deferred (legacy WT-1-D) |
/// | `Some(true)`     | `Some(Off)`      | Off (explicit disable wins) |
/// | `Some(true)`     | `Some(Deferred)` | Deferred (explicit) |
/// | `Some(true)`     | `Some(Synchronous)` | Synchronous (Form 2 path) |
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutoAtomiseMode {
    /// No atomisation. Equivalent to `auto_atomise = false`.
    Off,
    /// Legacy WT-1-D behaviour: source embedded first, atomiser runs
    /// on a detached worker thread.
    Deferred,
    /// Form 2 alignment: source embed is skipped, atomiser runs
    /// synchronously inside `memory_store`, source is archived with
    /// `atomised_into > 0` before the response returns. Atoms get
    /// their normal embed-on-insert path.
    Synchronous,
}

impl AutoAtomiseMode {
    /// Telemetry label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Deferred => "deferred",
            Self::Synchronous => "synchronous",
        }
    }
}

/// v0.7.0 Cluster-B (issue #767) — per-namespace enum for the
/// Form 1 synthesis-failure policy. See
/// [`GovernancePolicy::synthesis_failure_mode`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisFailureMode {
    /// Default — log + swallow + continue with the legacy dedup-merge
    /// / insert path. Backward-compatible with the v0.7.0 ship.
    #[default]
    FallThrough,
    /// Refuse the write with a typed error so callers observe the
    /// curator outage instead of inheriting silent fallback behaviour.
    BlockWrite,
}

impl SynthesisFailureMode {
    /// Telemetry label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FallThrough => "fall_through",
            Self::BlockWrite => "block_write",
        }
    }
}

/// v0.7.x Form 6 — namespace-policy enum for the
/// `pre_store::auto_classify_kind` substrate hook. See
/// [`GovernancePolicy::auto_classify_kind`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKindAutoClassify {
    /// Substrate quiet — caller-supplied (or default `Observation`)
    /// kind stands. The hook is a zero-cost no-op.
    #[default]
    Off,
    /// Deterministic regex-based heuristics only. No LLM round-trip.
    RegexOnly,
    /// Regex first; if no heuristic fires (or multiple fire with
    /// conflicting verdicts), fall through to a single-shot LLM
    /// classifier. Opt-in only.
    RegexThenLlm,
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

// #880 — `Default` for `GovernancePolicy` is derived now: each
// sub-struct's own `Default` returns "no per-namespace override" so
// every `effective_*` accessor falls through to the compiled-in
// default. The old hand-written impl is preserved verbatim in
// `CorePolicy::default()` (write=Any, promote=Any, delete=Owner,
// approver=Human, inherit=true, max_reflection_depth=None) and the
// six secondary policy structs `Default::default()` to the
// all-`None` shape that pre-#880 callers expected.

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
        // #880 — every sub-struct defaults to "no override" so the
        // bootstrap policy only differs from `Default::default()` by
        // tightening `core.write` to `Owner`.
        Self {
            core: CorePolicy {
                write: GovernanceLevel::Owner,
                ..CorePolicy::default()
            },
            ..Self::default()
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
        self.core.max_reflection_depth.unwrap_or(3)
    }

    /// v0.7.0 QW-1 — resolve the file-backed-export policy. Returns
    /// `false` (substrate stays SQL-canonical) when the namespace has
    /// no explicit override. `Some(true)` opts the namespace into the
    /// deferred filesystem write in the substrate `post_reflect` hook.
    ///
    /// Inheritance is **not** walked here — the caller resolves the
    /// most-specific policy via `resolve_governance_policy` and then
    /// queries this accessor on the result, mirroring how
    /// `effective_max_reflection_depth` is consumed.
    #[must_use]
    pub fn effective_auto_export_reflections_to_filesystem(&self) -> bool {
        self.export
            .auto_export_reflections_to_filesystem
            .unwrap_or(false)
    }

    /// v0.7.0 WT-1-D — resolve the auto-atomisation enable flag.
    /// Returns `false` (substrate stays quiet) when the namespace has
    /// no explicit override. `Some(true)` opts the namespace into the
    /// `pre_store` substrate hook's deferred curator-pass enqueue.
    ///
    /// Inheritance is **not** walked here — the caller resolves the
    /// most-specific policy via `resolve_governance_policy` and then
    /// queries this accessor on the result, mirroring how
    /// `effective_max_reflection_depth` is consumed.
    #[must_use]
    pub fn effective_auto_atomise(&self) -> bool {
        self.atomisation.auto_atomise.unwrap_or(false)
    }

    /// v0.7.0 WT-1-D — resolve the cl100k token threshold above which
    /// the auto-atomisation hook fires. Compiled default is **500**;
    /// matches the WT-1-D brief (memories ≤ 500 tokens are short
    /// enough to live as a single observation).
    #[must_use]
    pub fn effective_auto_atomise_threshold_cl100k(&self) -> u32 {
        self.atomisation
            .auto_atomise_threshold_cl100k
            .unwrap_or(500)
    }

    /// v0.7.0 WT-1-D — resolve the per-atom token budget for the
    /// auto-atomisation curator pass. Compiled default is **200**;
    /// matches `AtomiserConfig::default_max_atom_tokens` so the
    /// hook-driven path produces atoms indistinguishable from
    /// CLI/MCP-driven atomisation.
    #[must_use]
    pub fn effective_auto_atomise_max_atom_tokens(&self) -> u32 {
        self.atomisation.auto_atomise_max_atom_tokens.unwrap_or(200)
    }

    /// v0.7.0 Cluster-F PERF-5 — resolve the Synchronous-mode
    /// curator retry budget. Returns `None` when the namespace has
    /// no explicit override; the caller threads this through
    /// `Atomiser::atomise_sync_with_retries` and falls back to
    /// `AtomiserConfig::sync_curator_max_retries` (compiled default 1)
    /// when `None`. Documented in `docs/atomisation.md` alongside the
    /// Synchronous-mode latency envelope.
    #[must_use]
    pub fn effective_auto_atomise_max_retries(&self) -> Option<u32> {
        self.atomisation.auto_atomise_max_retries
    }

    /// v0.7.0 QW-2 — resolve the auto-persona regeneration cadence.
    /// Returns `None` (cadence disabled) when the namespace has no
    /// explicit override; `Some(N)` opts the namespace into deferred
    /// persona regeneration every N writes against an entity. The
    /// `post_store` hook reads this accessor on the resolved policy
    /// after walking the leaf-first ancestor chain.
    #[must_use]
    pub fn effective_auto_persona_trigger_every_n_memories(&self) -> Option<u32> {
        self.persona.auto_persona_trigger_every_n_memories
    }

    /// v0.7.0 QW-2 — resolve the file-backed-export policy for
    /// Persona-kind memories. Returns `false` (substrate stays
    /// SQL-canonical) when the namespace has no explicit override.
    /// Symmetric with
    /// [`Self::effective_auto_export_reflections_to_filesystem`].
    #[must_use]
    pub fn effective_auto_export_personas_to_filesystem(&self) -> bool {
        self.persona
            .auto_export_personas_to_filesystem
            .unwrap_or(false)
    }

    /// v0.7.x Form 2 — resolve the atomisation execution mode.
    ///
    /// Resolution rules (matches the table on
    /// [`AutoAtomiseMode`]):
    ///
    /// 1. `auto_atomise_mode = Some(mode)` wins — operator explicit.
    /// 2. Otherwise `auto_atomise = Some(true)` → [`AutoAtomiseMode::Deferred`]
    ///    (preserves pre-Form-2 deployments verbatim).
    /// 3. Otherwise [`AutoAtomiseMode::Off`].
    ///
    /// Both `Off` returns and an `Off` explicit override short-circuit
    /// the `pre_store` hook chain entirely.
    #[must_use]
    pub fn effective_auto_atomise_mode(&self) -> AutoAtomiseMode {
        if let Some(m) = self.atomisation.auto_atomise_mode {
            return m;
        }
        if self.atomisation.auto_atomise.unwrap_or(false) {
            AutoAtomiseMode::Deferred
        } else {
            AutoAtomiseMode::Off
        }
    }

    /// v0.7.x Form 1 — resolve the legacy per-pair classifier opt-in.
    /// Returns `false` (default) when absent or `Some(false)`, routing
    /// the substrate through the new single-batch action-emitting
    /// synthesiser. `Some(true)` keeps the legacy per-pair binary
    /// contradiction call (metadata-only outcome) for operators who
    /// depend on the v0.6.x behaviour.
    #[must_use]
    pub fn effective_legacy_per_pair_classifier(&self) -> bool {
        self.synthesis.legacy_per_pair_classifier.unwrap_or(false)
    }

    /// v0.7.0 Cluster-B (issue #767) — resolve the synthesis-failure
    /// policy. Default is [`SynthesisFailureMode::FallThrough`] to
    /// preserve backward compatibility with the v0.7.0 ship behaviour;
    /// operators opt in to [`SynthesisFailureMode::BlockWrite`] per
    /// namespace when a curator outage must be surfaced loudly.
    #[must_use]
    pub fn effective_synthesis_failure_mode(&self) -> SynthesisFailureMode {
        self.synthesis.synthesis_failure_mode.unwrap_or_default()
    }

    /// v0.7.0 Cluster-B (issue #767, SEC-1) — resolve the per-call
    /// delete-cap. Compiled default is **1**: a single LLM round-trip
    /// must not mass-delete a namespace without an explicit K10
    /// approval flow.
    #[must_use]
    pub fn effective_synthesis_max_deletes_per_call(&self) -> u32 {
        self.synthesis.synthesis_max_deletes_per_call.unwrap_or(1)
    }

    /// v0.7.0 Cluster-B (issue #767, PERF-7) — resolve the
    /// per-candidate character cap inlined into the synthesis prompt.
    /// Compiled default is **1500** characters (~400 cl100k tokens).
    /// Truncation only affects the LLM prompt, not the stored row.
    #[must_use]
    pub fn effective_synthesis_max_candidate_chars(&self) -> usize {
        self.synthesis.synthesis_max_candidate_chars.unwrap_or(1500) as usize
    }

    /// v0.7.0 polish (issue #782, PERF-11) — resolve the per-stage
    /// character cap inlined into a Form 3 multistep-ingest LLM-stage
    /// prompt. Compiled default is **1500** characters (~400 cl100k
    /// tokens), matching the synthesis cap (PERF-7) so operators have
    /// a single reasonable prompt-budget shape to reason about.
    /// Truncation only affects the LLM prompt content slot; the
    /// helper payloads (which carry their own preview truncation
    /// inside the helper) and the caller-visible final output are
    /// untouched.
    #[must_use]
    pub fn effective_multistep_max_content_chars(&self) -> usize {
        self.multistep.multistep_max_content_chars.unwrap_or(1500) as usize
    }

    /// #880 — auto-classify-kind accessor, missing in the pre-#880
    /// hand-written impl (callers were reading `policy.kind_class.auto_classify_kind`
    /// directly). Now exposed via a typed accessor so the call sites can
    /// migrate to the sub-struct path without referencing every field
    /// directly.
    #[must_use]
    pub fn effective_auto_classify_kind(&self) -> MemoryKindAutoClassify {
        self.kind_class.auto_classify_kind.unwrap_or_default()
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

// -----------------------------------------------------------------
// v0.7-polish coverage recovery (issue #767) — GovernancePolicy
// effective_* accessor + default-resolution coverage. Covers the
// Form 1/2/4/5/6 + QW-1/QW-2 + Cluster B/F fields and their accessors.
// -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn governance_policy_default_resolves_form_fields_to_none_and_compiled_defaults() {
        let p = GovernancePolicy::default();
        assert_eq!(p.core.write, GovernanceLevel::Any);
        assert_eq!(p.core.promote, GovernanceLevel::Any);
        assert_eq!(p.core.delete, GovernanceLevel::Owner);
        assert_eq!(p.core.approver, ApproverType::Human);
        assert!(p.core.inherit);
        // Every Form / Cluster field defaults to None.
        assert!(p.core.max_reflection_depth.is_none());
        assert!(p.export.auto_export_reflections_to_filesystem.is_none());
        assert!(p.atomisation.auto_atomise.is_none());
        assert!(p.atomisation.auto_atomise_threshold_cl100k.is_none());
        assert!(p.atomisation.auto_atomise_max_atom_tokens.is_none());
        assert!(p.atomisation.auto_atomise_max_retries.is_none());
        assert!(p.persona.auto_persona_trigger_every_n_memories.is_none());
        assert!(p.persona.auto_export_personas_to_filesystem.is_none());
        assert!(p.atomisation.auto_atomise_mode.is_none());
        assert!(p.synthesis.legacy_per_pair_classifier.is_none());
        assert!(p.kind_class.auto_classify_kind.is_none());
        assert!(p.synthesis.synthesis_failure_mode.is_none());
        assert!(p.synthesis.synthesis_max_deletes_per_call.is_none());
        assert!(p.synthesis.synthesis_max_candidate_chars.is_none());
        assert!(p.multistep.multistep_max_content_chars.is_none());
    }

    #[test]
    fn default_for_managed_namespace_tightens_write_to_owner() {
        let p = GovernancePolicy::default_for_managed_namespace();
        assert_eq!(p.core.write, GovernanceLevel::Owner);
        assert!(p.core.inherit);
        // All Form fields remain None — managed namespaces inherit
        // compiled defaults explicitly.
        assert!(p.core.max_reflection_depth.is_none());
        assert!(p.atomisation.auto_atomise.is_none());
        assert!(p.atomisation.auto_atomise_mode.is_none());
        assert!(p.synthesis.synthesis_failure_mode.is_none());
        assert!(p.multistep.multistep_max_content_chars.is_none());
    }

    #[test]
    fn effective_max_reflection_depth_defaults_to_three_when_none() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_max_reflection_depth(), 3);
    }

    #[test]
    fn effective_max_reflection_depth_returns_override_when_set() {
        let mut p = GovernancePolicy::default();
        p.core.max_reflection_depth = Some(7);
        assert_eq!(p.effective_max_reflection_depth(), 7);
    }

    #[test]
    fn effective_max_reflection_depth_returns_zero_kill_switch() {
        let mut p = GovernancePolicy::default();
        p.core.max_reflection_depth = Some(0);
        assert_eq!(p.effective_max_reflection_depth(), 0);
    }

    #[test]
    fn effective_auto_export_reflections_to_filesystem_defaults_false() {
        let p = GovernancePolicy::default();
        assert!(!p.effective_auto_export_reflections_to_filesystem());
    }

    #[test]
    fn effective_auto_export_reflections_to_filesystem_returns_override() {
        let mut p = GovernancePolicy::default();
        p.export.auto_export_reflections_to_filesystem = Some(true);
        assert!(p.effective_auto_export_reflections_to_filesystem());
        p.export.auto_export_reflections_to_filesystem = Some(false);
        assert!(!p.effective_auto_export_reflections_to_filesystem());
    }

    #[test]
    fn effective_auto_atomise_defaults_false() {
        let p = GovernancePolicy::default();
        assert!(!p.effective_auto_atomise());
    }

    #[test]
    fn effective_auto_atomise_returns_override() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise = Some(true);
        assert!(p.effective_auto_atomise());
    }

    #[test]
    fn effective_auto_atomise_threshold_cl100k_defaults_to_500() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_auto_atomise_threshold_cl100k(), 500);
    }

    #[test]
    fn effective_auto_atomise_threshold_cl100k_returns_override() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise_threshold_cl100k = Some(1000);
        assert_eq!(p.effective_auto_atomise_threshold_cl100k(), 1000);
    }

    #[test]
    fn effective_auto_atomise_max_atom_tokens_defaults_to_200() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_auto_atomise_max_atom_tokens(), 200);
    }

    #[test]
    fn effective_auto_atomise_max_atom_tokens_returns_override() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise_max_atom_tokens = Some(50);
        assert_eq!(p.effective_auto_atomise_max_atom_tokens(), 50);
    }

    #[test]
    fn effective_auto_atomise_max_retries_returns_none_by_default() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_auto_atomise_max_retries(), None);
    }

    #[test]
    fn effective_auto_atomise_max_retries_returns_override() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise_max_retries = Some(3);
        assert_eq!(p.effective_auto_atomise_max_retries(), Some(3));
    }

    #[test]
    fn effective_auto_persona_trigger_returns_none_by_default() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_auto_persona_trigger_every_n_memories(), None);
    }

    #[test]
    fn effective_auto_persona_trigger_returns_override() {
        let mut p = GovernancePolicy::default();
        p.persona.auto_persona_trigger_every_n_memories = Some(5);
        assert_eq!(p.effective_auto_persona_trigger_every_n_memories(), Some(5));
    }

    #[test]
    fn effective_auto_export_personas_to_filesystem_defaults_false() {
        let p = GovernancePolicy::default();
        assert!(!p.effective_auto_export_personas_to_filesystem());
    }

    #[test]
    fn effective_auto_export_personas_to_filesystem_returns_override() {
        let mut p = GovernancePolicy::default();
        p.persona.auto_export_personas_to_filesystem = Some(true);
        assert!(p.effective_auto_export_personas_to_filesystem());
    }

    #[test]
    fn effective_auto_atomise_mode_off_when_disabled() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_auto_atomise_mode(), AutoAtomiseMode::Off);
    }

    #[test]
    fn effective_auto_atomise_mode_explicit_off_wins_over_enabled_flag() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise = Some(true);
        p.atomisation.auto_atomise_mode = Some(AutoAtomiseMode::Off);
        assert_eq!(p.effective_auto_atomise_mode(), AutoAtomiseMode::Off);
    }

    #[test]
    fn effective_auto_atomise_mode_legacy_flag_implies_deferred() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise = Some(true);
        // No explicit mode → implicit Deferred (legacy WT-1-D behaviour).
        assert_eq!(p.effective_auto_atomise_mode(), AutoAtomiseMode::Deferred);
    }

    #[test]
    fn effective_auto_atomise_mode_explicit_synchronous() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise = Some(true);
        p.atomisation.auto_atomise_mode = Some(AutoAtomiseMode::Synchronous);
        assert_eq!(
            p.effective_auto_atomise_mode(),
            AutoAtomiseMode::Synchronous
        );
    }

    #[test]
    fn effective_auto_atomise_mode_explicit_deferred_when_flag_absent() {
        let mut p = GovernancePolicy::default();
        p.atomisation.auto_atomise_mode = Some(AutoAtomiseMode::Deferred);
        // Explicit mode wins regardless of the boolean flag.
        assert_eq!(p.effective_auto_atomise_mode(), AutoAtomiseMode::Deferred);
    }

    #[test]
    fn auto_atomise_mode_as_str_labels() {
        assert_eq!(AutoAtomiseMode::Off.as_str(), "off");
        assert_eq!(AutoAtomiseMode::Deferred.as_str(), "deferred");
        assert_eq!(AutoAtomiseMode::Synchronous.as_str(), "synchronous");
    }

    #[test]
    fn effective_legacy_per_pair_classifier_defaults_false() {
        let p = GovernancePolicy::default();
        assert!(!p.effective_legacy_per_pair_classifier());
    }

    #[test]
    fn effective_legacy_per_pair_classifier_returns_override() {
        let mut p = GovernancePolicy::default();
        p.synthesis.legacy_per_pair_classifier = Some(true);
        assert!(p.effective_legacy_per_pair_classifier());
    }

    #[test]
    fn effective_synthesis_failure_mode_defaults_to_fall_through() {
        let p = GovernancePolicy::default();
        assert_eq!(
            p.effective_synthesis_failure_mode(),
            SynthesisFailureMode::FallThrough
        );
    }

    #[test]
    fn effective_synthesis_failure_mode_returns_override() {
        let mut p = GovernancePolicy::default();
        p.synthesis.synthesis_failure_mode = Some(SynthesisFailureMode::BlockWrite);
        assert_eq!(
            p.effective_synthesis_failure_mode(),
            SynthesisFailureMode::BlockWrite
        );
    }

    #[test]
    fn synthesis_failure_mode_as_str_labels() {
        assert_eq!(SynthesisFailureMode::FallThrough.as_str(), "fall_through");
        assert_eq!(SynthesisFailureMode::BlockWrite.as_str(), "block_write");
    }

    #[test]
    fn synthesis_failure_mode_default_is_fall_through() {
        let v: SynthesisFailureMode = SynthesisFailureMode::default();
        assert_eq!(v, SynthesisFailureMode::FallThrough);
    }

    #[test]
    fn effective_synthesis_max_deletes_per_call_defaults_to_one() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_synthesis_max_deletes_per_call(), 1);
    }

    #[test]
    fn effective_synthesis_max_deletes_per_call_returns_override() {
        let mut p = GovernancePolicy::default();
        p.synthesis.synthesis_max_deletes_per_call = Some(8);
        assert_eq!(p.effective_synthesis_max_deletes_per_call(), 8);
    }

    #[test]
    fn effective_synthesis_max_candidate_chars_defaults_to_1500() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_synthesis_max_candidate_chars(), 1500);
    }

    #[test]
    fn effective_synthesis_max_candidate_chars_returns_override() {
        let mut p = GovernancePolicy::default();
        p.synthesis.synthesis_max_candidate_chars = Some(2_500);
        assert_eq!(p.effective_synthesis_max_candidate_chars(), 2_500);
    }

    #[test]
    fn effective_multistep_max_content_chars_defaults_to_1500() {
        let p = GovernancePolicy::default();
        assert_eq!(p.effective_multistep_max_content_chars(), 1500);
    }

    #[test]
    fn effective_multistep_max_content_chars_returns_override() {
        let mut p = GovernancePolicy::default();
        p.multistep.multistep_max_content_chars = Some(3_000);
        assert_eq!(p.effective_multistep_max_content_chars(), 3_000);
    }

    #[test]
    fn memory_kind_auto_classify_default_is_off() {
        let v: MemoryKindAutoClassify = MemoryKindAutoClassify::default();
        assert_eq!(v, MemoryKindAutoClassify::Off);
    }

    #[test]
    fn memory_kind_auto_classify_serde_round_trip() {
        for v in [
            MemoryKindAutoClassify::Off,
            MemoryKindAutoClassify::RegexOnly,
            MemoryKindAutoClassify::RegexThenLlm,
        ] {
            let s = serde_json::to_value(v).unwrap();
            let back: MemoryKindAutoClassify = serde_json::from_value(s).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn auto_atomise_mode_serde_round_trip() {
        for v in [
            AutoAtomiseMode::Off,
            AutoAtomiseMode::Deferred,
            AutoAtomiseMode::Synchronous,
        ] {
            let s = serde_json::to_value(v).unwrap();
            let back: AutoAtomiseMode = serde_json::from_value(s).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn synthesis_failure_mode_serde_round_trip() {
        for v in [
            SynthesisFailureMode::FallThrough,
            SynthesisFailureMode::BlockWrite,
        ] {
            let s = serde_json::to_value(v).unwrap();
            let back: SynthesisFailureMode = serde_json::from_value(s).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn governance_policy_serde_round_trip_with_all_v070_fields() {
        let mut p = GovernancePolicy::default();
        p.core.max_reflection_depth = Some(5);
        p.atomisation.auto_atomise = Some(true);
        p.atomisation.auto_atomise_mode = Some(AutoAtomiseMode::Synchronous);
        p.atomisation.auto_atomise_threshold_cl100k = Some(750);
        p.atomisation.auto_atomise_max_atom_tokens = Some(150);
        p.atomisation.auto_atomise_max_retries = Some(2);
        p.persona.auto_persona_trigger_every_n_memories = Some(10);
        p.persona.auto_export_personas_to_filesystem = Some(true);
        p.export.auto_export_reflections_to_filesystem = Some(true);
        p.synthesis.legacy_per_pair_classifier = Some(false);
        p.kind_class.auto_classify_kind = Some(MemoryKindAutoClassify::RegexOnly);
        p.synthesis.synthesis_failure_mode = Some(SynthesisFailureMode::BlockWrite);
        p.synthesis.synthesis_max_deletes_per_call = Some(4);
        p.synthesis.synthesis_max_candidate_chars = Some(2_000);
        p.multistep.multistep_max_content_chars = Some(3_000);
        let v = serde_json::to_value(&p).unwrap();
        let back: GovernancePolicy = serde_json::from_value(v).unwrap();
        assert_eq!(back.core.max_reflection_depth, Some(5));
        assert_eq!(
            back.atomisation.auto_atomise_mode,
            Some(AutoAtomiseMode::Synchronous)
        );
        assert_eq!(back.atomisation.auto_atomise_threshold_cl100k, Some(750));
        assert_eq!(back.persona.auto_persona_trigger_every_n_memories, Some(10));
        assert_eq!(
            back.synthesis.synthesis_failure_mode,
            Some(SynthesisFailureMode::BlockWrite)
        );
        assert_eq!(back.synthesis.synthesis_max_deletes_per_call, Some(4));
        assert_eq!(back.multistep.multistep_max_content_chars, Some(3_000));
    }

    #[test]
    fn from_metadata_returns_none_when_governance_key_absent() {
        let meta = json!({"unrelated": 42});
        assert!(GovernancePolicy::from_metadata(&meta).is_none());
    }

    #[test]
    fn from_metadata_returns_none_when_governance_key_is_null() {
        let meta = json!({"governance": null});
        assert!(GovernancePolicy::from_metadata(&meta).is_none());
    }

    #[test]
    fn from_metadata_parses_governance_blob() {
        let meta = json!({
            "governance": {
                "write": "owner",
                "max_reflection_depth": 4,
            },
        });
        let parsed = GovernancePolicy::from_metadata(&meta).unwrap().unwrap();
        assert_eq!(parsed.core.write, GovernanceLevel::Owner);
        assert_eq!(parsed.core.max_reflection_depth, Some(4));
    }

    #[test]
    fn from_metadata_propagates_parse_error_for_malformed_payload() {
        let meta = json!({"governance": {"write": 42}});
        let res = GovernancePolicy::from_metadata(&meta).unwrap();
        assert!(res.is_err());
    }
}
