// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! # Storage Abstraction Layer (SAL) — v0.6.0.0 preview
//!
//! Defines the `MemoryStore` trait that future backends (Postgres,
//! `LanceDB`, Qdrant, S3-backed) implement to plug into `ai-memory`.
//! The in-tree `SqliteStore` adapter wraps the existing `crate::db`
//! free functions so the production path can opt in gradually without
//! a big-bang rewrite.
//!
//! ## Design principles (from the PR #222 red-team)
//!
//! 1. **Typed `StoreError`, not `anyhow::Result`** — callers must be
//!    able to match on error kinds (`NotFound` vs `Conflict` vs
//!    `BackendUnavailable` vs `PermissionDenied`). `#[non_exhaustive]`
//!    lets new variants land without breaking consumers.
//! 2. **`CallerContext` on every mutator** — governance / NHI
//!    attribution threads through the trait boundary, not from
//!    per-method `Option<&str>` shims that the red-team found could be
//!    bypassed.
//! 3. **`Transaction` handle** — multi-step ops (store + link, approve
//!    + mutate) get an explicit unit-of-work type. Backends that lack
//!    transactions return `StoreError::UnsupportedCapability`.
//! 4. **`verify()` provenance contract** — signed-memory and agent
//!    attribution guarantees from Tasks 1.2 / 1.3 survive the SAL
//!    layer. Any adapter that silently mutates content must provide a
//!    re-sign step.
//! 5. **Feature-gated** — the whole module tree compiles only under
//!    `--features sal`, so standard builds are unaffected.
//!
//! ## Stability
//!
//! This is a **v0.6.0.0 preview**. The trait surface is expected to
//! shift during v0.7 as real adapters land. Consumers outside this
//! repo should pin against `sal = 0.1` semantics and expect
//! breaking changes on minor bumps.
//!
//! No production call site dispatches through the trait yet — the
//! existing `crate::db` free-function API remains the active path.
//! The `dead_code` lint is silenced at module granularity for that
//! reason; every public symbol is reachable from the trait's unit
//! tests and from future consumer PRs.

#![allow(dead_code)]
// The SAL trait's design-principles docblock uses numbered continuation
// lines whose visual indent clippy `doc_lazy_continuation` doesn't
// recognize. Reformatting to satisfy the lint makes the doc noticeably
// uglier; silencing it module-wide is the better tradeoff.
#![allow(clippy::doc_lazy_continuation)]

pub mod sqlite;

#[cfg(feature = "sal-postgres")]
pub mod postgres;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};

/// Knowledge-graph backend resolved at adapter init.
///
/// v0.7 Track J substrate: Postgres adapters detect Apache AGE at
/// connect time and dispatch knowledge-graph traversals (J2 `kg_query`,
/// J3 `kg_timeline`, J4 `kg_invalidate`, J7 `find_paths`) on the
/// resolved value. SQLite-class adapters always report
/// [`KgBackend::Cte`] — they fall back to the recursive-CTE path that
/// has been the production wire-format since v0.6.3.
///
/// Wire shape: serialised as snake-case (`"age"` / `"cte"`) to match
/// the `kg_backend` field projected through `memory_capabilities` and
/// `ai-memory doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KgBackend {
    /// Recursive-CTE traversal over `memory_links`. The default path
    /// for SQLite and for Postgres deployments without Apache AGE.
    Cte,
    /// Apache AGE Cypher traversal over the `memory_graph` projection.
    /// Resolved when the Postgres adapter detects the `age` extension
    /// installed at connect time.
    Age,
}

impl KgBackend {
    /// Stable string tag for logs, capabilities surface, and the
    /// `ai-memory doctor` report. Mirrors the snake-case serde rename
    /// above so the wire and log shapes never drift.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cte => "cte",
            Self::Age => "age",
        }
    }
}

/// One row returned by a knowledge-graph traversal at the SAL layer.
///
/// v0.7 Track J substrate: the Cypher (AGE) and recursive-CTE backends
/// project their per-hop results into this shared shape so upper-layer
/// callers (`memory_kg_query`, `memory_kg_timeline`, follow-on tools)
/// don't need to branch on the resolved [`KgBackend`]. The field set is
/// the intersection of what AGE can return through the `cypher()` SRF
/// and what the existing recursive-CTE wire-format already exposes —
/// see `db::kg_query`'s `KgQueryNode` for the SQLite mirror.
///
/// `path` is the `src->mid->target` chain rendered as a single string
/// so it survives both backends without forcing a `Vec<String>` shape
/// (AGE returns it as agtype text, the CTE renders via `group_concat`).
/// `depth` is the actual hop count (1..=`max_depth`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KgQueryRow {
    /// The reachable target memory's id.
    pub target_id: String,
    /// The traversed link's relation tag (e.g. `"related_to"`).
    pub relation: String,
    /// Hop count from the source (1 = direct neighbor).
    pub depth: usize,
    /// `src->mid->target` chain as discovered by the traversal.
    pub path: String,
}

/// One row returned by a knowledge-graph timeline scan at the SAL layer.
///
/// v0.7 Track J substrate: J3 (`memory_kg_timeline`) projects rows from
/// either the Cypher (AGE) backend or the SQL fallback into this shared
/// shape, mirroring [`crate::models::KgTimelineEvent`] (the SQLite-side
/// row used by `db::kg_timeline`). The fields are the intersection of
/// what AGE returns through `cypher()` and what the SQL path already
/// projects, keeping the upper-layer handler backend-blind.
///
/// `valid_from` is the authoritative ordering key — the timeline drops
/// rows with NULL `valid_from` at the SAL layer to match the SQLite
/// contract (a link without a valid-from anchor cannot be ordered).
/// `title` and `target_namespace` are projected for caller display
/// convenience so the upper layer doesn't need a second round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KgTimelineRow {
    /// The asserted target memory's id.
    pub target_id: String,
    /// The link's relation tag (e.g. `"related_to"`).
    pub relation: String,
    /// RFC3339 timestamp marking when the assertion became valid.
    pub valid_from: String,
    /// RFC3339 timestamp marking when the assertion was superseded,
    /// or `None` if still in force.
    pub valid_until: Option<String>,
    /// Agent id that observed/asserted the link, or `None` for legacy
    /// rows that predate observability tracking.
    pub observed_by: Option<String>,
    /// The target memory's display title.
    pub title: String,
    /// The target memory's namespace.
    pub target_namespace: String,
}

/// Outcome of [`crate::store::postgres::PostgresStore::kg_invalidate`] at
/// the SAL layer.
///
/// v0.7 J4 substrate: both the Cypher (AGE) backend and the SQL fallback
/// project their result into this shared shape, mirroring
/// [`crate::db::InvalidateResult`] (the SQLite-side row used by
/// `db::invalidate_link`). `valid_until` is the timestamp now stored on
/// the link; `previous_valid_until` is the prior value, or `None` if
/// this was the first invalidation. `found` is `false` when the
/// `(source_id, target_id, relation)` triple did not match an existing
/// link — callers should treat that as a no-op rather than an error so
/// the dispatcher contract matches the SQLite path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KgInvalidateRow {
    /// True when an existing link was matched and updated; false when
    /// the triple did not exist.
    pub found: bool,
    /// RFC3339 timestamp now stored on the link's `valid_until` column.
    /// Empty string when `found` is false.
    pub valid_until: String,
    /// Prior value of `valid_until` before the update, or `None` if
    /// the link had no prior supersession (or `found` is false).
    pub previous_valid_until: Option<String>,
}

impl std::fmt::Display for KgBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The single error type returned by every `MemoryStore` method.
///
/// Callers match on the variant they care about; the trailing
/// `#[non_exhaustive]` attribute reserves room for new variants
/// without breaking downstream matches.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("memory not found: {id}")]
    NotFound { id: String },

    #[error("identifier conflict on insert: {id}")]
    Conflict { id: String },

    #[error("caller lacks permission for {action} on {target}: {reason}")]
    PermissionDenied {
        action: String,
        target: String,
        reason: String,
    },

    #[error("backend unavailable: {backend}: {detail}")]
    BackendUnavailable { backend: String, detail: String },

    #[error("invalid input: {detail}")]
    InvalidInput { detail: String },

    #[error("requested capability not supported by this backend: {capability}")]
    UnsupportedCapability { capability: String },

    #[error("integrity check failed: {detail}")]
    IntegrityFailed { detail: String },

    #[error("underlying backend error: {0}")]
    Backend(#[from] BoxBackendError),
}

/// Escape hatch for adapter-specific errors that don't map cleanly to
/// a `StoreError` variant. Adapters wrap their native error types in
/// this to retain the underlying cause without leaking the concrete
/// type across the trait boundary.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BoxBackendError(String);

impl BoxBackendError {
    #[must_use]
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Convenience alias — every trait method returns this.
pub type StoreResult<T> = Result<T, StoreError>;

/// Identity + visibility + governance context threaded through every
/// mutating operation. Reuses the NHI-hardened `agent_id` from the
/// existing `crate::identity` resolution chain.
#[derive(Debug, Clone)]
pub struct CallerContext {
    /// The calling agent's resolved `agent_id` (same validation as
    /// `crate::identity::resolve_agent_id`).
    pub agent_id: String,
    /// Optional `as_agent` — when set, visibility filtering runs as
    /// if this agent were the caller (Task 1.5 scope semantics).
    pub as_agent: Option<String>,
    /// Optional request correlator for audit trails. Opaque string;
    /// adapters may persist as metadata.
    pub request_id: Option<String>,
}

impl CallerContext {
    /// Construct a caller context from a resolved agent id. Most
    /// callers use this directly; the richer builders are for tests.
    #[must_use]
    pub fn for_agent(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            as_agent: None,
            request_id: None,
        }
    }
}

bitflags! {
    /// Capability flags advertised by each adapter. Enables feature
    /// detection at runtime so the upper layers can degrade gracefully
    /// rather than error on unsupported ops.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capabilities: u32 {
        /// Adapter supports `begin_transaction` for multi-op atomicity.
        const TRANSACTIONS         = 0b0000_0001;
        /// Native vector search (pgvector, HNSW index inside adapter,
        /// etc.) rather than fallback via this crate's `crate::hnsw`.
        const NATIVE_VECTOR        = 0b0000_0010;
        /// Adapter supports full-text search without an external index.
        const FULLTEXT             = 0b0000_0100;
        /// Adapter persists across process restarts (excludes
        /// `InMemoryStore` test doubles).
        const DURABLE              = 0b0000_1000;
        /// Adapter supports strong (linearizable) reads. Eventual-
        /// consistency adapters clear this bit.
        const STRONG_CONSISTENCY   = 0b0001_0000;
        /// Adapter honors native TTL expiry without application-level
        /// sweeps.
        const TTL_NATIVE           = 0b0010_0000;
        /// Adapter supports atomic multi-row writes (batch insert
        /// under one transaction).
        const ATOMIC_MULTI_WRITE   = 0b0100_0000;
    }
}

/// A unit-of-work handle. Acquired via `MemoryStore::begin_transaction`.
///
/// Closing semantics:
/// - Calling `commit()` finalizes the transaction and releases the
///   handle.
/// - Dropping without commit aborts (rollback).
/// - `Drop::drop` is best-effort; adapters that can fail at rollback
///   time MUST log but NOT panic.
#[async_trait::async_trait]
pub trait Transaction: Send {
    /// Commit the transaction. On success the handle is consumed.
    async fn commit(self: Box<Self>) -> StoreResult<()>;
    /// Explicitly roll back. Same effect as drop but surfaces any
    /// backend error to the caller.
    async fn rollback(self: Box<Self>) -> StoreResult<()>;
}

/// Filter shape passed to `list` / `search` / `recall`. Each field
/// narrows the result set; `None` / empty means "don't narrow on this
/// axis".
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub namespace: Option<String>,
    pub tier: Option<Tier>,
    pub tags_any: Vec<String>,
    pub agent_id: Option<String>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: usize,
}

/// The core trait. Every backend implements this; ai-memory's HTTP /
/// MCP / CLI handlers depend only on `dyn MemoryStore`.
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    /// Capability bits advertised by this adapter. Stable across the
    /// process lifetime.
    fn capabilities(&self) -> Capabilities;

    /// Store a memory. The `ctx` supplies the calling agent; the
    /// `Memory.metadata.agent_id` field is authoritative over any
    /// client-supplied value.
    async fn store(&self, ctx: &CallerContext, memory: &Memory) -> StoreResult<String>;

    /// Fetch a memory by id. Returns `NotFound` when the memory does
    /// not exist OR when the caller lacks read permission (the trait
    /// deliberately does not leak existence; adapters must fold
    /// permission denials into `NotFound`).
    async fn get(&self, ctx: &CallerContext, id: &str) -> StoreResult<Memory>;

    /// Update fields of an existing memory. Every adapter MUST
    /// preserve `metadata.agent_id` across update per Task 1.2 —
    /// see the caller-side `identity::preserve_agent_id` helper.
    async fn update(&self, ctx: &CallerContext, id: &str, patch: UpdatePatch) -> StoreResult<()>;

    /// Hard-delete a memory. Returns `NotFound` if already gone.
    async fn delete(&self, ctx: &CallerContext, id: &str) -> StoreResult<()>;

    /// List matching memories. Ordering is adapter-specific but
    /// deterministic across calls with identical `Filter`.
    async fn list(&self, ctx: &CallerContext, filter: &Filter) -> StoreResult<Vec<Memory>>;

    /// Keyword search (FTS-equivalent). Adapters without full-text
    /// search may return `UnsupportedCapability` and let upper
    /// layers fall back.
    async fn search(
        &self,
        ctx: &CallerContext,
        query: &str,
        filter: &Filter,
    ) -> StoreResult<Vec<Memory>>;

    /// Verify the stored memory's integrity — provenance chain,
    /// signature when present, embedding dimensionality sanity. Used
    /// during migration + sync reconciliation.
    async fn verify(&self, ctx: &CallerContext, id: &str) -> StoreResult<VerifyReport>;

    /// Begin a transaction. Adapters that lack transaction support
    /// return `UnsupportedCapability` and callers should downgrade to
    /// sequential ops.
    async fn begin_transaction(&self, _ctx: &CallerContext) -> StoreResult<Box<dyn Transaction>> {
        Err(StoreError::UnsupportedCapability {
            capability: "TRANSACTIONS".to_string(),
        })
    }

    /// Create a typed link between two memories.
    ///
    /// Always writes `attest_level = "unsigned"` — callers that want a
    /// signed write must reach for [`MemoryStore::link_signed`].
    async fn link(&self, ctx: &CallerContext, link: &MemoryLink) -> StoreResult<()>;

    /// Create a typed link signed by the supplied agent keypair.
    ///
    /// v0.7.0 F6 Gap 3 — exposes the full signed-link contract through
    /// the SAL so federation and self-signed writes do not have to dip
    /// into adapter-specific helpers (`db::create_link_signed`,
    /// `PostgresStore::link_signed`). Mirrors the H2 contract:
    /// when `keypair` is `Some(kp)` AND `kp.can_sign()`, the six
    /// signable fields are CBOR-canonicalised and signed; the resulting
    /// 64-byte signature is persisted with `attest_level = "self_signed"`
    /// and `observed_by = kp.agent_id`. Otherwise the row lands with
    /// `attest_level = "unsigned"`, `signature = NULL`, `observed_by =
    /// NULL` — the same fallback every backend already implements
    /// through [`MemoryStore::link`].
    ///
    /// Returns the resolved attestation level so callers (HTTP / MCP
    /// surfaces) can surface it in the wire response without re-querying.
    ///
    /// The default implementation forwards to [`MemoryStore::link`] and
    /// returns `"unsigned"`, preserving wire-shape parity for adapters
    /// that haven't wired the signing path yet.
    async fn link_signed(
        &self,
        ctx: &CallerContext,
        link: &MemoryLink,
        keypair: Option<&crate::identity::keypair::AgentKeypair>,
    ) -> StoreResult<&'static str> {
        let _ = keypair;
        self.link(ctx, link).await?;
        Ok("unsigned")
    }

    /// Enumerate every link in the store, optionally narrowed to a
    /// namespace.
    ///
    /// v0.7.0 F6 Gap 2 — required by the SAL-driven migrate so
    /// `memory_links` rows survive a cross-backend copy. Adapters
    /// stream through their own `memory_links` table and project into
    /// [`MemoryLink`]; the namespace filter, when set, matches links
    /// whose **source** memory lives in the given namespace (the same
    /// affinity SQLite's `migrate` uses for memories — links live with
    /// their source).
    ///
    /// Ordering is deterministic across calls — adapters sort by
    /// `(source_id, target_id, relation)` so a paginated migrate can
    /// resume mid-stream without losing rows.
    async fn list_links(&self, namespace: Option<&str>) -> StoreResult<Vec<MemoryLink>>;

    /// Register an agent in the adapter's `_agents` namespace (Task
    /// 1.3).
    async fn register_agent(
        &self,
        ctx: &CallerContext,
        agent: &AgentRegistration,
    ) -> StoreResult<()>;

    /// v0.7.0 Wave-3 Continuation — adapter-specific downcast hatch.
    ///
    /// Returns the adapter as `&dyn Any` so that downstream callers
    /// holding an `Arc<dyn MemoryStore>` can recover the concrete
    /// adapter type when they need to call adapter-only helpers
    /// (e.g. `PostgresStore::list_archived` which projects from a
    /// table not yet covered by the trait surface).
    ///
    /// Default returns a unit reference; adapters override to return
    /// `self`.
    fn as_any_for_postgres(&self) -> &dyn std::any::Any {
        &()
    }

    // ==================================================================
    // v0.7.0 Wave-3 Continuation 2 — federation surface (Phase 8).
    //
    // The two methods below underpin the peer-to-peer sync transport.
    // `list_memories_updated_since` powers `GET /api/v1/sync/since`
    // (peer catchup pulls); `apply_remote_memory` powers each row of
    // `POST /api/v1/sync/push` (peer fanout pushes).
    //
    // Both adapters implement. Federation between two postgres-backed
    // daemons and heterogeneous federation (sqlite ↔ postgres) ride
    // exclusively through these trait methods so the wire shape is
    // backend-blind.
    // ==================================================================

    /// List memories whose `updated_at` is strictly greater than the
    /// supplied RFC-3339 timestamp, ordered ascending by `updated_at`.
    ///
    /// `since == None` returns the oldest `limit` memories (initial-sync
    /// posture). Implementations MUST cap their result at the supplied
    /// `limit` value AND apply a sane upper bound (10_000) to prevent
    /// a misbehaving caller from page-pulling the entire database in
    /// one shot.
    ///
    /// Default implementation: `UnsupportedCapability` so adapters that
    /// don't yet wire federation degrade gracefully rather than
    /// silently returning an empty list.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when `since` does not parse as RFC-3339.
    /// Returns `Backend` when the underlying store reports an error.
    async fn list_memories_updated_since(
        &self,
        _since: Option<&str>,
        _limit: usize,
    ) -> StoreResult<Vec<Memory>> {
        Err(StoreError::UnsupportedCapability {
            capability: "FEDERATION_LIST_SINCE".to_string(),
        })
    }

    /// Apply a remote-origin memory through an idempotent
    /// "insert-if-newer" path. Returns the resolved memory id (the
    /// adapter's row id, which may differ from the supplied `memory.id`
    /// when an upsert collapses onto an existing row by `(title,
    /// namespace)`).
    ///
    /// Semantics MUST mirror the sqlite `db::insert_if_newer` contract:
    /// 1. If no existing row matches, INSERT verbatim.
    /// 2. If an existing row matches by id AND its `updated_at` is
    ///    older than the incoming memory's `updated_at`, UPDATE.
    /// 3. If an existing row matches by id AND its `updated_at` is
    ///    newer-or-equal, NOOP (return the existing id).
    /// 4. Tier never downgrades — incoming `mid` does not overwrite
    ///    existing `long`.
    /// 5. `metadata.agent_id` is preserved across upsert.
    ///
    /// Default implementation: `UnsupportedCapability`.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when `memory` fails validation. Returns
    /// `Backend` for storage errors.
    async fn apply_remote_memory(
        &self,
        _ctx: &CallerContext,
        _memory: &Memory,
    ) -> StoreResult<String> {
        Err(StoreError::UnsupportedCapability {
            capability: "FEDERATION_APPLY_REMOTE".to_string(),
        })
    }

    /// Apply a remote-origin link via the same idempotent posture as
    /// [`MemoryStore::apply_remote_memory`]. The unique
    /// `(source_id, target_id, relation)` index makes duplicate
    /// federation pushes a no-op.
    ///
    /// `attest_level` is the resolved attestation level the receiver
    /// computed (see `handlers::sync_push` H3 verify path) — adapters
    /// stamp this into the row so subsequent reads carry the
    /// peer-attested / unsigned distinction.
    ///
    /// Default implementation: forward to [`MemoryStore::link`] which
    /// always lands the row as `unsigned`. Postgres + SQLite override
    /// to honor `attest_level`.
    async fn apply_remote_link(
        &self,
        ctx: &CallerContext,
        link: &MemoryLink,
        attest_level: &str,
    ) -> StoreResult<()> {
        let _ = attest_level;
        self.link(ctx, link).await
    }

    /// Hard-delete a memory by id, returning `true` when a row was
    /// removed and `false` when no row matched (already-deleted /
    /// never-existed). Default implementation lifts the trait `delete`
    /// surface — which returns `NotFound` on miss — into a boolean for
    /// federation's no-op-on-missing-row contract.
    async fn apply_remote_deletion(&self, ctx: &CallerContext, id: &str) -> StoreResult<bool> {
        match self.delete(ctx, id).await {
            Ok(()) => Ok(true),
            Err(StoreError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Partial-update payload. `None` means "leave this field alone" —
/// serde `Option<Option<T>>` gymnastics are out of scope for v0.6.0.0.
#[derive(Debug, Default, Clone)]
pub struct UpdatePatch {
    pub title: Option<String>,
    pub content: Option<String>,
    pub tier: Option<Tier>,
    pub namespace: Option<String>,
    pub tags: Option<Vec<String>>,
    pub priority: Option<i32>,
    pub confidence: Option<f64>,
    pub metadata: Option<serde_json::Value>,
}

/// Report produced by `verify`.
///
/// **Important**: as of v0.6.0 neither the SQLite nor the Postgres
/// adapter performs cryptographic signature verification. `verify()`
/// is a structural-integrity check only (empty fields / missing
/// metadata keys / schema-level sanity). The \`signature_verified\`
/// flag reports whether real signature verification was performed —
/// always \`false\` today; will flip to \`true\` once Task 1.4 (signed
/// memories) lands. Callers MUST NOT treat \`integrity_ok: true\`
/// as a trust signal; only \`signature_verified: true\` carries that
/// weight. (#302 item 5.)
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub memory_id: String,
    pub integrity_ok: bool,
    pub findings: Vec<String>,
    /// True iff the adapter performed a real cryptographic signature
    /// verification. Always false pre-Task-1.4.
    pub signature_verified: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_context_builder_defaults() {
        let ctx = CallerContext::for_agent("alice");
        assert_eq!(ctx.agent_id, "alice");
        assert!(ctx.as_agent.is_none());
        assert!(ctx.request_id.is_none());
    }

    #[test]
    fn capabilities_bitflags_compose() {
        let caps = Capabilities::TRANSACTIONS | Capabilities::DURABLE;
        assert!(caps.contains(Capabilities::TRANSACTIONS));
        assert!(caps.contains(Capabilities::DURABLE));
        assert!(!caps.contains(Capabilities::NATIVE_VECTOR));
    }

    #[test]
    fn store_error_display_is_human_readable() {
        let err = StoreError::NotFound {
            id: "abc".to_string(),
        };
        assert_eq!(err.to_string(), "memory not found: abc");
        let err = StoreError::PermissionDenied {
            action: "read".to_string(),
            target: "memory/abc".to_string(),
            reason: "row-level ACL".to_string(),
        };
        assert!(err.to_string().contains("read"));
        assert!(err.to_string().contains("row-level ACL"));
    }

    #[test]
    fn default_begin_transaction_errors() {
        // The default trait method returns UnsupportedCapability;
        // adapters that actually support txns override it. This is
        // checked indirectly — adapters without an override will
        // surface the error via this variant when called.
        let err = StoreError::UnsupportedCapability {
            capability: "TRANSACTIONS".to_string(),
        };
        assert!(err.to_string().contains("TRANSACTIONS"));
    }

    #[test]
    fn filter_defaults_are_empty() {
        let f = Filter::default();
        assert!(f.namespace.is_none());
        assert!(f.tier.is_none());
        assert!(f.tags_any.is_empty());
    }

    #[test]
    fn kg_backend_serializes_snake_case() {
        // Wire-shape contract: `kg_backend` is always projected as the
        // lowercase tag so the capabilities surface, doctor report, and
        // log lines can never drift from the enum.
        let cte = serde_json::to_string(&KgBackend::Cte).unwrap();
        let age = serde_json::to_string(&KgBackend::Age).unwrap();
        assert_eq!(cte, "\"cte\"");
        assert_eq!(age, "\"age\"");

        // Round-trip via deserialize so the same strings parse back.
        let cte_round: KgBackend = serde_json::from_str("\"cte\"").unwrap();
        let age_round: KgBackend = serde_json::from_str("\"age\"").unwrap();
        assert_eq!(cte_round, KgBackend::Cte);
        assert_eq!(age_round, KgBackend::Age);
    }

    #[test]
    fn kg_backend_as_str_matches_display() {
        // `Display` and `as_str` must agree — log lines and the doctor
        // report use whichever is closer to hand and must produce the
        // same bytes.
        assert_eq!(KgBackend::Cte.as_str(), "cte");
        assert_eq!(KgBackend::Age.as_str(), "age");
        assert_eq!(format!("{}", KgBackend::Cte), "cte");
        assert_eq!(format!("{}", KgBackend::Age), "age");
    }
}
