// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 F6 Gap 2 — `memory_links` survive a cross-backend round trip.
//!
//! Seeds a `SQLite` source with 10 memories and 20 links (varied
//! relations, includes a cycle and a depth-5 chain), migrates to
//! Postgres, then migrates back to a fresh `SQLite`. Asserts:
//!
//! - memory count matches at each leg,
//! - link count matches at each leg,
//! - per-leg edge fingerprints match (sha256 of sorted
//!   `(source_id, target_id, relation)` triples).
//!
//! The Postgres leg requires `AI_MEMORY_TEST_POSTGRES_URL`. When the
//! env-var is absent the test still exercises the `SQLite` → `SQLite`
//! round-trip so the link migration logic gets coverage on every CI
//! run; the Postgres-bridge assertions just skip with `eprintln!`.

#![cfg(feature = "sal")]

use ai_memory::migrate;
use ai_memory::models::{Memory, MemoryLink, Tier};
use ai_memory::store::sqlite::SqliteStore;
use ai_memory::store::{CallerContext, MemoryStore};

fn ctx() -> CallerContext {
    CallerContext::for_agent("ai:migrate-roundtrip")
}

fn now_rfc() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn seed_memory(id: &str, ns: &str, title: &str) -> Memory {
    let now = now_rfc();
    Memory {
        id: id.to_string(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: format!("body for {title}"),
        tags: vec!["roundtrip".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id":"ai:migrate-roundtrip"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

/// Hash the canonical `(source_id, target_id, relation)` triples of a
/// link set so two different backends can be compared byte-equivalently.
/// Not crypto-strong; just a stable equality fingerprint.
fn fingerprint_links(links: &[MemoryLink]) -> String {
    use std::hash::{Hash, Hasher};
    let mut tuples: Vec<(String, String, String)> = links
        .iter()
        // v0.7.0 fix campaign R1-M4 — relation is now `Copy` enum;
        // project to its canonical wire string for the fingerprint
        // tuple shape used in the rest of this file.
        .map(|l| {
            (
                l.source_id.clone(),
                l.target_id.clone(),
                l.relation.as_str().to_string(),
            )
        })
        .collect();
    tuples.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for t in &tuples {
        t.hash(&mut hasher);
    }
    format!("{:016x}-count={}", hasher.finish(), tuples.len())
}

fn link(src: &str, dst: &str, rel: &str) -> MemoryLink {
    MemoryLink {
        source_id: src.to_string(),
        target_id: dst.to_string(),
        // v0.7.0 fix campaign R1-M4 — typed relation closed-set.
        relation: ai_memory::models::MemoryLinkRelation::from_str(rel)
            .expect("test fixture relation must be one of the closed-set variants"),
        created_at: now_rfc(),
        signature: None,
        observed_by: None,
        valid_from: None,
        valid_until: None,
    }
}

/// Seed 10 memories + 20 links into `store`. The link topology
/// includes:
///
/// - a 5-deep chain: m0 -> m1 -> m2 -> m3 -> m4 -> m5,
/// - a cycle: m6 -> m7 -> m6,
/// - mixed relations: `related_to` / `supersedes` / `contradicts` /
///   `derived_from`.
async fn seed_corpus(store: &dyn MemoryStore) {
    let ns = "roundtrip";
    for i in 0..10 {
        let id = format!("m{i}");
        let mem = seed_memory(&id, ns, &format!("title-{i}"));
        store.store(&ctx(), &mem).await.expect("seed memory");
    }
    // 5-deep chain
    for i in 0..5 {
        store
            .link(
                &ctx(),
                &link(&format!("m{i}"), &format!("m{}", i + 1), "related_to"),
            )
            .await
            .expect("seed chain link");
    }
    // Cycle
    store
        .link(&ctx(), &link("m6", "m7", "related_to"))
        .await
        .expect("seed cycle a");
    store
        .link(&ctx(), &link("m7", "m6", "related_to"))
        .await
        .expect("seed cycle b");
    // Various relations & cross-edges to total 20 links
    let edges = [
        ("m0", "m2", "supersedes"),
        ("m1", "m3", "contradicts"),
        ("m2", "m4", "derived_from"),
        ("m3", "m5", "related_to"),
        ("m4", "m6", "related_to"),
        ("m5", "m7", "related_to"),
        ("m6", "m8", "supersedes"),
        ("m7", "m9", "derived_from"),
        ("m8", "m9", "related_to"),
        ("m9", "m0", "contradicts"), // back-edge to source
        ("m0", "m9", "related_to"),
        ("m1", "m8", "supersedes"),
        ("m2", "m9", "derived_from"),
    ];
    for (s, t, r) in edges {
        store.link(&ctx(), &link(s, t, r)).await.expect("seed edge");
    }
}

#[tokio::test]
async fn migrate_links_sqlite_to_sqlite_roundtrip() {
    // Baseline: SQLite -> SQLite preserves the link set byte-for-byte.
    // This branch always runs (no live PG required).
    let src_tmp = tempfile::NamedTempFile::new().unwrap();
    let dst_tmp = tempfile::NamedTempFile::new().unwrap();
    let src = SqliteStore::open(src_tmp.path()).unwrap();
    let dst = SqliteStore::open(dst_tmp.path()).unwrap();

    seed_corpus(&src).await;

    let src_links = src.list_links(None).await.expect("src list_links");
    assert_eq!(
        src_links.len(),
        20,
        "seed corpus must produce exactly 20 links"
    );
    let src_fp = fingerprint_links(&src_links);

    let report = migrate::migrate(&src, &dst, 10, None, false).await;
    assert!(
        report.errors.is_empty(),
        "migration must complete cleanly, errors={:?}",
        report.errors
    );
    assert_eq!(
        report.memories_read, report.memories_written,
        "every memory must transfer"
    );
    assert_eq!(report.links_read, 20);
    assert_eq!(
        report.links_read,
        report.links_written + report.links_skipped,
        "links_read must equal written + skipped"
    );
    assert_eq!(
        report.links_skipped, 0,
        "fresh destination must skip zero links"
    );

    let dst_links = dst.list_links(None).await.expect("dst list_links");
    assert_eq!(dst_links.len(), 20, "destination must hold all 20 links");
    assert_eq!(fingerprint_links(&dst_links), src_fp);
}

#[tokio::test]
async fn migrate_links_idempotent_replay_reports_skipped() {
    // Re-running the migration must surface every link as
    // `links_skipped` rather than `links_written`, matching the
    // `INSERT OR IGNORE` / `ON CONFLICT DO NOTHING` semantics.
    let src_tmp = tempfile::NamedTempFile::new().unwrap();
    let dst_tmp = tempfile::NamedTempFile::new().unwrap();
    let src = SqliteStore::open(src_tmp.path()).unwrap();
    let dst = SqliteStore::open(dst_tmp.path()).unwrap();
    seed_corpus(&src).await;

    let r1 = migrate::migrate(&src, &dst, 10, None, false).await;
    assert!(r1.errors.is_empty());
    assert_eq!(r1.links_written, 20);
    assert_eq!(r1.links_skipped, 0);

    let r2 = migrate::migrate(&src, &dst, 10, None, false).await;
    assert!(r2.errors.is_empty());
    assert_eq!(r2.links_read, 20);
    assert_eq!(
        r2.links_skipped, 20,
        "second migrate must report every link as skipped"
    );
    assert_eq!(
        r2.links_written, 0,
        "second migrate must not double-count writes"
    );
}

#[tokio::test]
async fn migrate_links_dry_run_skips_writes_but_reports_reads() {
    // Dry-run still tallies `links_read` so operators can size the
    // migration before committing — but no destination side-effect.
    let src_tmp = tempfile::NamedTempFile::new().unwrap();
    let dst_tmp = tempfile::NamedTempFile::new().unwrap();
    let src = SqliteStore::open(src_tmp.path()).unwrap();
    let dst = SqliteStore::open(dst_tmp.path()).unwrap();
    seed_corpus(&src).await;

    let report = migrate::migrate(&src, &dst, 10, None, true).await;
    assert!(report.errors.is_empty());
    assert_eq!(report.links_read, 20);
    assert_eq!(report.links_written, 0);
    assert_eq!(report.links_skipped, 0);

    let dst_links = dst.list_links(None).await.expect("dst list_links");
    assert!(dst_links.is_empty(), "dry-run must not touch destination");
}

#[cfg(feature = "sal-postgres")]
#[tokio::test]
async fn migrate_links_sqlite_to_postgres_to_sqlite_roundtrip() {
    // SQLite → Postgres → SQLite. The two SQLite leg fingerprints must
    // be identical, proving the link set survives a full cross-backend
    // round trip.
    use ai_memory::store::postgres::PostgresStore;

    let Ok(pg_url) = std::env::var("AI_MEMORY_TEST_POSTGRES_URL") else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let src_tmp = tempfile::NamedTempFile::new().unwrap();
    let dst_tmp = tempfile::NamedTempFile::new().unwrap();
    let src = SqliteStore::open(src_tmp.path()).unwrap();
    let dst_sqlite = SqliteStore::open(dst_tmp.path()).unwrap();
    let pg = PostgresStore::connect(&pg_url).await.expect("connect pg");

    // Pre-clean the live PG so a previous run's seed corpus doesn't
    // leak into this run's count assertions. We delete only memories
    // in our test namespace; cascade drops the linked rows.
    sqlx_cleanup_namespace(&pg_url, "roundtrip").await;

    seed_corpus(&src).await;
    let src_fp = fingerprint_links(&src.list_links(None).await.unwrap());

    // Leg 1: SQLite -> Postgres.
    let r1 = migrate::migrate(&src, &pg, 10, Some("roundtrip".to_string()), false).await;
    assert!(r1.errors.is_empty(), "leg 1 errors: {:?}", r1.errors);
    assert_eq!(r1.links_read, 20);
    assert_eq!(r1.links_written, 20);

    let pg_links = pg
        .list_links(Some("roundtrip"))
        .await
        .expect("pg list_links");
    assert_eq!(pg_links.len(), 20);
    let pg_fp = fingerprint_links(&pg_links);
    assert_eq!(pg_fp, src_fp, "Postgres leg must mirror source fingerprint");

    // Leg 2: Postgres -> SQLite.
    let r2 = migrate::migrate(&pg, &dst_sqlite, 10, Some("roundtrip".to_string()), false).await;
    assert!(r2.errors.is_empty(), "leg 2 errors: {:?}", r2.errors);
    assert_eq!(r2.links_read, 20);
    assert_eq!(r2.links_written, 20);

    let dst_links = dst_sqlite
        .list_links(None)
        .await
        .expect("dst sqlite list_links");
    assert_eq!(dst_links.len(), 20);
    assert_eq!(
        fingerprint_links(&dst_links),
        src_fp,
        "round-trip fingerprint must equal the original SQLite fingerprint"
    );
}

#[cfg(feature = "sal-postgres")]
async fn sqlx_cleanup_namespace(pg_url: &str, namespace: &str) {
    // Best-effort delete for test isolation. Failures are tolerated
    // because the test fixtures we ship may not have this namespace
    // yet on a fresh Postgres.
    let pool = sqlx::PgPool::connect(pg_url)
        .await
        .expect("connect for cleanup");
    let _ = sqlx::query("DELETE FROM memories WHERE namespace = $1")
        .bind(namespace)
        .execute(&pool)
        .await;
}
