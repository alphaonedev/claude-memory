// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `SQLite` adapter for the [`super::MemoryStore`] trait — v0.7 proposal
//! skeleton.
//!
//! See issue #221 and [`super`] for the architectural rationale. During the
//! v0.7.0-alpha extraction phase this file will absorb the call patterns
//! currently embedded directly in `db.rs` call sites across `mcp.rs`,
//! `handlers.rs`, and `main.rs`. For now it sketches the shape so reviewers
//! can react to concrete Rust.
//!
//! # What's wired
//!
//! - [`SqliteStore::store`] / [`SqliteStore::health_check`] — thin
//!   delegations to `crate::db`, demonstrating the async → `spawn_blocking` →
//!   rusqlite pattern the rest of the adapter will follow.
//! - [`SqliteStore::capabilities`] / [`SqliteStore::backend_name`] — static
//!   metadata.
//!
//! # What's `todo!()`
//!
//! Every other method. They'll be filled during v0.7.0-alpha, one per PR so
//! the call-site migration is reviewable in small pieces.

use super::{
    AgentRegistration, ApproveOutcome, Approver, CallerContext, Capabilities, Filter, GcPolicy,
    GcReport, MemoryStore, PendingActionRequest, RecallRequest, Scored, UpdatePatch,
};
use crate::db;
use crate::models::{Memory, MemoryLink};
use anyhow::Result;
use async_trait::async_trait;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// SQLite-backed store. The inner `Arc<Mutex<Connection>>` mirrors the
/// current v0.6.x shape so the extraction can be done without touching the
/// concurrency model; the connection pool referenced in #219 is a
/// v0.7.x follow-up that's internal to this adapter.
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    #[allow(dead_code)] // Consumed by migration / health-check paths not yet ported.
    db_path: PathBuf,
}

impl SqliteStore {
    /// Open a `SQLite` store at `path`. Delegates schema setup to the existing
    /// [`crate::db::open`] so the v0.7 skeleton opens identically to v0.6.x.
    pub fn open(path: PathBuf) -> Result<Self> {
        let conn = db::open(&path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: path,
        })
    }
}

#[async_trait]
impl MemoryStore for SqliteStore {
    // ----- CRUD — wired as a shape demonstration -----
    async fn store(&self, mem: &Memory) -> Result<String> {
        let conn = self.conn.clone();
        let mem = mem.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.blocking_lock();
            db::insert(&guard, &mem)
        })
        .await?
    }

    async fn health_check(&self) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.blocking_lock();
            Ok(db::health_check(&guard).unwrap_or(false))
        })
        .await?
    }

    // ----- Everything below is filled in during v0.7.0-alpha -----
    async fn get(&self, _id: &str, _ctx: &CallerContext) -> Result<Option<Memory>> {
        todo!("v0.7.0-alpha: port db::get + visibility filter via CallerContext")
    }
    async fn update(&self, _id: &str, _patch: UpdatePatch) -> Result<Memory> {
        todo!("v0.7.0-alpha: port db::update")
    }
    async fn delete(&self, _id: &str, _ctx: &CallerContext) -> Result<bool> {
        todo!("v0.7.0-alpha: port db::delete")
    }
    async fn list(&self, _filter: Filter, _ctx: &CallerContext) -> Result<Vec<Memory>> {
        todo!("v0.7.0-alpha: port db::list + translate Filter to SQL")
    }

    async fn keyword_search(&self, _q: &str, _filter: Filter) -> Result<Vec<Scored>> {
        todo!("v0.7.0-alpha: port db::search (FTS5)")
    }
    async fn vector_search(&self, _embedding: &[f32], _filter: Filter) -> Result<Vec<Scored>> {
        todo!("v0.7.0-alpha: port HNSW semantic path (linear scan if index absent)")
    }
    async fn hybrid_recall(&self, _req: RecallRequest) -> Result<Vec<Scored>> {
        todo!("v0.7.0-alpha: port db::recall_hybrid — the big one; keep touch atomicity")
    }

    async fn link(&self, _src: &str, _tgt: &str, _relation: &str) -> Result<()> {
        todo!("v0.7.0-alpha: port db::link")
    }
    async fn neighbors(&self, _id: &str, _relation: Option<&str>) -> Result<Vec<MemoryLink>> {
        todo!("v0.7.0-alpha: port db::get_links")
    }

    async fn register_agent(&self, _reg: AgentRegistration) -> Result<String> {
        todo!("v0.7.0-alpha: port db::register_agent")
    }
    async fn is_registered_agent(&self, _agent_id: &str) -> Result<bool> {
        todo!("v0.7.0-alpha: port is_registered_agent check")
    }
    async fn queue_pending(&self, _req: PendingActionRequest) -> Result<String> {
        todo!("v0.7.0-alpha: port pending_actions queue path")
    }
    async fn approve_pending(&self, _id: &str, _approver: Approver) -> Result<ApproveOutcome> {
        todo!("v0.7.0-alpha: port approve_with_approver_type (post-#216 hardening)")
    }

    async fn gc(&self, _policy: GcPolicy) -> Result<GcReport> {
        todo!("v0.7.0-alpha: port db::gc + db::auto_purge_archive")
    }
    async fn migrate(&self) -> Result<()> {
        todo!("v0.7.0-alpha: expose db::open migration path explicitly")
    }

    fn capabilities(&self) -> Capabilities {
        // SQLite advertises everything except CURSOR_PAGINATION (LIMIT/OFFSET
        // only in v0.6.x schema; cursor support lands with v0.8 streaming).
        Capabilities::NATIVE_FTS
            | Capabilities::NATIVE_VECTOR  // HNSW-in-process satisfies this at the trait boundary
            | Capabilities::TRANSACTIONS
            | Capabilities::JSON_FILTERS
            | Capabilities::SCHEMA_EVOLUTION
            | Capabilities::FAST_COUNT
    }

    fn backend_name(&self) -> &'static str {
        "sqlite"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_and_health_check() {
        // :memory: is the v0.7 equivalent of a file-less smoke test.
        let store = SqliteStore::open(std::path::PathBuf::from(":memory:")).unwrap();
        assert!(store.health_check().await.unwrap());
        assert_eq!(store.backend_name(), "sqlite");
        assert!(
            store
                .capabilities()
                .contains(Capabilities::NATIVE_FTS | Capabilities::TRANSACTIONS)
        );
    }

    #[tokio::test]
    async fn store_round_trip_demonstrates_shape() {
        use crate::models::Tier;
        use chrono::Utc;

        let store = SqliteStore::open(std::path::PathBuf::from(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "sal-proposal".into(),
            title: "SAL skeleton round-trip".into(),
            content: "Prove the async → spawn_blocking → rusqlite shape.".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        let id = store.store(&mem).await.unwrap();
        assert!(!id.is_empty());
    }
}
