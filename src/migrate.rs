// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Cross-backend migration tool — stream memories from one SAL backend
//! to another (v0.7 track B, PR 2 of N).
//!
//! Gated behind `--features sal` (trait + `SqliteStore`), extended
//! transparently by `--features sal-postgres` (adds the Postgres
//! adapter).
//!
//! ## Supported URL shapes
//!
//! - `sqlite:///absolute/path/to/file.db` → `SqliteStore`
//! - `sqlite://./relative/path.db` (two slashes) → `SqliteStore`
//! - `postgres://user:pass@host:port/dbname` → `PostgresStore`
//!   (only when `--features sal-postgres`)
//!
//! Anything else is rejected with a clear error.
//!
//! ## CLI
//!
//! ```text
//! ai-memory migrate --from sqlite:///var/lib/ai-memory/ai-memory.db \
//!                   --to postgres://user:pass@pg:5432/ai_memory \
//!                   [--batch 1000] [--dry-run] [--namespace foo]
//! ```
//!
//! Reads batches via `MemoryStore::list`, writes via `MemoryStore::store`.
//! Each write uses the source memory's id verbatim — the adapter's
//! upsert-on-id semantics means repeating the migration is idempotent.
//!
//! ## What this module does NOT do
//!
//! - **Daemon adapter selection** (`ai-memory serve --store-url
//!   postgres://…`) — that's a bigger refactor because `handlers.rs`
//!   still calls `crate::db::` free functions. Deferred to v0.7.1.
//! - **Live dual-write** — this is a one-way copy. Reverse migration
//!   (pg → sqlite) works identically but carries the same semantics.
//! - **Schema rewriting** — both backends use the same `Memory` shape.

#![cfg(feature = "sal")]

use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::store::{CallerContext, Filter, MemoryStore, sqlite::SqliteStore};

/// One migration batch. Exposed for external callers that want to
/// run a migration programmatically (e.g. a test harness or a
/// management-plane service).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MigrationReport {
    pub from_url: String,
    pub to_url: String,
    pub memories_read: usize,
    pub memories_written: usize,
    pub batches: usize,
    pub errors: Vec<String>,
    pub dry_run: bool,
}

/// Build a `Box<dyn MemoryStore>` from a URL. Feature-gated — the
/// Postgres branch exists only when `sal-postgres` is compiled in.
///
/// Async because the `sal-postgres` branch awaits a connection-pool
/// build. The `sqlite://` branch is synchronous under the hood but
/// returns via the same `Future` so callers have a single polymorphic
/// code path regardless of feature combination.
///
/// # Errors
///
/// Returns an error for unrecognised URL schemes or adapter-
/// construction failures (bad path, unreachable Postgres, etc.).
#[allow(clippy::unused_async)]
pub async fn open_store(url: &str) -> Result<Box<dyn MemoryStore>> {
    if let Some(path) = url.strip_prefix("sqlite://") {
        // Strip the optional third slash (sqlite:///foo → /foo;
        // sqlite://./foo → ./foo).
        let clean = path
            .strip_prefix('/')
            .map_or(path, |p| if p.starts_with('/') { p } else { path });
        let store = SqliteStore::open(clean).context("open sqlite adapter")?;
        return Ok(Box::new(store));
    }

    #[cfg(feature = "sal-postgres")]
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let store = crate::store::postgres::PostgresStore::connect(url)
            .await
            .context("connect postgres adapter")?;
        return Ok(Box::new(store));
    }

    anyhow::bail!("unrecognised store URL: {url} (expected sqlite:///path or postgres://...)")
}

/// Run the migration. Streams through the source in pages of
/// `batch_size`, writing each page to the destination. Idempotent on
/// re-run — both adapters' `store` implementations upsert on memory id.
pub async fn migrate(
    from: &dyn MemoryStore,
    to: &dyn MemoryStore,
    batch_size: usize,
    namespace_filter: Option<String>,
    dry_run: bool,
) -> MigrationReport {
    let ctx = CallerContext::for_agent("ai:migrate");
    let mut report = MigrationReport {
        memories_read: 0,
        memories_written: 0,
        batches: 0,
        errors: Vec::new(),
        dry_run,
        ..MigrationReport::default()
    };

    // Migration strategy (blocker #298 fix).
    //
    // The earlier pagination used `created_at` as the cursor, but
    // adapter `list` returns rows ordered by `priority DESC, updated_at
    // DESC`. Low-priority memories newer than page-1's `min(created_at)`
    // were permanently skipped — the priority-ordered page didn't
    // include them, and the created_at cursor then filtered them out on
    // the next call. That's data loss, silently.
    //
    // For v0.6.0 we migrate in a single `list` call capped at MAX_ROWS.
    // The caller's `batch_size` parameter is kept for API compatibility
    // but is NOT used to cap total rows — it's a hint for the future
    // streaming migrate tool (tracked in v0.7 as
    // `MemoryStore::list_all`).
    //
    // Correctness > throughput: a correct single-call migrate is
    // strictly preferable to a paginated migrate that silently drops
    // rows. If the source exceeds MAX_ROWS the migration refuses
    // loudly rather than truncating.
    const MAX_ROWS: usize = 1_000_000;
    let _ = batch_size; // Retained for API compatibility; see comment above.

    let filter = Filter {
        namespace: namespace_filter.clone(),
        until: None,
        limit: MAX_ROWS,
        ..Filter::default()
    };
    let page = match from.list(&ctx, &filter).await {
        Ok(p) => p,
        Err(e) => {
            report.errors.push(format!("source list failed: {e}"));
            return report;
        }
    };

    // Detect cap saturation. If the source returned exactly MAX_ROWS
    // memories, refuse rather than risk silent truncation. Operators
    // with >1M memories need the streaming migrate (v0.7).
    if page.len() >= MAX_ROWS {
        report.errors.push(format!(
            "source has >= {} memories; single-call migrate cap reached. \
             Use the streaming migrate tool (v0.7+) instead of \
             silently dropping rows.",
            MAX_ROWS
        ));
        return report;
    }

    let mut seen: HashSet<String> = HashSet::new();
    report.batches = 1;
    for mem in &page {
        if !seen.insert(mem.id.clone()) {
            continue;
        }
        report.memories_read += 1;
        if !dry_run {
            match to.store(&ctx, mem).await {
                Ok(_) => report.memories_written += 1,
                Err(e) => report.errors.push(format!("write {} failed: {e}", mem.id)),
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, Tier};

    fn sample_memory(id: &str, ns: &str, title: &str) -> Memory {
        sample_memory_at(id, ns, title, chrono::Utc::now())
    }

    fn sample_memory_at(
        id: &str,
        ns: &str,
        title: &str,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> Memory {
        let ts = created_at.to_rfc3339();
        Memory {
            id: id.to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("content for {title} with some body"),
            tags: vec!["migrate-test".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: ts.clone(),
            updated_at: ts,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id":"ai:migrate-test"}),
        }
    }

    #[tokio::test]
    async fn open_store_sqlite_url() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        let url = format!("sqlite://{path}");
        let store = open_store(&url).await.expect("open sqlite store");
        let ctx = CallerContext::for_agent("ai:t");
        let mem = sample_memory("test-1", "ns", "hello");
        store.store(&ctx, &mem).await.expect("store");
        let got = store.get(&ctx, "test-1").await.expect("get");
        assert_eq!(got.title, "hello");
    }

    #[tokio::test]
    async fn open_store_rejects_unknown_scheme() {
        match open_store("nosql://not-supported").await {
            Err(e) => assert!(e.to_string().contains("unrecognised store URL")),
            Ok(_) => panic!("expected unrecognised-scheme error"),
        }
    }

    #[tokio::test]
    async fn migrate_sqlite_to_sqlite_roundtrip() {
        let src_tmp = tempfile::NamedTempFile::new().unwrap();
        let dst_tmp = tempfile::NamedTempFile::new().unwrap();
        let src = SqliteStore::open(src_tmp.path()).unwrap();
        let dst = SqliteStore::open(dst_tmp.path()).unwrap();
        let ctx = CallerContext::for_agent("ai:seed");
        let base = chrono::Utc::now() - chrono::Duration::hours(1);
        for i in 0..5 {
            let mem = sample_memory_at(
                &format!("m{i}"),
                "ns",
                &format!("title {i}"),
                base + chrono::Duration::seconds(i),
            );
            src.store(&ctx, &mem).await.unwrap();
        }
        let report = migrate(&src, &dst, 2, None, false).await;
        assert_eq!(report.memories_read, 5);
        assert_eq!(report.memories_written, 5);
        // v0.6.0 migrate is single-call (blocker #298 fix); batch_size
        // parameter retained for API compat but doesn't force pagination.
        assert_eq!(report.batches, 1);
        // Verify destination has them all.
        for i in 0..5 {
            let got = dst.get(&ctx, &format!("m{i}")).await.expect("get dst");
            assert_eq!(got.title, format!("title {i}"));
        }
    }

    #[tokio::test]
    async fn migrate_dry_run_does_not_write() {
        let src_tmp = tempfile::NamedTempFile::new().unwrap();
        let dst_tmp = tempfile::NamedTempFile::new().unwrap();
        let src = SqliteStore::open(src_tmp.path()).unwrap();
        let dst = SqliteStore::open(dst_tmp.path()).unwrap();
        let ctx = CallerContext::for_agent("ai:seed");
        for i in 0..3 {
            let mem = sample_memory(&format!("dm{i}"), "ns", &format!("dry {i}"));
            src.store(&ctx, &mem).await.unwrap();
        }
        let report = migrate(&src, &dst, 5, None, true).await;
        assert_eq!(report.memories_read, 3);
        assert_eq!(report.memories_written, 0);
        assert!(report.dry_run);
        // Destination should be empty.
        let err = dst.get(&ctx, "dm0").await.unwrap_err();
        assert!(matches!(err, crate::store::StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn migrate_is_idempotent_on_rerun() {
        let src_tmp = tempfile::NamedTempFile::new().unwrap();
        let dst_tmp = tempfile::NamedTempFile::new().unwrap();
        let src = SqliteStore::open(src_tmp.path()).unwrap();
        let dst = SqliteStore::open(dst_tmp.path()).unwrap();
        let ctx = CallerContext::for_agent("ai:seed");
        for i in 0..3 {
            let mem = sample_memory(&format!("im{i}"), "ns", &format!("idem {i}"));
            src.store(&ctx, &mem).await.unwrap();
        }
        let r1 = migrate(&src, &dst, 10, None, false).await;
        let r2 = migrate(&src, &dst, 10, None, false).await;
        assert_eq!(r1.memories_written, 3);
        assert_eq!(r2.memories_written, 3);
        assert!(r1.errors.is_empty() && r2.errors.is_empty());
    }

    #[tokio::test]
    async fn migrate_with_namespace_filter() {
        let src_tmp = tempfile::NamedTempFile::new().unwrap();
        let dst_tmp = tempfile::NamedTempFile::new().unwrap();
        let src = SqliteStore::open(src_tmp.path()).unwrap();
        let dst = SqliteStore::open(dst_tmp.path()).unwrap();
        let ctx = CallerContext::for_agent("ai:seed");
        let m_a = sample_memory("ns-m1", "wanted", "yes1");
        let m_b = sample_memory("ns-m2", "wanted", "yes2");
        let m_c = sample_memory("ns-m3", "other", "no");
        for m in [&m_a, &m_b, &m_c] {
            src.store(&ctx, m).await.unwrap();
        }
        let report = migrate(&src, &dst, 10, Some("wanted".to_string()), false).await;
        assert_eq!(report.memories_read, 2);
        assert_eq!(report.memories_written, 2);
        assert!(dst.get(&ctx, "ns-m1").await.is_ok());
        assert!(dst.get(&ctx, "ns-m2").await.is_ok());
        assert!(dst.get(&ctx, "ns-m3").await.is_err());
    }
}
