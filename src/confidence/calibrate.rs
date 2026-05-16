// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 5 — calibration sweep.
//!
//! Reads `confidence_shadow_observations` since N days back and emits
//! per-(namespace, source) baselines: the median derived confidence the
//! [`crate::confidence::derive`] engine produced over the observed
//! window. Driven by the `ai-memory calibrate confidence --from-shadow`
//! CLI subcommand and the `memory_calibrate_confidence` MCP tool.
//!
//! Audit-honest contract: the sweep is **read-only** by default. The
//! computed baselines are surfaced as a report; persistence into a
//! calibration store is an opt-in follow-up that operators run only
//! after reviewing the output (so a poorly-sampled window can't
//! silently re-pin a namespace's confidence ceiling).
//!
//! # Streaming aggregation (Cluster G, PERF-12)
//!
//! Pre-Cluster-G, this module materialised the entire window into a
//! `Vec<(ShadowObservation, String)>` (via INNER JOIN against
//! `memories` to pull the source role), then grouped + sorted in Rust.
//! A long-running shadow-mode deployment with millions of observations
//! exhausted memory on the calibration call.
//!
//! Post-Cluster-G, the sweep streams in two passes:
//!
//! 1. **Group counts + mean** (single SQL aggregation):
//!    ```sql
//!    SELECT namespace, source, COUNT(*), AVG(derived_confidence)
//!    FROM confidence_shadow_observations
//!    WHERE observed_at >= ?1
//!    GROUP BY namespace, source
//!    ```
//!
//! 2. **Per-group median + bucket histogram** (cursor-based scan):
//!    ```sql
//!    SELECT derived_confidence FROM confidence_shadow_observations
//!    WHERE observed_at >= ?1 AND namespace = ?2 AND source = ?3
//!    ORDER BY derived_confidence ASC
//!    ```
//!    The compound `(namespace, source, observed_at)` index added in
//!    schema v40 keeps the WHERE-predicate scan tight; the ORDER BY
//!    DESCfile by sort merge stays in scratch space (no full-table
//!    Vec materialisation). Median is picked at row index
//!    `count / 2` (lower median for even counts, identical to the
//!    pre-Cluster-G `(a+b)/2` semantics within the test tolerance);
//!    buckets fold into 10 stack-allocated counters via a single pass.
//!
//! The denormalised `source` column (also schema v40) eliminates the
//! join with `memories` entirely — orphan observation rows whose
//! source memory has been CASCADE-deleted continue to surface in the
//! report under their stamped `source` value, which is the audit-
//! honest behaviour (the calibration sample was real; the source
//! memory's later deletion doesn't unmake the observation).

use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;

/// Default sweep window. The Form 5 brief calls for 30 days; tunable
/// per call via the CLI `--days N` flag and the MCP `days` parameter.
pub const DEFAULT_WINDOW_DAYS: i64 = 30;

/// One per-(namespace, source) row in the calibration report.
///
/// `source` is the `memories.source` role label (`user`, `claude`,
/// `api`, …) denormalised onto each shadow observation via the
/// v40-schema column. `count` is the number of observations that
/// contributed; `median` and the bucket distribution let an operator
/// spot a skewed sample.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PerSourceBaseline {
    pub namespace: String,
    pub source: String,
    pub count: u64,
    /// Median derived confidence across the window. Robust to outliers
    /// vs. the mean.
    pub median: f64,
    /// Mean derived confidence — emitted alongside the median so a
    /// caller can spot a skew-vs-tail distinction at a glance.
    pub mean: f64,
    /// Bucketed distribution of derived values. 10 buckets covering
    /// `[0.0, 0.1)` … `[0.9, 1.0]` so a downstream UI can plot a
    /// histogram without re-reading the observation table.
    pub buckets: [u64; 10],
}

/// Top-level calibration report emitted by the sweep.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CalibrationReport {
    pub window_days: i64,
    pub total_observations: u64,
    pub baselines: Vec<PerSourceBaseline>,
}

/// Compute the calibration report by scanning shadow observations from
/// the last `days` days.
///
/// `now` is parameterised so tests can pin a deterministic clock. The
/// production CLI/MCP wrappers pass `Utc::now()`.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the SELECT fails.
#[allow(clippy::cast_precision_loss)]
pub fn calibrate_from_shadow(
    conn: &Connection,
    days: i64,
    now: chrono::DateTime<Utc>,
) -> Result<CalibrationReport> {
    let since_dt = now - Duration::days(days);
    let since = since_dt.to_rfc3339();

    // Pass 1: per-group count + mean, computed entirely in SQL. The
    // denormalised `source` column (schema v40) lets us avoid the
    // INNER JOIN against `memories` that pre-Cluster-G code carried.
    let mut stmt = conn.prepare(
        "SELECT namespace, source, COUNT(*), AVG(derived_confidence)
         FROM confidence_shadow_observations
         WHERE observed_at >= ?1
         GROUP BY namespace, source
         ORDER BY namespace, source",
    )?;
    let groups: Vec<(String, String, i64, f64)> = stmt
        .query_map(params![since.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let total_observations: u64 = groups.iter().map(|(_, _, c, _)| *c as u64).sum();

    // Pass 2: per-group cursor scan for median + bucket histogram.
    // The compound (namespace, source, observed_at) index from
    // schema v40 makes the WHERE filter cheap; the per-group result
    // set is bounded by the group size (typically thousands, not
    // millions) so the streaming Vec<f64> stays small.
    let mut median_stmt = conn.prepare(
        "SELECT derived_confidence
         FROM confidence_shadow_observations
         WHERE observed_at >= ?1 AND namespace = ?2 AND source = ?3
         ORDER BY derived_confidence ASC",
    )?;

    let mut baselines: Vec<PerSourceBaseline> = Vec::with_capacity(groups.len());
    for (namespace, source, count_i64, mean) in groups {
        if count_i64 <= 0 {
            continue;
        }
        let count = count_i64 as u64;
        let mut values: Vec<f64> = Vec::with_capacity(count as usize);
        let mut rows =
            median_stmt.query(params![since.as_str(), namespace.as_str(), source.as_str()])?;
        let mut buckets = [0_u64; 10];
        while let Some(row) = rows.next()? {
            let v: f64 = row.get(0)?;
            let idx = ((v.clamp(0.0, 1.0) * 10.0) as usize).min(9);
            buckets[idx] += 1;
            values.push(v);
        }
        // Values arrived ORDER BY ASC — pick the median by index.
        let median = if values.is_empty() {
            0.0
        } else if values.len() % 2 == 0 {
            let mid = values.len() / 2;
            (values[mid - 1] + values[mid]) / 2.0
        } else {
            values[values.len() / 2]
        };
        baselines.push(PerSourceBaseline {
            namespace,
            source,
            count,
            median,
            mean,
            buckets,
        });
    }
    drop(median_stmt);

    Ok(CalibrationReport {
        window_days: days,
        total_observations,
        baselines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::shadow::observe;
    use crate::models::ConfidenceSignals;
    use crate::storage::open as open_storage;

    fn open_tmp() -> (Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("test.db");
        let _ = open_storage(&path).expect("open storage");
        let conn = Connection::open(&path).expect("open conn");
        (conn, dir)
    }

    fn seed_mem(conn: &Connection, id: &str, ns: &str, source: &str) {
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, source, created_at, updated_at)
             VALUES (?1, 'mid', ?2, ?1, 'c', ?3, '2026-05-15T00:00:00Z', '2026-05-15T00:00:00Z')",
            params![id, ns, source],
        )
        .expect("seed mem");
    }

    fn signals() -> ConfidenceSignals {
        ConfidenceSignals::default()
    }

    #[test]
    fn calibrate_emits_per_source_baselines() {
        let (conn, _dir) = open_tmp();
        seed_mem(&conn, "m1", "ns", "user");
        seed_mem(&conn, "m2", "ns", "user");
        seed_mem(&conn, "m3", "ns", "claude");
        observe(&conn, "m1", "ns", "user", 0.9, 0.5, &signals(), None).unwrap();
        observe(&conn, "m2", "ns", "user", 0.9, 0.7, &signals(), None).unwrap();
        observe(&conn, "m3", "ns", "claude", 0.9, 0.3, &signals(), None).unwrap();

        let report = calibrate_from_shadow(&conn, 30, Utc::now()).expect("calibrate");
        assert_eq!(report.total_observations, 3);
        assert_eq!(report.baselines.len(), 2);
        let user = report
            .baselines
            .iter()
            .find(|b| b.source == "user")
            .expect("user baseline");
        assert_eq!(user.count, 2);
        assert!(
            (user.median - 0.6).abs() < 1e-6,
            "median got {}",
            user.median
        );
        let claude = report
            .baselines
            .iter()
            .find(|b| b.source == "claude")
            .expect("claude baseline");
        assert!((claude.median - 0.3).abs() < 1e-6);
    }

    #[test]
    fn calibrate_buckets_cover_full_range() {
        let (conn, _dir) = open_tmp();
        seed_mem(&conn, "m1", "ns", "user");
        for v in &[0.05, 0.25, 0.45, 0.55, 0.95] {
            observe(&conn, "m1", "ns", "user", 0.9, *v, &signals(), None).unwrap();
        }
        let report = calibrate_from_shadow(&conn, 30, Utc::now()).expect("calibrate");
        let b = &report.baselines[0];
        // One value in each of buckets 0, 2, 4, 5, 9
        assert_eq!(b.buckets[0], 1);
        assert_eq!(b.buckets[2], 1);
        assert_eq!(b.buckets[4], 1);
        assert_eq!(b.buckets[5], 1);
        assert_eq!(b.buckets[9], 1);
        assert_eq!(b.count, 5);
    }

    #[test]
    fn calibrate_filters_by_window() {
        let (conn, _dir) = open_tmp();
        seed_mem(&conn, "m1", "ns", "user");
        // Insert one row with a very old observed_at by direct INSERT.
        conn.execute(
            "INSERT INTO confidence_shadow_observations
                (memory_id, namespace, source, caller_confidence, derived_confidence,
                 signals, recall_outcome, observed_at)
             VALUES ('m1', 'ns', 'user', 0.9, 0.5, '{}', NULL, '2020-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        observe(&conn, "m1", "ns", "user", 0.9, 0.7, &signals(), None).unwrap();
        let report = calibrate_from_shadow(&conn, 1, Utc::now()).expect("calibrate");
        // Old row outside the 1-day window drops out.
        assert_eq!(report.total_observations, 1);
        let b = &report.baselines[0];
        assert!((b.median - 0.7).abs() < 1e-6);
    }

    #[test]
    fn calibrate_empty_table_returns_empty_report() {
        let (conn, _dir) = open_tmp();
        let report = calibrate_from_shadow(&conn, 30, Utc::now()).expect("calibrate");
        assert_eq!(report.total_observations, 0);
        assert!(report.baselines.is_empty());
    }
}
