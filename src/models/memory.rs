// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::default_metadata;

/// L1-1 (v0.7.0) — typed memory-kind discriminator stored in the
/// `memories.memory_kind` column (schema v30).
///
/// `Observation` and `Reflection` exist since v0.7.0. `Persona`
/// landed in v0.7.0 QW-2 (schema v36) as the substrate-native
/// Tencent-pattern L3 persona artefact.
///
/// v0.7.x Form 6 (issue #759) — Batman taxonomy extension. The
/// `Concept | Entity | Claim | Relation | Event | Conversation |
/// Decision` variants give downstream readers a richer atom-type
/// vocabulary aligned with the Batman framework's exemplar
/// (Tolaria's frontmatter-as-type schema). All seven variants
/// serialize as snake_case strings via the existing
/// `memory_kind TEXT` column — no schema migration is required
/// because the column has no CHECK constraint. Old rows with no
/// kind read as `Observation` (the SQL `DEFAULT 'observation'`).
/// A future-schema variant a binary doesn't recognise reads as
/// `Observation` via the `unwrap_or_default()` chain in
/// `row_to_memory` (forward-compat).
///
/// `Observation` is the default for every memory created before v30 (the
/// `DEFAULT 'observation'` SQL column handles the backfill contract for
/// rows that pre-date the migration; new inserts that omit the field also
/// land at `Observation`). `Reflection` is set by the `memory_reflect`
/// write path in addition to the existing `metadata.type='reflection'`
/// back-compat marker. `Persona` is set by the QW-2
/// `PersonaGenerator` and the `memory_persona_generate` MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Default — a direct observation or note from the caller.
    #[default]
    Observation,
    /// A memory synthesised by the reflection pass over lower-depth
    /// peers (set by `memory_reflect` and the curator reflection pass).
    Reflection,
    /// v0.7.0 QW-2 — Persona-as-artifact. A curator-generated
    /// Markdown profile summarising an entity, derived from a
    /// cluster of Reflection-kind memories about that entity. The
    /// `entity_id` + `persona_version` columns on `memories` are
    /// populated only for this variant.
    Persona,
    /// v0.7.x Form 6 — abstract definition / vocabulary term
    /// ("ownership is a Rust borrow-checker rule").
    Concept,
    /// v0.7.x Form 6 — named real-world thing (person, org, product,
    /// system component). Pairs with `entity_id` on the row when the
    /// caller has registered the entity in the KG.
    Entity,
    /// v0.7.x Form 6 — factual assertion the caller is recording
    /// ("the build broke at 14:32 UTC"). Distinct from
    /// `Observation` in that a `Claim` is a propositional commitment;
    /// a `Reflection` chain may agree or contradict it.
    Claim,
    /// v0.7.x Form 6 — typed pair / triple. Anchors a KG relation
    /// inside the memory substrate so an operator can query the
    /// relation set with the same recall pipeline used for free-text.
    Relation,
    /// v0.7.x Form 6 — temporally-bounded happening
    /// ("deploy at 09:00", "incident at 14:32"). Distinct from
    /// `Observation` only when the caller wants the
    /// downstream-filtering surface to separate "what I saw" from
    /// "what happened".
    Event,
    /// v0.7.x Form 6 — captured dialogue turn (the substrate also
    /// stores conversations as `Observation`-kind today; this kind
    /// makes the type explicit for callers that want to filter to
    /// just conversational atoms).
    Conversation,
    /// v0.7.x Form 6 (L1-6 reservation) — choice point with
    /// rationale. Distinct from `Reflection` in that a `Decision`
    /// commits to a course of action; reflections summarise. The
    /// L1-6 work (v0.8.0) will likely add columns for
    /// rationale / alternatives, but the variant lands now so
    /// callers can start typing decisions.
    Decision,
}

impl MemoryKind {
    /// Column-wire string (matches the SQL `DEFAULT 'observation'` value).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Reflection => "reflection",
            Self::Persona => "persona",
            Self::Concept => "concept",
            Self::Entity => "entity",
            Self::Claim => "claim",
            Self::Relation => "relation",
            Self::Event => "event",
            Self::Conversation => "conversation",
            Self::Decision => "decision",
        }
    }

    /// Parse the column-wire string. Returns `None` on unrecognised values
    /// so callers can fall back to `Observation` (forward-compat with
    /// future variants that land in a newer DB on an older binary).
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "observation" => Some(Self::Observation),
            "reflection" => Some(Self::Reflection),
            "persona" => Some(Self::Persona),
            "concept" => Some(Self::Concept),
            "entity" => Some(Self::Entity),
            "claim" => Some(Self::Claim),
            "relation" => Some(Self::Relation),
            "event" => Some(Self::Event),
            "conversation" => Some(Self::Conversation),
            "decision" => Some(Self::Decision),
            _ => None,
        }
    }

    /// Enumerate every variant in declaration order. Used by the
    /// capabilities surface (Form 6 `CapabilityMemoryKindVocab`) and
    /// by the recall filter parser when the caller passes `"all"`.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Observation,
            Self::Reflection,
            Self::Persona,
            Self::Concept,
            Self::Entity,
            Self::Claim,
            Self::Relation,
            Self::Event,
            Self::Conversation,
            Self::Decision,
        ]
    }

    /// v0.7.x Form 6 — parse a comma-separated list of kind names
    /// into a deduplicated `Vec<MemoryKind>`. Returns `None` when the
    /// input is empty (after trim) so callers can treat "no filter"
    /// distinctly from "empty filter list ⇒ nothing matches". Unknown
    /// tokens are skipped silently so a future variant emitted by a
    /// newer client doesn't break recall on an older binary.
    #[must_use]
    pub fn parse_csv(s: &str) -> Option<Vec<Self>> {
        let mut out: Vec<Self> = Vec::new();
        for tok in s.split(',') {
            let t = tok.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(k) = Self::from_str(t)
                && !out.contains(&k)
            {
                out.push(k);
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

impl std::fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
    /// L1-1 (v0.7.0) — typed memory-kind discriminator.  Stored in
    /// `memories.memory_kind TEXT NOT NULL DEFAULT 'observation'` (schema v30).
    /// `Observation` for every pre-v30 row (SQL default); `Reflection` for
    /// memories minted by `memory_reflect` or the curator reflection pass.
    ///
    /// `#[serde(default)]` ensures round-trips with pre-v30 federation peers
    /// that don't yet emit the field.
    #[serde(default)]
    pub memory_kind: MemoryKind,
    /// v0.7.0 QW-2 — populated only when `memory_kind == Persona`.
    /// Identifies the subject of the persona. Stored on the SQL
    /// column `memories.entity_id TEXT NULL` (schema v36).
    /// `skip_serializing_if = "Option::is_none"` keeps the absent
    /// shape on the wire for pre-QW-2 federation peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// v0.7.0 QW-2 — monotonic per-(entity_id, namespace) version
    /// counter for the Persona artefact. Populated only when
    /// `memory_kind == Persona`. Each `PersonaGenerator::generate`
    /// call writes a new row with `version + 1`; older rows stay
    /// queryable for audit / rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_version: Option<i32>,
    /// v0.7.0 Form 4 (issue #757) — fact-provenance citations array.
    /// Each entry carries a typed [`Citation`] envelope (uri,
    /// accessed_at, optional hash, optional span). Stored on the
    /// `memories.citations` TEXT column (schema v38) as a JSON-encoded
    /// array — legacy rows default to an empty vector via the SQL
    /// `DEFAULT '[]'` clause and the serde default below. Validator
    /// surface lives at `crate::validate::validate_citation`.
    #[serde(default)]
    pub citations: Vec<Citation>,
    /// v0.7.0 Form 4 (issue #757) — first-class URI-form pointer to
    /// the cited source body. Distinct from the role-label `source`
    /// column. Accepted schemes: `uri:` (HTTP URL), `doc:` (substrate
    /// doc id), `file:` (filesystem path). Validator surface lives at
    /// `crate::validate::validate_source_uri`. Mapped onto the
    /// `memories.source_uri` TEXT column (schema v38). NULL on legacy
    /// rows and on rows that do not yet carry a URI form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// v0.7.0 Form 4 (issue #757) — byte-range into the parent source
    /// body. Populated by the WT-1-B atomisation writer for each atom
    /// (atom-grain span fact-provenance) and may be set by callers
    /// who can pin the offset of a memory inside its referenced
    /// source. Mapped onto the `memories.source_span` TEXT column
    /// (schema v38) as a JSON `{start, end}` envelope. Validator
    /// surface lives at `crate::validate::validate_source_span`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_span: Option<SourceSpan>,
}

/// v0.7.0 Form 4 (issue #757) — fact-provenance citation envelope.
///
/// One entry inside `Memory::citations`. The shape mirrors common
/// scholarly-citation needs while staying substrate-friendly:
///
/// * `uri` — URL, `doc:<id>` substrate pointer, or `file:<path>`. The
///   validator (`crate::validate::validate_citation`) rejects bare
///   strings; callers must use one of the typed schemes.
/// * `accessed_at` — RFC3339 timestamp at which the cited source was
///   read by the agent. Captures the fact-grain "when did this claim
///   become known to me" datum.
/// * `hash` — optional SHA-256 of the cited content. Lets a downstream
///   verifier confirm the source has not drifted since capture.
/// * `span` — optional byte-range pinning the specific quote inside
///   the cited body. Composes with `Memory::source_span` for
///   atom-grain lineage (the parent's span points into the source,
///   the atom's `source_span` points into the parent's body).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Citation {
    pub uri: String,
    pub accessed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// v0.7.0 Form 4 (issue #757) — byte-range envelope used by
/// `Memory::source_span` and `Citation::span`.
///
/// `start` and `end` are zero-based byte offsets into the parent
/// body. The half-open convention `[start, end)` matches Rust's
/// slice semantics, so the cited slice is `body[start..end]`. The
/// validator (`crate::validate::validate_source_span`) requires
/// `start < end` and bounds both within `usize::MAX`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
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
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
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
    /// v0.7.0 Form 4 (issue #757) — fact-provenance citations
    /// supplied at write time. Each entry must satisfy
    /// `validate::validate_citation`. Empty by default.
    #[serde(default)]
    pub citations: Vec<Citation>,
    /// v0.7.0 Form 4 — optional URI-form pointer to the cited source
    /// body. Must satisfy `validate::validate_source_uri` when set.
    #[serde(default)]
    pub source_uri: Option<String>,
    /// v0.7.0 Form 4 — optional byte-range into the parent source
    /// body. Must satisfy `validate::validate_source_span` when set.
    #[serde(default)]
    pub source_span: Option<SourceSpan>,
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
    /// v0.7.0 Form 4 (issue #757) — restrict to memories whose
    /// `citations` array is non-empty. Composes with the other
    /// filters; default `None` preserves v0.7.0 recall semantics.
    #[serde(default)]
    pub has_citations: Option<bool>,
    /// v0.7.0 Form 4 (issue #757) — restrict to memories whose
    /// `source_uri` column begins with this exact prefix.
    #[serde(default)]
    pub source_uri_prefix: Option<String>,
    /// v0.7.x Form 6 (issue #759) — Batman-taxonomy memory-kind
    /// filter. Comma-separated string (`kinds=concept,claim`).
    /// OR-of-kinds within the param; AND with namespace / tags /
    /// time-window / visibility. `None` (default) preserves the
    /// pre-Form-6 "no kind filter" semantics. Unknown tokens are
    /// silently dropped (forward-compat with future variants).
    #[serde(default)]
    pub kinds: Option<String>,
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
    /// v0.7.0 Form 4 (issue #757) — restrict to memories whose
    /// `citations` array is non-empty. Composes with the other
    /// filters.
    #[serde(default)]
    pub has_citations: Option<bool>,
    /// v0.7.0 Form 4 (issue #757) — restrict to memories whose
    /// `source_uri` column begins with this exact prefix.
    #[serde(default)]
    pub source_uri_prefix: Option<String>,
    /// v0.7.x Form 6 (issue #759) — Batman-taxonomy memory-kind
    /// filter. Accepts either a JSON array of strings
    /// (`{"kinds": ["concept", "claim"]}`) or a comma-separated
    /// string (`{"kinds": "concept,claim"}`). OR-of-kinds within
    /// the param; AND with the other filters.
    #[serde(default)]
    pub kinds: Option<serde_json::Value>,
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

    /// v0.7.x Form 6 — parse the optional `kinds` JSON field.
    /// Accepts a JSON array of strings or a single comma-separated
    /// string. Treats `"all"` as "no filter" (returns `None`).
    /// Drops unknown tokens silently.
    #[must_use]
    pub fn resolved_kinds(&self) -> Option<Vec<MemoryKind>> {
        let raw = self.kinds.as_ref()?;
        if let Some(s) = raw.as_str() {
            if s.trim().eq_ignore_ascii_case("all") {
                return None;
            }
            return MemoryKind::parse_csv(s);
        }
        if let Some(arr) = raw.as_array() {
            let mut out: Vec<MemoryKind> = Vec::new();
            for v in arr {
                if let Some(name) = v.as_str()
                    && let Some(k) = MemoryKind::from_str(name.trim())
                    && !out.contains(&k)
                {
                    out.push(k);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        } else {
            None
        }
    }
}

impl RecallQuery {
    /// v0.7.x Form 6 — parse the optional `kinds` query string.
    /// Comma-separated. `"all"` (case-insensitive) is treated as "no
    /// filter" (returns `None`). Drops unknown tokens silently.
    #[must_use]
    pub fn resolved_kinds(&self) -> Option<Vec<MemoryKind>> {
        let s = self.kinds.as_deref()?;
        if s.trim().eq_ignore_ascii_case("all") {
            return None;
        }
        MemoryKind::parse_csv(s)
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
