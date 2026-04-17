// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Storage Abstraction Layer (SAL) — v0.7 proposal skeleton.
//!
//! See issue #221 for the architectural rationale. The short version:
//! v0.6.x is `SQLite`-only and that caps deployment options (`Postgres`+pgvector,
//! `Qdrant`, `LanceDB`, `Chroma`, managed vector stores). This module introduces
//! a [`MemoryStore`] trait that every backend implements. `SQLite` remains the
//! default; other backends slot in behind feature flags.
//!
//! # Status
//!
//! This is a **proposal skeleton**, not a shipped feature. The file exists so
//! the v0.7 design can be reviewed as concrete Rust rather than prose. Most
//! methods on [`sqlite::SqliteStore`] are `todo!()` — they'll be filled in
//! during the v0.7.0-alpha extraction phase. Do not wire this into
//! `main.rs` / `handlers.rs` / `mcp.rs` until the extraction lands behind
//! a feature flag.
//!
//! # Design principles
//!
//! 1. **Backend-agnostic filters** — callers pass a structured [`Filter`]
//!    AST, never raw SQL. Each adapter translates the AST into its native
//!    query language (SQL for SQLite/Postgres, filter JSON for Qdrant,
//!    metadata predicates for Chroma).
//! 2. **Graceful degradation via [`Capabilities`]** — backends advertise
//!    what they support natively. The core layer has fallbacks for every
//!    capability that could be absent (e.g., if `NATIVE_FTS` is false, the
//!    core layer runs keyword match in Rust after a filter fetch).
//! 3. **No behaviour change during extraction** — v0.7.0-alpha lands
//!    [`sqlite::SqliteStore`] behind the trait with identical semantics to
//!    v0.6.x. All existing tests must pass unchanged.
//! 4. **Governance / scope / NHI stay in the core layer** — backends filter
//!    by namespace/scope/`agent_id` via the [`Filter`] AST but never
//!    re-implement visibility rules.

use crate::models::{ApproverType, Memory, MemoryLink, Tier};
use anyhow::Result;
use async_trait::async_trait;
use bitflags::bitflags;

pub mod sqlite;

/// Caller context for every storage operation.
///
/// Carries the calling agent's identity and its position in the namespace
/// hierarchy so backends can apply visibility filtering without each
/// implementation re-deriving it from raw inputs.
#[derive(Debug, Clone)]
pub struct CallerContext {
    /// Agent that initiated the operation (NHI). None when unauthenticated
    /// / anonymous — backends should treat this as "no visibility filter
    /// applied" since the core layer validates before reaching the store.
    pub agent_id: Option<String>,
    /// Agent's hierarchical namespace position, if known. Used by visibility
    /// filtering to select team/unit/org-scoped memories.
    pub as_agent_namespace: Option<String>,
}

/// Backend-agnostic filter AST.
///
/// Core-layer code builds a [`Filter`] from the caller's request; adapters
/// translate it to their native query model. No raw SQL, no Qdrant JSON —
/// nothing leaks to the trait boundary.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub namespace: Option<NamespaceFilter>,
    pub tier: Option<Tier>,
    pub min_priority: Option<i32>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub tags_any: Vec<String>,
    pub agent_id: Option<String>,
    pub scope: Option<ScopeFilter>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Namespace matching semantics. Exact match for flat namespaces; the
/// hierarchy variant expands to all ancestors (Task 1.12 proximity recall).
#[derive(Debug, Clone)]
pub enum NamespaceFilter {
    Exact(String),
    Hierarchy(String),
}

/// Visibility-scope filter matching `src/db.rs::visibility_clause` semantics.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct ScopeFilter {
    pub private_prefix: Option<String>,
    pub team_prefix: Option<String>,
    pub unit_prefix: Option<String>,
    pub org_prefix: Option<String>,
}

/// A memory returned from a scored retrieval operation (keyword, vector,
/// or hybrid recall).
#[derive(Debug, Clone)]
pub struct Scored {
    pub memory: Memory,
    pub score: f64,
}

/// Query parameters for a full hybrid-recall operation. Captures everything
/// `db::recall_hybrid` needs today; add fields here (not to every method)
/// as new recall features land.
#[derive(Debug, Clone)]
pub struct RecallRequest {
    pub query: String,
    pub query_embedding: Option<Vec<f32>>,
    pub filter: Filter,
    pub caller: CallerContext,
    pub budget_tokens: Option<usize>,
}

/// Touch payload — recall mutates `access_count` / `last_accessed_at` / TTL /
/// promotion on matched rows. Extracted so backends can implement it as
/// an atomic update.
#[derive(Debug, Clone)]
pub struct TouchPolicy {
    pub short_ttl_extend_secs: i64,
    pub mid_ttl_extend_secs: i64,
    pub promotion_threshold: i32,
}

/// Update patch used by `update`. `None` fields are left untouched.
#[derive(Debug, Clone, Default)]
pub struct UpdatePatch {
    pub title: Option<String>,
    pub content: Option<String>,
    pub tier: Option<Tier>,
    pub namespace: Option<String>,
    pub tags: Option<Vec<String>>,
    pub priority: Option<i32>,
    pub confidence: Option<f64>,
    pub expires_at: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Outcome of a pending-action approval.
#[derive(Debug, Clone)]
pub enum ApproveOutcome {
    Approved,
    Pending { votes: usize, quorum: u32 },
    Rejected(String),
}

/// Agent registration payload.
#[derive(Debug, Clone)]
pub struct AgentRegistration {
    pub agent_id: String,
    pub agent_type: String,
    pub capabilities: Vec<String>,
}

/// Approver identity for a pending-action decision.
#[derive(Debug, Clone)]
pub struct Approver {
    pub agent_id: String,
    pub approver_type: ApproverType,
}

/// Pending-action request (store/delete/promote gated by governance).
#[derive(Debug, Clone)]
pub struct PendingActionRequest {
    pub action_type: String, // "store" / "delete" / "promote"
    pub namespace: String,
    pub requested_by: String,
    pub payload: serde_json::Value,
}

/// Garbage-collection policy.
#[derive(Debug, Clone, Default)]
pub struct GcPolicy {
    pub archive_before_delete: bool,
    pub archive_max_days: Option<i64>,
}

/// Counters returned from a GC sweep.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub expired: usize,
    pub archived: usize,
    pub purged: usize,
}

bitflags! {
    /// Capability flags advertised by each backend. The core layer uses
    /// these to decide when to dispatch a native operation vs. fall back
    /// to a portable implementation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capabilities: u32 {
        /// Backend has native full-text search (SQLite FTS5, Postgres
        /// tsvector, Qdrant full-text, Elasticsearch). Absent → core
        /// layer runs keyword match in Rust after a filter fetch.
        const NATIVE_FTS       = 0b0000_0001;
        /// Backend has native vector ANN (Postgres+pgvector, Qdrant,
        /// LanceDB). Absent → core layer does linear scan in Rust.
        const NATIVE_VECTOR    = 0b0000_0010;
        /// Backend supports multi-statement transactions.
        const TRANSACTIONS     = 0b0000_0100;
        /// Backend can filter on JSON metadata fields
        /// (SQLite `json_extract`, Postgres `->>`, Qdrant payload).
        const JSON_FILTERS     = 0b0000_1000;
        /// Backend supports forward-evolving schema (DDL migrations).
        /// Document-store backends generally return false.
        const SCHEMA_EVOLUTION = 0b0001_0000;
        /// Backend can cheaply produce a strict row count for a filter.
        const FAST_COUNT       = 0b0010_0000;
        /// Backend supports cursor/pagination beyond LIMIT+OFFSET.
        const CURSOR_PAGINATION = 0b0100_0000;
    }
}

/// Storage trait every backend implements.
///
/// All methods are async; adapters that wrap a blocking driver (rusqlite)
/// run their work via `spawn_blocking` internally. Callers do NOT need to
/// know which model the backend uses.
#[async_trait]
#[allow(dead_code)] // Skeleton — methods are wired to real callers in v0.7.0-alpha.
pub trait MemoryStore: Send + Sync {
    // ----- CRUD -----
    async fn store(&self, mem: &Memory) -> Result<String>;
    async fn get(&self, id: &str, ctx: &CallerContext) -> Result<Option<Memory>>;
    async fn update(&self, id: &str, patch: UpdatePatch) -> Result<Memory>;
    async fn delete(&self, id: &str, ctx: &CallerContext) -> Result<bool>;
    async fn list(&self, filter: Filter, ctx: &CallerContext) -> Result<Vec<Memory>>;

    // ----- Retrieval -----
    async fn keyword_search(&self, q: &str, filter: Filter) -> Result<Vec<Scored>>;
    async fn vector_search(&self, embedding: &[f32], filter: Filter) -> Result<Vec<Scored>>;
    async fn hybrid_recall(&self, req: RecallRequest) -> Result<Vec<Scored>>;

    // ----- Graph -----
    async fn link(&self, src: &str, tgt: &str, relation: &str) -> Result<()>;
    async fn neighbors(&self, id: &str, relation: Option<&str>) -> Result<Vec<MemoryLink>>;

    // ----- Governance / agents / pending actions -----
    async fn register_agent(&self, reg: AgentRegistration) -> Result<String>;
    async fn is_registered_agent(&self, agent_id: &str) -> Result<bool>;
    async fn queue_pending(&self, req: PendingActionRequest) -> Result<String>;
    async fn approve_pending(&self, id: &str, approver: Approver) -> Result<ApproveOutcome>;

    // ----- Lifecycle -----
    async fn gc(&self, policy: GcPolicy) -> Result<GcReport>;
    async fn migrate(&self) -> Result<()>;
    async fn health_check(&self) -> Result<bool>;

    /// Static capability flags (no I/O).
    fn capabilities(&self) -> Capabilities;

    /// Human-readable backend identifier for logs and diagnostics.
    fn backend_name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_compose() {
        let c = Capabilities::NATIVE_FTS | Capabilities::NATIVE_VECTOR;
        assert!(c.contains(Capabilities::NATIVE_FTS));
        assert!(c.contains(Capabilities::NATIVE_VECTOR));
        assert!(!c.contains(Capabilities::TRANSACTIONS));
    }

    #[test]
    fn filter_default_is_empty() {
        let f = Filter::default();
        assert!(f.namespace.is_none());
        assert!(f.tags_any.is_empty());
    }
}
