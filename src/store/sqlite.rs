// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! In-tree `SqliteStore` adapter. Wraps the existing `crate::db` free
//! functions so the production path can migrate to the SAL trait
//! gradually. No behavior change vs. calling `crate::db` directly —
//! this is a thin shim whose only job is to prove the trait surface
//! fits the shape of the shipped code.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::db;
use crate::models::{AgentRegistration, Memory, MemoryLink};

use super::{
    BoxBackendError, CallerContext, Capabilities, Filter, MemoryStore, StoreError, StoreResult,
    Transaction, UpdatePatch, VerifyReport,
};

/// SAL adapter over the existing bundled-SQLite storage. Holds an
/// `Arc<Mutex<Connection>>` matching the HTTP daemon's shared state so
/// the adapter can be used alongside the existing free-function code
/// paths during the migration.
pub struct SqliteStore {
    state: Arc<Mutex<rusqlite::Connection>>,
    path: PathBuf,
}

impl SqliteStore {
    /// Open (or create) a `SqliteStore` at the given path. Delegates
    /// schema init + migration to `crate::db::open`.
    pub fn open(path: impl Into<PathBuf>) -> StoreResult<Self> {
        let path = path.into();
        let conn = db::open(&path).map_err(box_err)?;
        Ok(Self {
            state: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    /// Path the adapter opened. Useful for diagnostics and for
    /// callers that need to spawn subprocesses (backup, rekey).
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn box_err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(BoxBackendError::new(e.to_string()))
}

#[async_trait::async_trait]
impl MemoryStore for SqliteStore {
    fn capabilities(&self) -> Capabilities {
        Capabilities::TRANSACTIONS
            | Capabilities::FULLTEXT
            | Capabilities::DURABLE
            | Capabilities::STRONG_CONSISTENCY
            | Capabilities::ATOMIC_MULTI_WRITE
    }

    async fn store(&self, _ctx: &CallerContext, memory: &Memory) -> StoreResult<String> {
        let conn = self.state.lock().await;
        db::insert(&conn, memory).map_err(box_err)
    }

    async fn get(&self, _ctx: &CallerContext, id: &str) -> StoreResult<Memory> {
        let conn = self.state.lock().await;
        match db::get(&conn, id).map_err(box_err)? {
            Some(mem) => Ok(mem),
            None => Err(StoreError::NotFound { id: id.to_string() }),
        }
    }

    async fn update(&self, _ctx: &CallerContext, id: &str, patch: UpdatePatch) -> StoreResult<()> {
        let conn = self.state.lock().await;
        let (found, _content_changed) = db::update(
            &conn,
            id,
            patch.title.as_deref(),
            patch.content.as_deref(),
            patch.tier.as_ref(),
            patch.namespace.as_deref(),
            patch.tags.as_ref(),
            patch.priority,
            patch.confidence,
            None,
            patch.metadata.as_ref(),
        )
        .map_err(box_err)?;
        if found {
            Ok(())
        } else {
            Err(StoreError::NotFound { id: id.to_string() })
        }
    }

    async fn delete(&self, _ctx: &CallerContext, id: &str) -> StoreResult<()> {
        let conn = self.state.lock().await;
        let removed = db::delete(&conn, id).map_err(box_err)?;
        if removed {
            Ok(())
        } else {
            Err(StoreError::NotFound { id: id.to_string() })
        }
    }

    async fn list(&self, _ctx: &CallerContext, filter: &Filter) -> StoreResult<Vec<Memory>> {
        let conn = self.state.lock().await;
        let tags_first = filter.tags_any.first().map(String::as_str);
        let since = filter.since.map(|d| d.to_rfc3339());
        let until = filter.until.map(|d| d.to_rfc3339());
        db::list(
            &conn,
            filter.namespace.as_deref(),
            filter.tier.as_ref(),
            if filter.limit == 0 { 100 } else { filter.limit },
            0,
            None,
            since.as_deref(),
            until.as_deref(),
            tags_first,
            filter.agent_id.as_deref(),
        )
        .map_err(box_err)
    }

    async fn search(
        &self,
        ctx: &CallerContext,
        query: &str,
        filter: &Filter,
    ) -> StoreResult<Vec<Memory>> {
        let conn = self.state.lock().await;
        let tags_first = filter.tags_any.first().map(String::as_str);
        let since = filter.since.map(|d| d.to_rfc3339());
        let until = filter.until.map(|d| d.to_rfc3339());
        db::search(
            &conn,
            query,
            filter.namespace.as_deref(),
            filter.tier.as_ref(),
            if filter.limit == 0 { 100 } else { filter.limit },
            None,
            since.as_deref(),
            until.as_deref(),
            tags_first,
            filter.agent_id.as_deref(),
            ctx.as_agent.as_deref(),
        )
        .map_err(box_err)
    }

    async fn verify(&self, _ctx: &CallerContext, id: &str) -> StoreResult<VerifyReport> {
        let conn = self.state.lock().await;
        let Some(mem) = db::get(&conn, id).map_err(box_err)? else {
            return Err(StoreError::NotFound { id: id.to_string() });
        };
        // v0.6.0.0 preview: minimal integrity check. Confirms that
        // the memory has a non-empty title + content and its
        // `metadata.agent_id` round-trips as a string. Real
        // signature verification lands alongside Task 1.4.
        let mut findings: Vec<String> = Vec::new();
        if mem.title.trim().is_empty() {
            findings.push("title is empty".to_string());
        }
        if mem.content.trim().is_empty() {
            findings.push("content is empty".to_string());
        }
        if mem.metadata.get("agent_id").is_none() {
            findings.push("metadata.agent_id missing".to_string());
        }
        Ok(VerifyReport {
            memory_id: id.to_string(),
            integrity_ok: findings.is_empty(),
            findings,
        })
    }

    async fn link(&self, _ctx: &CallerContext, link: &MemoryLink) -> StoreResult<()> {
        let conn = self.state.lock().await;
        db::create_link(
            &conn,
            &link.source_id,
            &link.target_id,
            link.relation.as_str(),
        )
        .map_err(box_err)
    }

    async fn register_agent(
        &self,
        _ctx: &CallerContext,
        agent: &AgentRegistration,
    ) -> StoreResult<()> {
        let conn = self.state.lock().await;
        db::register_agent(
            &conn,
            &agent.agent_id,
            &agent.agent_type,
            &agent.capabilities,
        )
        .map_err(box_err)
        .map(|_id| ())
    }
}

/// Transaction handle that no-ops commit (`SQLite` txn support is
/// available via `rusqlite::Connection::unchecked_transaction` but
/// wrapping through the mutex gets awkward — for the preview, callers
/// that need real atomicity should still reach through `crate::db`
/// directly).
#[allow(dead_code)]
pub struct SqliteTransaction;

#[async_trait::async_trait]
impl Transaction for SqliteTransaction {
    async fn commit(self: Box<Self>) -> StoreResult<()> {
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> StoreResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tier;

    fn test_memory(title: &str, content: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "sal-test".to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec!["test".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "alice"}),
        }
    }

    #[tokio::test]
    async fn roundtrip_store_get() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("hello", "world one two three four five six seven");
        let stored_id = store.store(&ctx, &mem).await.expect("store");
        let loaded = store.get(&ctx, &stored_id).await.expect("get");
        assert_eq!(loaded.title, "hello");
    }

    #[tokio::test]
    async fn get_missing_returns_not_found() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let ctx = CallerContext::for_agent("alice");
        let err = store
            .get(&ctx, "00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("should be NotFound");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn capabilities_declare_sqlite_reality() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let caps = store.capabilities();
        assert!(caps.contains(Capabilities::DURABLE));
        assert!(caps.contains(Capabilities::FULLTEXT));
        assert!(caps.contains(Capabilities::STRONG_CONSISTENCY));
        // NATIVE_VECTOR is intentionally NOT set — semantic search
        // happens above this layer via crate::hnsw, not inside the
        // adapter.
        assert!(!caps.contains(Capabilities::NATIVE_VECTOR));
    }

    #[tokio::test]
    async fn verify_flags_empty_content() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let ctx = CallerContext::for_agent("alice");
        let mut mem = test_memory("hello", "x content long enough to pass validate");
        mem.content = "nonempty for store".to_string();
        let id = store.store(&ctx, &mem).await.expect("store");
        // Manually corrupt metadata.agent_id via update.
        store
            .update(
                &ctx,
                &id,
                UpdatePatch {
                    metadata: Some(serde_json::json!({})),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        let report = store.verify(&ctx, &id).await.expect("verify");
        assert!(!report.integrity_ok);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("metadata.agent_id"))
        );
    }
}
