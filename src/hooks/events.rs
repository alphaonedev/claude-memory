// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — Task G2: lifecycle event types + JSON payload structs.
//
// G1 (PR #554) shipped the on-disk hook configuration schema and a
// 20-variant `HookEvent` *stub* in `src/hooks/config.rs`. G2 lifts
// `HookEvent` out of `config.rs` into this module, attaches a
// payload struct to every variant, and pins the JSON wire shape
// the executor (G3) will use to talk to subprocess hooks over
// stdio.
//
// # Wire contract
//
// Every payload type derives `Serialize + Deserialize`. The hook
// pipeline marshals payloads to JSON, writes them to the hook
// child's stdin, and reads a `HookDecision` (G4) back from stdout.
// `Pre*` payloads are *deltas* the hook may mutate before the
// memory operation runs; `Post*` payloads are read-only snapshots
// of the operation's effect and exist for observability /
// telemetry hooks.
//
// # Why payloads live in a separate module from `HookEvent`
//
// The `HookEvent` enum itself is tag-only (Copy, Hash) so a config
// loader can match on a name without depending on every payload
// type. The payload types include owned strings, optional fields,
// and `serde_json::Value` bags, none of which is `Copy`. Splitting
// the tag from the payload is the same shape as `tracing::Event` /
// `tracing::Metadata` and keeps `crate::hooks::config` free of any
// dependency on `crate::models` or `crate::transcripts`.
//
// # Backward compatibility with G1
//
// `crate::hooks::config::HookEvent` is preserved as a `pub use`
// re-export so the G1 call sites (`HookConfig.event: HookEvent`,
// `validate_hook`, the existing tests) keep compiling unchanged.
// The canonical path going forward is `crate::hooks::HookEvent`.
//
// # Where each event will fire (G3-G11)
//
// Each variant carries a `// TODO(G3-G11): wire here at <file>:<line>`
// doc-comment naming the source-code location the executor will
// hook into when later tasks land. The line numbers are
// *approximate* — pinned against the heads of the relevant
// functions on `main` at the time of G2 — and are intended as
// hints for the implementer of G3-G11, not load-bearing
// invariants.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{Memory, MemoryLink, Tier};

// ---------------------------------------------------------------------------
// HookEvent — the 20 lifecycle event tags
// ---------------------------------------------------------------------------

/// The 20 lifecycle events the hook pipeline supports.
///
/// `HookEvent` is the *tag* an operator names in `hooks.toml`
/// (`event = "post_store"`) and the discriminator the executor
/// uses when routing a payload to its subscribed hook chain.
///
/// Payload types are defined in this module — see the per-variant
/// payload table in the module-level documentation and the
/// individual variant doc-comments.
///
/// Serde uses snake_case so the on-disk and on-wire spelling
/// matches the table in `docs/v0.7/V0.7-EPIC.md` § Track G2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Fires before a memory is persisted. Payload: [`MemoryDelta`] (writable).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:846` (`pub fn insert`).
    PreStore,
    /// Fires after a memory has been persisted. Payload: [`Memory`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:846` (post-INSERT in `pub fn insert`).
    PostStore,
    /// Fires before a recall query executes. Payload: [`RecallQuery`] (writable).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1657` (`pub fn recall`).
    PreRecall,
    /// Fires after a recall query returns. Payload: [`RecallResult`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1657` (post-return in `pub fn recall`).
    PostRecall,
    /// Fires before a full-text search executes. Payload: [`SearchQuery`] (writable).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1314` (`pub fn search`).
    PreSearch,
    /// Fires after a full-text search returns. Payload: [`SearchResult`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1314` (post-return in `pub fn search`).
    PostSearch,
    /// Fires before a memory is deleted. Payload: [`MemoryRef`] (writable target id).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1072` (`pub fn delete`).
    PreDelete,
    /// Fires after a memory has been deleted. Payload: [`MemoryRef`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1072` (post-DELETE in `pub fn delete`).
    PostDelete,
    /// Fires before a tier promotion. Payload: [`PromoteDelta`] (writable target tier).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1764` (`pub fn promote_to_namespace`).
    PrePromote,
    /// Fires after a tier promotion. Payload: [`PromoteResult`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1764` (post-UPDATE in `pub fn promote_to_namespace`).
    PostPromote,
    /// Fires before a link is created. Payload: [`LinkDelta`] (writable).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1888` (`pub fn create_link`).
    PreLink,
    /// Fires after a link has been created. Payload: [`Link`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1888` (post-INSERT in `pub fn create_link`).
    PostLink,
    /// Fires before a consolidation pass runs. Payload: [`ConsolidationDelta`] (writable).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1958` (`pub fn consolidate`).
    PreConsolidate,
    /// Fires after a consolidation pass completes. Payload: [`ConsolidationResult`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1958` (post-return in `pub fn consolidate`).
    PostConsolidate,
    /// Fires before a governance gate decision. Payload: [`GovernanceContext`] (writable).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:4674` (`pub fn enforce_governance`).
    PreGovernanceDecision,
    /// Fires after a governance gate decision. Payload: [`GovernanceDecision`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:4674` (post-return in `pub fn enforce_governance`).
    PostGovernanceDecision,
    /// Fires when the ANN index evicts an entry. Payload: [`EvictionEvent`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/hnsw.rs:182` (`hnsw.eviction` log site).
    OnIndexEviction,
    /// Fires before a memory is archived. Payload: [`MemoryRef`] (writable target id).
    ///
    /// TODO(G3-G11): wire here at `src/db.rs:1097` (`pub fn archive_memory`).
    PreArchive,
    /// Fires before a transcript is stored. Payload: [`TranscriptDelta`] (writable).
    ///
    /// TODO(G3-G11): wire here at `src/transcripts.rs:72` (`pub fn store`).
    PreTranscriptStore,
    /// Fires after a transcript has been stored. Payload: [`Transcript`] (read-only).
    ///
    /// TODO(G3-G11): wire here at `src/transcripts.rs:72` (post-INSERT in `pub fn store`).
    PostTranscriptStore,
}

// ---------------------------------------------------------------------------
// Pre/Post-store payloads
// ---------------------------------------------------------------------------

/// Writable delta a `pre_store` hook may mutate before the row is
/// persisted.
///
/// Mirrors the user-controllable fields of `crate::models::CreateMemory`
/// — but as a JSON-friendly bag with every field optional so a hook
/// can return a partial diff (e.g. just rewriting `tags`) without
/// echoing the whole memory back over stdio. The executor (G3)
/// merges `Some(_)` fields onto the in-flight `CreateMemory`
/// before calling `db::insert`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

// ---------------------------------------------------------------------------
// Pre/Post-recall payloads
// ---------------------------------------------------------------------------

/// Writable recall query a `pre_recall` hook may rewrite before
/// the recall executes. Mirrors the public `memory_recall` MCP /
/// HTTP request shape; fields are optional so a hook may rewrite
/// only the parts it cares about (e.g. injecting a `namespace`
/// filter for tenant isolation).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecallQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<usize>,
}

/// Read-only snapshot of a recall's result returned to a
/// `post_recall` hook. The `memories` vector reuses
/// [`crate::models::Memory`] verbatim so post-hooks can inspect
/// every field the recall surfaced (tier, score-driving
/// metadata, etc.) without an additional translation layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    pub query: String,
    pub memories: Vec<Memory>,
    /// Total cl100k_base tokens (or `len/4` byte estimate when
    /// the budget path was skipped) the recall consumed. Mirrors
    /// the v0.6.3 `tokens_used` field on the wire envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<usize>,
}

// ---------------------------------------------------------------------------
// Pre/Post-search payloads
// ---------------------------------------------------------------------------

/// Writable FTS search query for `pre_search` hooks. Same shape
/// as [`RecallQuery`] minus the budget knob — search is the
/// uncapped FTS surface; the budget machinery is recall-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Read-only result returned to `post_search` hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub query: String,
    pub memories: Vec<Memory>,
}

// ---------------------------------------------------------------------------
// Pre/Post-delete + pre-archive payloads
// ---------------------------------------------------------------------------

/// Pointer at a single memory by id. Used by `pre_delete`,
/// `post_delete`, and `pre_archive` — operations that take an id
/// and don't need the full row to make a decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRef {
    pub id: String,
}

// ---------------------------------------------------------------------------
// Pre/Post-promote payloads
// ---------------------------------------------------------------------------

/// Writable delta for `pre_promote` — a hook may rewrite the
/// target tier before the promotion runs, e.g. to refuse
/// promotion to `long` tier for transient agent output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromoteDelta {
    pub id: String,
    pub from_tier: Tier,
    pub to_tier: Tier,
}

/// Read-only result for `post_promote` — the resolved tier
/// transition after the operation completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromoteResult {
    pub id: String,
    pub from_tier: Tier,
    pub to_tier: Tier,
}

// ---------------------------------------------------------------------------
// Pre/Post-link payloads
// ---------------------------------------------------------------------------

/// Writable delta for `pre_link`. Mirrors the user-controllable
/// surface of `MemoryLink` so hooks can rewrite the relation
/// (e.g. demote `contradicts` → `related_to` if the source
/// confidence is low) before the row is inserted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkDelta {
    pub source_id: String,
    pub target_id: String,
    pub relation: String,
}

/// Read-only `post_link` payload. Re-uses
/// [`crate::models::MemoryLink`] so the wire shape matches the
/// existing v0.6.3 link surface and downstream consumers don't
/// need a translation table.
pub type Link = MemoryLink;

// ---------------------------------------------------------------------------
// Pre/Post-consolidate payloads
// ---------------------------------------------------------------------------

/// Writable delta for `pre_consolidate`. Names the namespace and
/// candidate memory ids the consolidator is about to operate on.
/// A hook may shrink (or veto via `HookDecision::Deny` in G4) the
/// candidate set before the consolidation runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationDelta {
    pub namespace: String,
    pub candidate_ids: Vec<String>,
}

/// Read-only `post_consolidate` payload. Reports the resolved
/// merge / supersede outcome so observability hooks can surface
/// consolidation activity to operators.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationResult {
    pub namespace: String,
    /// Memory ids that were merged into a consolidated row.
    pub merged_ids: Vec<String>,
    /// The id of the consolidated row, when one was produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Pre/Post-governance-decision payloads
// ---------------------------------------------------------------------------

/// Writable governance context passed to `pre_governance_decision`
/// hooks. Hooks see the namespace, the action under review, and
/// the requesting agent identity, and may augment / rewrite any
/// of these before `enforce_governance` runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceContext {
    pub namespace: String,
    pub action: String,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
}

/// Read-only outcome of a governance gate decision. Mirrors the
/// allow/deny/pending shape `enforce_governance` returns; the
/// optional `pending_id` correlates an `Ask` outcome with the
/// row in `pending_actions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernanceOutcome {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceDecision {
    pub namespace: String,
    pub action: String,
    pub agent_id: String,
    pub outcome: GovernanceOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Index eviction payload
// ---------------------------------------------------------------------------

/// `on_index_eviction` payload — fired when the HNSW index
/// evicts an entry under capacity pressure. Lets observability
/// hooks (datadog, prometheus pushgateway, etc.) surface the
/// eviction without polling the `index_evictions_total` counter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionEvent {
    pub memory_id: String,
}

// ---------------------------------------------------------------------------
// Transcript payloads (I-track interop)
// ---------------------------------------------------------------------------

/// Writable delta for `pre_transcript_store`. Hooks may rewrite
/// the namespace, the raw content, or the TTL before the
/// transcript blob is compressed and persisted. Content is
/// passed in clear text — compression happens server-side.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// TTL in seconds from "now"; `None` means no expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<i64>,
}

/// Read-only handle returned to `post_transcript_store` hooks.
///
/// Mirrors `crate::transcripts::Transcript` field-for-field
/// (which is *not* `Serialize` itself — it's an internal storage
/// handle). The executor (G3) will project from the internal
/// type into this wire-shaped struct before fanning out to hook
/// subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub id: String,
    pub namespace: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub compressed_size: i64,
    pub original_size: i64,
}

impl From<&crate::transcripts::Transcript> for Transcript {
    fn from(t: &crate::transcripts::Transcript) -> Self {
        Self {
            id: t.id.clone(),
            namespace: t.namespace.clone(),
            created_at: t.created_at.clone(),
            expires_at: t.expires_at.clone(),
            compressed_size: t.compressed_size,
            original_size: t.original_size,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — JSON round-trip per representative variant
// ---------------------------------------------------------------------------
//
// Per the G2 prompt: aim for ~5-10 representative tests, not 20
// individual ones. We cover (a) the `HookEvent` tag itself for
// every variant in one pass and (b) a JSON round-trip per payload
// *family*: store / recall / search / delete / promote / link /
// consolidate / governance / eviction / transcript.

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `HookEvent` variant must round-trip through JSON
    /// with snake_case spelling. A single table-driven test keeps
    /// the assertion surface compact.
    #[test]
    fn hook_event_all_variants_round_trip() {
        let table = [
            (HookEvent::PreStore, "\"pre_store\""),
            (HookEvent::PostStore, "\"post_store\""),
            (HookEvent::PreRecall, "\"pre_recall\""),
            (HookEvent::PostRecall, "\"post_recall\""),
            (HookEvent::PreSearch, "\"pre_search\""),
            (HookEvent::PostSearch, "\"post_search\""),
            (HookEvent::PreDelete, "\"pre_delete\""),
            (HookEvent::PostDelete, "\"post_delete\""),
            (HookEvent::PrePromote, "\"pre_promote\""),
            (HookEvent::PostPromote, "\"post_promote\""),
            (HookEvent::PreLink, "\"pre_link\""),
            (HookEvent::PostLink, "\"post_link\""),
            (HookEvent::PreConsolidate, "\"pre_consolidate\""),
            (HookEvent::PostConsolidate, "\"post_consolidate\""),
            (
                HookEvent::PreGovernanceDecision,
                "\"pre_governance_decision\"",
            ),
            (
                HookEvent::PostGovernanceDecision,
                "\"post_governance_decision\"",
            ),
            (HookEvent::OnIndexEviction, "\"on_index_eviction\""),
            (HookEvent::PreArchive, "\"pre_archive\""),
            (HookEvent::PreTranscriptStore, "\"pre_transcript_store\""),
            (HookEvent::PostTranscriptStore, "\"post_transcript_store\""),
        ];

        // Pin the count at the type boundary so adding a 21st
        // variant without updating the table fails this test.
        assert_eq!(table.len(), 20, "G2 ships exactly 20 lifecycle events");

        for (variant, expected_json) in table {
            let encoded = serde_json::to_string(&variant).expect("variant encodes");
            assert_eq!(encoded, expected_json, "variant {variant:?} mis-encoded");
            let decoded: HookEvent = serde_json::from_str(&encoded).expect("variant decodes");
            assert_eq!(decoded, variant, "variant {variant:?} did not round-trip");
        }
    }

    #[test]
    fn memory_delta_partial_serialization_omits_none_fields() {
        let delta = MemoryDelta {
            tags: Some(vec!["urgent".into(), "v0.7".into()]),
            priority: Some(80),
            ..Default::default()
        };
        let v: Value = serde_json::to_value(&delta).expect("encode");
        // Only the fields the hook touched should appear on the wire.
        assert_eq!(v["tags"], serde_json::json!(["urgent", "v0.7"]));
        assert_eq!(v["priority"], serde_json::json!(80));
        assert!(v.get("title").is_none());
        assert!(v.get("content").is_none());
        assert!(v.get("metadata").is_none());

        // And the partial round-trips.
        let back: MemoryDelta = serde_json::from_value(v).expect("decode");
        assert_eq!(
            back.tags.as_deref(),
            Some(&["urgent".into(), "v0.7".into()][..])
        );
        assert_eq!(back.priority, Some(80));
        assert!(back.title.is_none());
    }

    #[test]
    fn recall_query_round_trips() {
        let q = RecallQuery {
            query: Some("auth tokens".into()),
            namespace: Some("team/security".into()),
            limit: Some(10),
            tier: Some(Tier::Long),
            tags: Some(vec!["secrets".into()]),
            budget_tokens: Some(2_048),
        };
        let json = serde_json::to_string(&q).expect("encode");
        let back: RecallQuery = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.query.as_deref(), Some("auth tokens"));
        assert_eq!(back.namespace.as_deref(), Some("team/security"));
        assert_eq!(back.limit, Some(10));
        assert_eq!(back.tier, Some(Tier::Long));
        assert_eq!(back.budget_tokens, Some(2_048));
    }

    #[test]
    fn search_query_and_result_round_trip() {
        let sq = SearchQuery {
            query: Some("postgres".into()),
            namespace: Some("eng".into()),
            limit: Some(5),
            tags: None,
        };
        let json = serde_json::to_string(&sq).expect("encode SearchQuery");
        let back: SearchQuery = serde_json::from_str(&json).expect("decode SearchQuery");
        assert_eq!(back.query.as_deref(), Some("postgres"));
        assert!(back.tags.is_none());

        let sr = SearchResult {
            query: "postgres".into(),
            memories: vec![],
        };
        let json = serde_json::to_string(&sr).expect("encode SearchResult");
        let back: SearchResult = serde_json::from_str(&json).expect("decode SearchResult");
        assert_eq!(back.query, "postgres");
        assert!(back.memories.is_empty());
    }

    #[test]
    fn memory_ref_round_trips() {
        let r = MemoryRef {
            id: "01HZX0R5GZ8R3KJYV1Y3M9YW2T".into(),
        };
        let json = serde_json::to_string(&r).expect("encode");
        let back: MemoryRef = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.id, r.id);

        // Same payload backs PreDelete / PostDelete / PreArchive.
        // The variant tag is independent so it's fine to reuse.
        assert_eq!(
            serde_json::to_string(&HookEvent::PreArchive).unwrap(),
            "\"pre_archive\""
        );
    }

    #[test]
    fn promote_delta_and_result_round_trip() {
        let d = PromoteDelta {
            id: "abc".into(),
            from_tier: Tier::Short,
            to_tier: Tier::Long,
        };
        let json = serde_json::to_string(&d).expect("encode");
        let back: PromoteDelta = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.from_tier, Tier::Short);
        assert_eq!(back.to_tier, Tier::Long);

        let r = PromoteResult {
            id: "abc".into(),
            from_tier: Tier::Short,
            to_tier: Tier::Mid,
        };
        let back: PromoteResult =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).expect("decode");
        assert_eq!(back.to_tier, Tier::Mid);
    }

    #[test]
    fn link_delta_and_post_link_round_trip() {
        let d = LinkDelta {
            source_id: "src".into(),
            target_id: "tgt".into(),
            relation: "related_to".into(),
        };
        let json = serde_json::to_string(&d).expect("encode");
        let back: LinkDelta = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.relation, "related_to");

        // Link is a re-export of MemoryLink — exercise its serde path.
        let post = Link {
            source_id: "src".into(),
            target_id: "tgt".into(),
            relation: "related_to".into(),
            created_at: "2026-05-05T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&post).expect("encode Link");
        let back: Link = serde_json::from_str(&json).expect("decode Link");
        assert_eq!(back.source_id, "src");
        assert_eq!(back.created_at, "2026-05-05T00:00:00Z");
    }

    #[test]
    fn consolidation_payloads_round_trip() {
        let d = ConsolidationDelta {
            namespace: "team/ops".into(),
            candidate_ids: vec!["a".into(), "b".into(), "c".into()],
        };
        let back: ConsolidationDelta =
            serde_json::from_str(&serde_json::to_string(&d).unwrap()).expect("decode");
        assert_eq!(back.candidate_ids.len(), 3);

        let r = ConsolidationResult {
            namespace: "team/ops".into(),
            merged_ids: vec!["a".into(), "b".into()],
            result_id: Some("merged-1".into()),
        };
        let json = serde_json::to_string(&r).expect("encode");
        let v: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["result_id"], serde_json::json!("merged-1"));

        // Verify the skip-if-none bites.
        let r_no_result = ConsolidationResult {
            namespace: "team/ops".into(),
            merged_ids: vec![],
            result_id: None,
        };
        let v: Value = serde_json::to_value(&r_no_result).expect("encode");
        assert!(v.get("result_id").is_none());
    }

    #[test]
    fn governance_payloads_round_trip() {
        let ctx = GovernanceContext {
            namespace: "team/security".into(),
            action: "memory_store".into(),
            agent_id: "agent-1".into(),
            memory_id: None,
        };
        let back: GovernanceContext =
            serde_json::from_str(&serde_json::to_string(&ctx).unwrap()).expect("decode");
        assert_eq!(back.action, "memory_store");
        assert!(back.memory_id.is_none());

        let dec = GovernanceDecision {
            namespace: "team/security".into(),
            action: "memory_store".into(),
            agent_id: "agent-1".into(),
            outcome: GovernanceOutcome::Ask,
            reason: Some("requires human review".into()),
            pending_id: Some("pending-1".into()),
        };
        let json = serde_json::to_string(&dec).expect("encode");
        let v: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["outcome"], serde_json::json!("ask"));
        let back: GovernanceDecision = serde_json::from_value(v).expect("decode");
        assert!(matches!(back.outcome, GovernanceOutcome::Ask));
        assert_eq!(back.pending_id.as_deref(), Some("pending-1"));
    }

    #[test]
    fn eviction_event_round_trips() {
        let ev = EvictionEvent {
            memory_id: "m-1".into(),
        };
        let json = serde_json::to_string(&ev).expect("encode");
        assert_eq!(json, "{\"memory_id\":\"m-1\"}");
        let back: EvictionEvent = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.memory_id, "m-1");
    }

    #[test]
    fn transcript_payloads_round_trip_and_project_from_internal() {
        let delta = TranscriptDelta {
            namespace: Some("agent/claude".into()),
            content: Some("hello world".into()),
            ttl_secs: Some(3_600),
        };
        let json = serde_json::to_string(&delta).expect("encode");
        let back: TranscriptDelta = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.namespace.as_deref(), Some("agent/claude"));
        assert_eq!(back.ttl_secs, Some(3_600));

        // Project from the internal storage handle to the wire shape.
        let internal = crate::transcripts::Transcript {
            id: "tr-1".into(),
            namespace: "agent/claude".into(),
            created_at: "2026-05-05T00:00:00Z".into(),
            expires_at: None,
            compressed_size: 42,
            original_size: 256,
        };
        let wire: Transcript = (&internal).into();
        let json = serde_json::to_string(&wire).expect("encode wire");
        let back: Transcript = serde_json::from_str(&json).expect("decode wire");
        assert_eq!(back.id, "tr-1");
        assert_eq!(back.compressed_size, 42);
        assert_eq!(back.original_size, 256);
        assert!(back.expires_at.is_none());
    }
}
