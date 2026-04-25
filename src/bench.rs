// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Pillar 3 / Stream E — `ai-memory bench` workload runner.
//!
//! Measures hot-path operations against the budgets published in
//! `PERFORMANCE.md` and returns p50/p95/p99 latencies plus a pass/fail
//! verdict per operation. The CI guard (Stream F) enforces the same
//! 10% p95 tolerance documented in `PERFORMANCE.md`.
//!
//! This iteration covers the three embedding-free operations
//! (`memory_store` no-embedding, `memory_search` FTS5,
//! `memory_recall` hot depth=1). Embedding-bound and KG operations are
//! tracked as follow-up work — they need an embedder process and a
//! seeded KG fixture, neither of which belongs on the hot path of a
//! `cargo test` invocation.

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
}

impl Operation {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::StoreNoEmbedding => "memory_store (no embedding)",
            Self::SearchFts => "memory_search (FTS5)",
            Self::RecallHot => "memory_recall (hot, depth=1)",
        }
    }

    /// p95 budget in milliseconds, sourced from `PERFORMANCE.md`.
    #[must_use]
    pub fn target_p95_ms(self) -> f64 {
        match self {
            Self::StoreNoEmbedding => 20.0,
            Self::SearchFts => 100.0,
            Self::RecallHot => 50.0,
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
    Ok(vec![store, search, recall])
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
    fn run_returns_three_results() {
        let conn = fresh_conn();
        let results = run(&conn, &small_config()).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].operation, Operation::StoreNoEmbedding);
        assert_eq!(results[1].operation, Operation::SearchFts);
        assert_eq!(results[2].operation, Operation::RecallHot);
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
        assert!(table.contains("Status"));
    }

    #[test]
    fn operation_targets_match_performance_md() {
        // Pinned to PERFORMANCE.md — if you change a budget, change both.
        assert!((Operation::StoreNoEmbedding.target_p95_ms() - 20.0).abs() < 1e-9);
        assert!((Operation::SearchFts.target_p95_ms() - 100.0).abs() < 1e-9);
        assert!((Operation::RecallHot.target_p95_ms() - 50.0).abs() < 1e-9);
    }
}
