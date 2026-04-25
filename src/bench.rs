// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Pillar 3 / Stream E — `ai-memory bench` workload runner.
//!
//! Measures hot-path operations against the budgets published in
//! `PERFORMANCE.md` and returns p50/p95/p99 latencies plus a pass/fail
//! verdict per operation. The CI guard (Stream F) enforces the same
//! 10% p95 tolerance documented in `PERFORMANCE.md`.
//!
//! Coverage in this build:
//! - Embedding-free CRUD: `memory_store` (no embedding), `memory_search`
//!   (FTS5), `memory_recall` (hot, depth=1).
//! - Knowledge-graph traversal:
//!     - `memory_kg_query` (depth=1) and `memory_kg_timeline` against a
//!       fan-out fixture (50 sources × 4 outbound links each, every
//!       link `valid_from`-stamped).
//!     - `memory_kg_query` (depth=3, depth=5) against a chain fixture
//!       (50 chains × 5 hops each = 300 memories + 250 links). depth=3
//!       hits the "depth ≤ 3" 100 ms budget bucket; depth=5 hits the
//!       "depth ≤ 5" 250 ms tail-case bucket.
//!
//! Both fixtures live in the same in-process disposable `SQLite` — no
//! external service required.
//!
//! Embedding-bound paths (`memory_store` with embedding,
//! `memory_recall` cold/full hybrid) still require an embedder process
//! and are tracked as follow-up Stream E work — they don't belong on
//! the hot path of a `cargo test` invocation.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::time::{Duration, Instant};

use crate::db;
use crate::models::{Memory, Tier};

/// CI guard tolerance — measured p95 may exceed budget by this factor
/// before the run is marked `Fail`. Mirrors `PERFORMANCE.md`.
pub const P95_TOLERANCE: f64 = 1.10;

/// Default seeded namespace for the bench workload.
pub const BENCH_NAMESPACE: &str = "ai-memory-bench";

/// Default workload size — keep small enough for `cargo test`, large
/// enough that p99 has signal.
pub const DEFAULT_ITERATIONS: usize = 200;

/// Default warmup iterations discarded from the percentile sample.
pub const DEFAULT_WARMUP: usize = 20;

/// Hot-path operations covered by this iteration of the bench tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// `memory_store` without embedding — pure `SQLite` write path.
    StoreNoEmbedding,
    /// `memory_search` — FTS5 keyword baseline.
    SearchFts,
    /// `memory_recall` hot path, depth=1 (no hierarchy expansion).
    RecallHot,
    /// `memory_kg_query` recursive-CTE traversal at depth=1 (the
    /// shallowest path through the depth ≤ 3 budget bucket).
    KgQueryDepth1,
    /// `memory_kg_query` recursive-CTE traversal at depth=3 (the
    /// deepest path inside the "depth ≤ 3" 100 ms budget bucket). Driven
    /// against a chain fixture so the recursive CTE actually visits
    /// three hops per query.
    KgQueryDepth3,
    /// `memory_kg_query` recursive-CTE traversal at depth=5 (the tail
    /// case for the "depth ≤ 5" 250 ms budget bucket). Driven against
    /// the same chain fixture as depth=3.
    KgQueryDepth5,
    /// `memory_kg_timeline` — ordered timeline for a single source.
    KgTimeline,
}

impl Operation {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::StoreNoEmbedding => "memory_store (no embedding)",
            Self::SearchFts => "memory_search (FTS5)",
            Self::RecallHot => "memory_recall (hot, depth=1)",
            Self::KgQueryDepth1 => "memory_kg_query (depth=1)",
            Self::KgQueryDepth3 => "memory_kg_query (depth=3)",
            Self::KgQueryDepth5 => "memory_kg_query (depth=5)",
            Self::KgTimeline => "memory_kg_timeline",
        }
    }

    /// p95 budget in milliseconds, sourced from `PERFORMANCE.md`.
    ///
    /// `KgQueryDepth1` and `KgQueryDepth3` both fall in the
    /// "depth ≤ 3" (100 ms) bucket; `KgQueryDepth5` is the tail case
    /// at "depth ≤ 5" (250 ms). `SearchFts` and `KgTimeline` happen to
    /// share the same numeric budget as the depth ≤ 3 bucket despite
    /// belonging to different table rows in `PERFORMANCE.md`.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn target_p95_ms(self) -> f64 {
        match self {
            Self::StoreNoEmbedding => 20.0,
            Self::SearchFts => 100.0,
            Self::RecallHot => 50.0,
            Self::KgQueryDepth1 => 100.0,
            Self::KgQueryDepth3 => 100.0,
            Self::KgQueryDepth5 => 250.0,
            Self::KgTimeline => 100.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationResult {
    pub operation: Operation,
    /// Pretty label, duplicated for JSON consumers.
    pub label: &'static str,
    pub target_p95_ms: f64,
    pub measured_p50_ms: f64,
    pub measured_p95_ms: f64,
    pub measured_p99_ms: f64,
    pub samples: usize,
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub iterations: usize,
    pub warmup: usize,
    pub namespace: String,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            iterations: DEFAULT_ITERATIONS,
            warmup: DEFAULT_WARMUP,
            namespace: BENCH_NAMESPACE.to_string(),
        }
    }
}

/// Run the bench workload and return per-operation results.
///
/// Each operation seeds its own data inside the supplied connection so
/// callers can hand in either a fresh in-memory DB (for tests) or a
/// disposable on-disk DB (for the CLI).
///
/// # Errors
///
/// Returns the underlying [`db`] error if any of the seeded inserts
/// or queries fail.
pub fn run(conn: &Connection, config: &BenchConfig) -> Result<Vec<OperationResult>> {
    let store = run_store_no_embedding(conn, config)?;
    let search = run_search_fts(conn, config)?;
    let recall = run_recall_hot(conn, config)?;
    let kg_sources = seed_kg_fixture(conn, &config.namespace)?;
    let kg_query = run_kg_query_depth1(conn, config, &kg_sources)?;
    let kg_chain_sources = seed_kg_chain_fixture(conn, &config.namespace)?;
    let kg_query_d3 =
        run_kg_query_chain(conn, config, &kg_chain_sources, Operation::KgQueryDepth3, 3)?;
    let kg_query_d5 =
        run_kg_query_chain(conn, config, &kg_chain_sources, Operation::KgQueryDepth5, 5)?;
    let kg_timeline = run_kg_timeline(conn, config, &kg_sources)?;
    Ok(vec![
        store,
        search,
        recall,
        kg_query,
        kg_query_d3,
        kg_query_d5,
        kg_timeline,
    ])
}

fn run_store_no_embedding(conn: &Connection, config: &BenchConfig) -> Result<OperationResult> {
    let total = config.warmup + config.iterations;
    let mut samples = Vec::with_capacity(config.iterations);
    for i in 0..total {
        let mem = synth_memory(&config.namespace, i, "store");
        let start = Instant::now();
        db::insert(conn, &mem)?;
        let elapsed = start.elapsed();
        if i >= config.warmup {
            samples.push(elapsed);
        }
    }
    Ok(percentile_summary(Operation::StoreNoEmbedding, &samples))
}

fn run_search_fts(conn: &Connection, config: &BenchConfig) -> Result<OperationResult> {
    seed_corpus(conn, &config.namespace, "search", 200)?;
    let total = config.warmup + config.iterations;
    let mut samples = Vec::with_capacity(config.iterations);
    for i in 0..total {
        let query = format!("topic-{}", i % 50);
        let start = Instant::now();
        let _ = db::search(
            conn,
            &query,
            Some(&config.namespace),
            None,
            10,
            None,
            None,
            None,
            None,
            None,
            None,
        )?;
        let elapsed = start.elapsed();
        if i >= config.warmup {
            samples.push(elapsed);
        }
    }
    Ok(percentile_summary(Operation::SearchFts, &samples))
}

fn run_recall_hot(conn: &Connection, config: &BenchConfig) -> Result<OperationResult> {
    seed_corpus(conn, &config.namespace, "recall", 200)?;
    let warmup_query = "topic 0 category 0";
    for _ in 0..config.warmup {
        let _ = db::recall(
            conn,
            warmup_query,
            Some(&config.namespace),
            10,
            None,
            None,
            None,
            0,
            0,
            None,
            None,
        )?;
    }
    let mut samples = Vec::with_capacity(config.iterations);
    for i in 0..config.iterations {
        let query = format!("topic {} category {}", i % 50, i % 10);
        let start = Instant::now();
        let _ = db::recall(
            conn,
            &query,
            Some(&config.namespace),
            10,
            None,
            None,
            None,
            0,
            0,
            None,
            None,
        )?;
        samples.push(start.elapsed());
    }
    Ok(percentile_summary(Operation::RecallHot, &samples))
}

/// Source memory IDs returned from [`seed_kg_fixture`]. Each source has
/// `KG_FIXTURE_LINKS_PER_SOURCE` outbound links — the bench drives both
/// `kg_query` and `kg_timeline` against the same fixture.
const KG_FIXTURE_SOURCES: usize = 50;
const KG_FIXTURE_LINKS_PER_SOURCE: usize = 4;

/// Linear-chain fixture geometry for the depth=3 / depth=5 runners.
/// `KG_CHAIN_FIXTURE_CHAINS` chains × `KG_CHAIN_FIXTURE_HOPS` hops yields
/// `chains * (hops + 1)` memories and `chains * hops` links — so 50 × 5
/// matches the fan-out fixture's order of magnitude (300 memories +
/// 250 links). depth=5 reaches every node in a chain; depth=3 reaches
/// the first three follow-on hops.
const KG_CHAIN_FIXTURE_CHAINS: usize = 50;
const KG_CHAIN_FIXTURE_HOPS: usize = 5;

fn run_kg_query_depth1(
    conn: &Connection,
    config: &BenchConfig,
    sources: &[String],
) -> Result<OperationResult> {
    debug_assert!(
        !sources.is_empty(),
        "kg_query bench requires a seeded fixture"
    );
    let total = config.warmup + config.iterations;
    let mut samples = Vec::with_capacity(config.iterations);
    for i in 0..total {
        let src = &sources[i % sources.len()];
        let start = Instant::now();
        let _ = db::kg_query(conn, src, 1, None, None, None)?;
        let elapsed = start.elapsed();
        if i >= config.warmup {
            samples.push(elapsed);
        }
    }
    Ok(percentile_summary(Operation::KgQueryDepth1, &samples))
}

fn run_kg_query_chain(
    conn: &Connection,
    config: &BenchConfig,
    sources: &[String],
    operation: Operation,
    max_depth: usize,
) -> Result<OperationResult> {
    debug_assert!(
        !sources.is_empty(),
        "kg_query chain bench requires a seeded fixture"
    );
    let total = config.warmup + config.iterations;
    let mut samples = Vec::with_capacity(config.iterations);
    for i in 0..total {
        let src = &sources[i % sources.len()];
        let start = Instant::now();
        let _ = db::kg_query(conn, src, max_depth, None, None, None)?;
        let elapsed = start.elapsed();
        if i >= config.warmup {
            samples.push(elapsed);
        }
    }
    Ok(percentile_summary(operation, &samples))
}

fn run_kg_timeline(
    conn: &Connection,
    config: &BenchConfig,
    sources: &[String],
) -> Result<OperationResult> {
    debug_assert!(
        !sources.is_empty(),
        "kg_timeline bench requires a seeded fixture"
    );
    let total = config.warmup + config.iterations;
    let mut samples = Vec::with_capacity(config.iterations);
    for i in 0..total {
        let src = &sources[i % sources.len()];
        let start = Instant::now();
        let _ = db::kg_timeline(conn, src, None, None, None)?;
        let elapsed = start.elapsed();
        if i >= config.warmup {
            samples.push(elapsed);
        }
    }
    Ok(percentile_summary(Operation::KgTimeline, &samples))
}

/// Seed the in-process KG fixture: `KG_FIXTURE_SOURCES` source memories,
/// each with `KG_FIXTURE_LINKS_PER_SOURCE` outbound links to distinct
/// targets. Every link sets `valid_from` so `kg_timeline` (which skips
/// rows with NULL `valid_from`) sees the full corpus. Returns the source
/// IDs so the runners can hand them to `kg_query` / `kg_timeline`.
fn seed_kg_fixture(conn: &Connection, namespace: &str) -> Result<Vec<String>> {
    let mut sources = Vec::with_capacity(KG_FIXTURE_SOURCES);
    for s in 0..KG_FIXTURE_SOURCES {
        let src = synth_memory(namespace, s, "kg-src");
        // `db::insert` upserts on `(title, namespace)` and returns the
        // canonical id, which differs from `src.id` if the row already
        // exists. Use the returned id so the fixture remains correct
        // even when `run()` is invoked twice against the same conn.
        let src_id = db::insert(conn, &src)?;
        for t in 0..KG_FIXTURE_LINKS_PER_SOURCE {
            let target_idx = s * KG_FIXTURE_LINKS_PER_SOURCE + t;
            let tgt = synth_memory(namespace, target_idx, "kg-tgt");
            let tgt_id = db::insert(conn, &tgt)?;
            // `db::create_link` stamps `created_at` and `valid_from` to
            // the current wall clock — sufficient for `kg_timeline`
            // (which skips rows with NULL `valid_from`).
            db::create_link(conn, &src_id, &tgt_id, "related_to")?;
        }
        sources.push(src_id);
    }
    Ok(sources)
}

/// Seed the linear-chain KG fixture used by the depth=3 / depth=5
/// runners: `KG_CHAIN_FIXTURE_CHAINS` chains, each
/// `KG_CHAIN_FIXTURE_HOPS` links long. Every node and link uses titles
/// disjoint from the fan-out fixture's `kg-src` / `kg-tgt` prefixes, so
/// both fixtures coexist in the same connection without colliding on
/// the `(title, namespace)` upsert. Returns the source IDs (one per
/// chain) so the runners can drive `kg_query` against them.
fn seed_kg_chain_fixture(conn: &Connection, namespace: &str) -> Result<Vec<String>> {
    let mut sources = Vec::with_capacity(KG_CHAIN_FIXTURE_CHAINS);
    for c in 0..KG_CHAIN_FIXTURE_CHAINS {
        let mut prev_id = {
            let head = synth_memory(namespace, c, "kg-chain-src");
            db::insert(conn, &head)?
        };
        let chain_head_id = prev_id.clone();
        for h in 0..KG_CHAIN_FIXTURE_HOPS {
            let node_idx = c * KG_CHAIN_FIXTURE_HOPS + h;
            let next = synth_memory(namespace, node_idx, "kg-chain-node");
            let next_id = db::insert(conn, &next)?;
            db::create_link(conn, &prev_id, &next_id, "related_to")?;
            prev_id = next_id;
        }
        sources.push(chain_head_id);
    }
    Ok(sources)
}

fn seed_corpus(conn: &Connection, namespace: &str, prefix: &str, count: usize) -> Result<()> {
    for i in 0..count {
        let mem = synth_memory(namespace, i, prefix);
        db::insert(conn, &mem)?;
    }
    Ok(())
}

fn synth_memory(namespace: &str, i: usize, prefix: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: namespace.to_string(),
        title: format!("bench-{prefix}-{i}"),
        content: format!(
            "bench memory {i} content about topic {} category {} for {prefix} workload",
            i % 50,
            i % 10
        ),
        tags: vec![],
        priority: i32::try_from((i % 9) + 1).unwrap_or(5),
        confidence: 1.0,
        source: "bench".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "bench"}),
    }
}

fn percentile_summary(operation: Operation, samples: &[Duration]) -> OperationResult {
    debug_assert!(
        !samples.is_empty(),
        "bench operation produced no samples; iterations must be > 0"
    );
    let mut sorted: Vec<f64> = samples.iter().map(duration_ms).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = percentile(&sorted, 0.50);
    let p95 = percentile(&sorted, 0.95);
    let p99 = percentile(&sorted, 0.99);
    let target = operation.target_p95_ms();
    let status = if p95 <= target * P95_TOLERANCE {
        Status::Pass
    } else {
        Status::Fail
    };
    OperationResult {
        operation,
        label: operation.label(),
        target_p95_ms: target,
        measured_p50_ms: p50,
        measured_p95_ms: p95,
        measured_p99_ms: p99,
        samples: sorted.len(),
        status,
    }
}

fn duration_ms(d: &Duration) -> f64 {
    let secs = d.as_secs_f64();
    secs * 1000.0
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = q * (sorted.len() as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = rank - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Render a results table to a string in the same shape used in the
/// `PERFORMANCE.md` "Operator Self-Verification" example.
#[must_use]
pub fn render_table(results: &[OperationResult]) -> String {
    let mut out = String::new();
    out.push_str(
        "Operation                       Target (p95)   Measured (p95)   p50      p99      Status\n",
    );
    out.push_str(
        "─────────────────────────────────────────────────────────────────────────────────────────\n",
    );
    for r in results {
        let status_str = match r.status {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
        };
        // target budgets are documented as small integer ms; rounding
        // to the nearest int ms is what the table in PERFORMANCE.md
        // shows. Saturating cast guards against pathological future
        // changes to a non-integer or huge value.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target_ms = r.target_p95_ms.round() as i64;
        let line = format!(
            "{:<30}  < {:>4} ms       {:>7.1} ms       {:>5.1}    {:>5.1}    {}\n",
            r.label, target_ms, r.measured_p95_ms, r.measured_p50_ms, r.measured_p99_ms, status_str
        );
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn fresh_conn() -> Connection {
        db::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn small_config() -> BenchConfig {
        BenchConfig {
            iterations: 30,
            warmup: 5,
            namespace: "bench-test".to_string(),
        }
    }

    #[test]
    fn percentile_interpolates() {
        let s = vec![1.0, 2.0, 3.0, 4.0];
        assert!((percentile(&s, 0.50) - 2.5).abs() < 1e-9);
        assert!((percentile(&s, 0.0) - 1.0).abs() < 1e-9);
        assert!((percentile(&s, 1.0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn percentile_handles_singleton_and_empty() {
        assert!((percentile(&[], 0.5) - 0.0).abs() < 1e-9);
        assert!((percentile(&[42.0], 0.99) - 42.0).abs() < 1e-9);
    }

    #[test]
    fn run_returns_all_seven_results() {
        let conn = fresh_conn();
        let results = run(&conn, &small_config()).unwrap();
        assert_eq!(results.len(), 7);
        assert_eq!(results[0].operation, Operation::StoreNoEmbedding);
        assert_eq!(results[1].operation, Operation::SearchFts);
        assert_eq!(results[2].operation, Operation::RecallHot);
        assert_eq!(results[3].operation, Operation::KgQueryDepth1);
        assert_eq!(results[4].operation, Operation::KgQueryDepth3);
        assert_eq!(results[5].operation, Operation::KgQueryDepth5);
        assert_eq!(results[6].operation, Operation::KgTimeline);
        for r in &results {
            assert_eq!(r.samples, 30);
            assert!(r.measured_p50_ms <= r.measured_p95_ms);
            assert!(r.measured_p95_ms <= r.measured_p99_ms);
            assert!(r.target_p95_ms > 0.0);
        }
    }

    #[test]
    fn status_is_fail_when_p95_over_tolerance() {
        let r = OperationResult {
            operation: Operation::StoreNoEmbedding,
            label: Operation::StoreNoEmbedding.label(),
            target_p95_ms: 20.0,
            measured_p50_ms: 5.0,
            measured_p95_ms: 25.0,
            measured_p99_ms: 30.0,
            samples: 100,
            status: Status::Fail,
        };
        assert_eq!(r.status, Status::Fail);
        // 25 > 20 * 1.10 = 22 → Fail
        let recomputed = if 25.0_f64 <= 20.0 * P95_TOLERANCE {
            Status::Pass
        } else {
            Status::Fail
        };
        assert_eq!(recomputed, Status::Fail);
    }

    #[test]
    fn status_is_pass_within_tolerance() {
        // 21 ms over 20 ms budget = 5% over → still PASS (under 10%).
        let recomputed = if 21.0_f64 <= 20.0 * P95_TOLERANCE {
            Status::Pass
        } else {
            Status::Fail
        };
        assert_eq!(recomputed, Status::Pass);
    }

    #[test]
    fn render_table_includes_all_operations() {
        let conn = fresh_conn();
        let results = run(&conn, &small_config()).unwrap();
        let table = render_table(&results);
        assert!(table.contains("memory_store (no embedding)"));
        assert!(table.contains("memory_search (FTS5)"));
        assert!(table.contains("memory_recall (hot, depth=1)"));
        assert!(table.contains("memory_kg_query (depth=1)"));
        assert!(table.contains("memory_kg_query (depth=3)"));
        assert!(table.contains("memory_kg_query (depth=5)"));
        assert!(table.contains("memory_kg_timeline"));
        assert!(table.contains("Status"));
    }

    #[test]
    fn operation_targets_match_performance_md() {
        // Pinned to PERFORMANCE.md — if you change a budget, change both.
        assert!((Operation::StoreNoEmbedding.target_p95_ms() - 20.0).abs() < 1e-9);
        assert!((Operation::SearchFts.target_p95_ms() - 100.0).abs() < 1e-9);
        assert!((Operation::RecallHot.target_p95_ms() - 50.0).abs() < 1e-9);
        assert!((Operation::KgQueryDepth1.target_p95_ms() - 100.0).abs() < 1e-9);
        assert!((Operation::KgQueryDepth3.target_p95_ms() - 100.0).abs() < 1e-9);
        assert!((Operation::KgQueryDepth5.target_p95_ms() - 250.0).abs() < 1e-9);
        assert!((Operation::KgTimeline.target_p95_ms() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn seed_kg_chain_fixture_traverses_to_max_depth() {
        let conn = fresh_conn();
        let sources = seed_kg_chain_fixture(&conn, "kg-chain-fixture-test").unwrap();
        assert_eq!(sources.len(), KG_CHAIN_FIXTURE_CHAINS);
        // Every chain must yield exactly `KG_CHAIN_FIXTURE_HOPS` reachable
        // nodes at depth=KG_CHAIN_FIXTURE_HOPS — that's what justifies the
        // depth=5 budget bucket. depth=3 must reach exactly 3 nodes.
        for src in &sources {
            let depth5 = db::kg_query(&conn, src, KG_CHAIN_FIXTURE_HOPS, None, None, None).unwrap();
            assert_eq!(
                depth5.len(),
                KG_CHAIN_FIXTURE_HOPS,
                "depth={KG_CHAIN_FIXTURE_HOPS} on a {KG_CHAIN_FIXTURE_HOPS}-hop chain must reach every node"
            );
            let depth3 = db::kg_query(&conn, src, 3, None, None, None).unwrap();
            assert_eq!(
                depth3.len(),
                3,
                "depth=3 on a {KG_CHAIN_FIXTURE_HOPS}-hop chain must reach exactly 3 follow-on nodes"
            );
        }
    }

    #[test]
    fn seed_kg_fixture_populates_sources_and_links() {
        let conn = fresh_conn();
        let sources = seed_kg_fixture(&conn, "kg-fixture-test").unwrap();
        assert_eq!(sources.len(), KG_FIXTURE_SOURCES);
        // Every source carries the expected fan-out, every link has a
        // non-null `valid_from` (otherwise `kg_timeline` would skip it).
        for src in &sources {
            let nodes = db::kg_query(&conn, src, 1, None, None, None).unwrap();
            assert_eq!(nodes.len(), KG_FIXTURE_LINKS_PER_SOURCE);
            let timeline = db::kg_timeline(&conn, src, None, None, None).unwrap();
            assert_eq!(timeline.len(), KG_FIXTURE_LINKS_PER_SOURCE);
            for ev in &timeline {
                // `kg_timeline` filters out NULL `valid_from` rows in SQL,
                // so any returned event must carry a non-empty stamp.
                assert!(
                    !ev.valid_from.is_empty(),
                    "kg fixture must stamp valid_from on every link"
                );
            }
        }
    }
}
