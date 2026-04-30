// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.6.3.1 Phase P3 — recall observability acceptance tests.
//!
//! Closes audit gaps G2 (HNSW silent eviction at 100k), G8 (reranker
//! silent fallback to lexical), and G11 (embedder silent degrade to
//! keyword-only) by asserting that every silent-degrade path becomes
//! observable in the `memory_recall` response `meta` block AND in the
//! process-local counters surfaced via `db::stats` /
//! `handle_capabilities`.
//!
//! Run with:
//!     AI_MEMORY_NO_CONFIG=1 cargo test --test recall_observability
//!
//! The tests run end-to-end through the public `mcp::handle_recall`
//! handler so the wire shape (the JSON `meta` block) is asserted exactly
//! as a remote MCP client would see it. No fixtures are imported from
//! the daemon — each test seeds an in-memory SQLite DB with the minimal
//! row set it needs.

use ai_memory::config::{ResolvedScoring, ResolvedTtl};
use ai_memory::db;
use ai_memory::hnsw::VectorIndex;
use ai_memory::models::{Memory, Tier};
use ai_memory::reranker::CrossEncoder;
use serde_json::json;

/// Build a fresh on-disk SQLite DB with the canonical schema applied.
/// Returns `(connection, temp-path)`. The path is held by the caller so
/// the file outlives the connection.
fn fresh_db() -> (rusqlite::Connection, std::path::PathBuf) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ai-memory-recall-obs-{}.db", uuid::Uuid::new_v4()));
    let conn = db::open(&path).expect("open fresh test db");
    (conn, path)
}

/// Minimal `Memory` factory — unique title per call to avoid the
/// (title, namespace) UPSERT collapse.
fn make_memory(title: &str, content: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: format!("test-{}", uuid::Uuid::new_v4()),
        tier: Tier::Long,
        namespace: "test".to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
    }
}

// ---------------------------------------------------------------------------
// G11 — embedder silent degrade to keyword-only
// ---------------------------------------------------------------------------

#[test]
fn recall_response_meta_reports_keyword_only_when_embedder_disabled() {
    let (conn, _path) = fresh_db();
    // Seed two memories the FTS5 stage can match against the query.
    db::insert(
        &conn,
        &make_memory(
            "Rust ownership",
            "Rust ownership prevents data races at compile time.",
        ),
    )
    .expect("insert");
    db::insert(
        &conn,
        &make_memory(
            "Python typing",
            "Python typing is dynamic with optional gradual hints.",
        ),
    )
    .expect("insert");

    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();

    // Embedder = None simulates the "embedder failed at startup" or
    // "keyword tier configured" case — `handle_recall` must fall through
    // to keyword-only and report `recall_mode = "keyword_only"`.
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "ownership", "namespace": "test"}),
        None, // embedder disabled
        None, // no vector index
        None, // no reranker
        false,
        &ttl,
        &scoring,
    )
    .expect("recall");

    let meta = resp
        .get("meta")
        .expect("response.meta is required (P3 closure)");
    assert_eq!(
        meta["recall_mode"].as_str(),
        Some("keyword_only"),
        "embedder=None must surface keyword_only mode in meta"
    );
    assert_eq!(
        meta["reranker_used"].as_str(),
        Some("none"),
        "reranker=None must surface 'none' (not 'lexical')"
    );
    // candidate_counts.fts must reflect what FTS actually returned.
    let fts = meta["candidate_counts"]["fts"]
        .as_u64()
        .expect("candidate_counts.fts must be present and numeric");
    assert!(
        fts >= 1,
        "FTS should have matched at least one of the seeded memories (got {fts})"
    );
    assert_eq!(
        meta["candidate_counts"]["hnsw"].as_u64(),
        Some(0),
        "keyword_only mode must report hnsw=0 (no semantic stage ran)"
    );
    assert!(
        (meta["blend_weight"].as_f64().expect("blend_weight numeric") - 0.0).abs() < f64::EPSILON,
        "keyword_only mode must report blend_weight=0.0"
    );
}

// ---------------------------------------------------------------------------
// G8 — reranker silent fallback to lexical Jaccard
// ---------------------------------------------------------------------------

#[test]
fn recall_response_meta_reports_lexical_when_neural_unavailable() {
    let (conn, _path) = fresh_db();
    db::insert(
        &conn,
        &make_memory(
            "Async Rust runtime",
            "Tokio is the dominant async runtime in the Rust ecosystem.",
        ),
    )
    .expect("insert");

    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();

    // CrossEncoder::new() returns the Lexical variant — exercise the
    // path where a reranker IS configured but it's the lexical fallback
    // (the same shape produced by `new_neural()` when the BERT download
    // fails). `handle_recall` must surface `reranker_used = "lexical"`.
    let lexical = CrossEncoder::new();
    assert!(
        !lexical.is_neural(),
        "test precondition: CrossEncoder::new() must be Lexical"
    );
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "rust runtime", "namespace": "test"}),
        None,
        None,
        Some(&lexical),
        false,
        &ttl,
        &scoring,
    )
    .expect("recall");

    let meta = resp.get("meta").expect("response.meta required");
    assert_eq!(
        meta["reranker_used"].as_str(),
        Some("lexical"),
        "lexical CrossEncoder must surface as 'lexical' (G8 closure)"
    );
    // Reranker present + embedder absent still reports keyword_only —
    // the meta block reports each axis independently.
    assert_eq!(
        meta["recall_mode"].as_str(),
        Some("keyword_only"),
        "embedder=None still reports keyword_only even with reranker on"
    );
}

// ---------------------------------------------------------------------------
// G2 — HNSW silent oldest-eviction
// ---------------------------------------------------------------------------

#[test]
fn hnsw_eviction_increments_counter() {
    // Reset the process-local counter so concurrent tests don't bleed.
    ai_memory::hnsw::reset_eviction_counters_for_test();
    assert_eq!(
        ai_memory::hnsw::index_evictions_total(),
        0,
        "test precondition: counter starts at 0 after reset"
    );
    assert!(
        !ai_memory::hnsw::evicted_recently(60),
        "test precondition: no recent evictions after reset"
    );

    // The MAX_ENTRIES cap is 100_000 — building the index at the cap and
    // inserting one more row trips the eviction path. We seed the build
    // call with `MAX_ENTRIES` synthetic vectors (2-d, L2-normalized to
    // satisfy the cosine-distance contract) so the `insert` exceeds the
    // cap by exactly one and we can assert the counter delta precisely.
    const MAX_ENTRIES: usize = 100_000;
    let entries: Vec<(String, Vec<f32>)> = (0..MAX_ENTRIES)
        .map(|i| {
            // 2-d L2-normalized vector with a small per-i variation so
            // build_hnsw doesn't degenerate on identical points.
            #[allow(clippy::cast_precision_loss)]
            let theta = (i as f32).mul_add(0.0001, 0.001);
            let v = vec![theta.cos(), theta.sin()];
            (format!("seed-{i}"), v)
        })
        .collect();
    let idx = VectorIndex::build(entries);
    assert_eq!(idx.len(), MAX_ENTRIES, "index should be full at cap");

    // One more insert — must trip the eviction branch and bump the
    // counter by exactly 1.
    idx.insert("overflow-1".to_string(), vec![1.0_f32, 0.0]);

    let after = ai_memory::hnsw::index_evictions_total();
    assert_eq!(
        after, 1,
        "single overflow insert must evict exactly one entry (got {after})"
    );
    assert!(
        ai_memory::hnsw::evicted_recently(60),
        "evicted_recently(60s) must be true immediately after eviction"
    );

    // The same counter must surface through `db::stats` so a `memory_stats`
    // RPC sees it.
    let (conn, db_path) = fresh_db();
    let stats = db::stats(&conn, &db_path).expect("stats");
    assert_eq!(
        stats.index_evictions_total, after,
        "db::stats must report the same process-local counter"
    );
}
