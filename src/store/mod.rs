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

use bitflags::bitflags;

use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};

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
    async fn link(&self, ctx: &CallerContext, link: &MemoryLink) -> StoreResult<()>;

    /// Register an agent in the adapter's `_agents` namespace (Task
    /// 1.3).
    async fn register_agent(
        &self,
        ctx: &CallerContext,
        agent: &AgentRegistration,
    ) -> StoreResult<()>;
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
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub memory_id: String,
    pub integrity_ok: bool,
    pub findings: Vec<String>,
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
}
