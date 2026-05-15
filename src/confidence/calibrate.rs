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

use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;

use super::shadow::ShadowObservation;

/// Default sweep window. The Form 5 brief calls for 30 days; tunable
/// per call via the CLI `--days N` flag and the MCP `days` parameter.
pub const DEFAULT_WINDOW_DAYS: i64 = 30;

/// One per-(namespace, source) row in the calibration report.
///
/// `source` is the `memories.source` role label (`user`, `claude`,
/// `api`, …) joined back to each shadow observation via `memory_id`.
/// `count` is the number of observations that contributed; `median`
/// and the bucket distribution let an operator spot a skewed sample.
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
pub fn calibrate_from_shadow(
    conn: &Connection,
    days: i64,
    now: chrono::DateTime<Utc>,
) -> Result<CalibrationReport> {
    let since_dt = now - Duration::days(days);
    let since = since_dt.to_rfc3339();

    // Join shadow observations against memories to pull the `source`
    // role label. Rows whose source memory has been deleted (cascade
    // FK fires) drop out of the report. Sample stays representative.
    let mut stmt = conn.prepare(
        "SELECT o.id, o.memory_id, o.namespace, o.caller_confidence, o.derived_confidence,
                o.signals, o.recall_outcome, o.observed_at, m.source
         FROM confidence_shadow_observations o
         INNER JOIN memories m ON m.id = o.memory_id
         WHERE o.observed_at >= ?1
         ORDER BY o.namespace, m.source, o.observed_at ASC",
    )?;
    type Row = (ShadowObservation, String);
    let rows = stmt.query_map(params![since], |row| {
        Ok((
            ShadowObservation {
                id: row.get(0)?,
                memory_id: row.get(1)?,
                namespace: row.get(2)?,
                caller_confidence: row.get(3)?,
                derived_confidence: row.get(4)?,
                signals_json: row.get(5)?,
                recall_outcome: row.get(6)?,
                observed_at: row.get(7)?,
            },
            row.get::<_, String>(8)?,
        ))
    })?;
    let mut all: Vec<Row> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let total_observations = all.len() as u64;

    // Group by (namespace, source). `all` is already sorted by the
    // SQL ORDER BY clause so we can stream-collect.
    let mut baselines: Vec<PerSourceBaseline> = Vec::new();
    let mut group: Vec<f64> = Vec::new();
    let mut current_ns: Option<String> = None;
    let mut current_src: Option<String> = None;

    fn finalise(ns: &str, src: &str, values: &mut Vec<f64>, out: &mut Vec<PerSourceBaseline>) {
        if values.is_empty() {
            return;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if values.len() % 2 == 0 {
            (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
        } else {
            values[values.len() / 2]
        };
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let mut buckets = [0_u64; 10];
        for &v in values.iter() {
            let idx = ((v.clamp(0.0, 1.0) * 10.0) as usize).min(9);
            buckets[idx] += 1;
        }
        out.push(PerSourceBaseline {
            namespace: ns.to_string(),
            source: src.to_string(),
            count: values.len() as u64,
            median,
            mean,
            buckets,
        });
        values.clear();
    }

    all.drain(..).for_each(|(obs, src)| {
        let same_ns = current_ns.as_deref() == Some(obs.namespace.as_str());
        let same_src = current_src.as_deref() == Some(src.as_str());
        if !(same_ns && same_src) {
            if let (Some(ns), Some(s)) = (&current_ns, &current_src) {
                finalise(ns, s, &mut group, &mut baselines);
            }
            current_ns = Some(obs.namespace.clone());
            current_src = Some(src.clone());
        }
        group.push(obs.derived_confidence);
    });
    if let (Some(ns), Some(s)) = (&current_ns, &current_src) {
        finalise(ns, s, &mut group, &mut baselines);
    }

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
        observe(&conn, "m1", "ns", 0.9, 0.5, &signals(), None).unwrap();
        observe(&conn, "m2", "ns", 0.9, 0.7, &signals(), None).unwrap();
        observe(&conn, "m3", "ns", 0.9, 0.3, &signals(), None).unwrap();

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
            observe(&conn, "m1", "ns", 0.9, *v, &signals(), None).unwrap();
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
                (memory_id, namespace, caller_confidence, derived_confidence,
                 signals, recall_outcome, observed_at)
             VALUES ('m1', 'ns', 0.9, 0.5, '{}', NULL, '2020-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        observe(&conn, "m1", "ns", 0.9, 0.7, &signals(), None).unwrap();
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
