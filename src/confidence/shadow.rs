// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 5 — shadow-mode telemetry pipeline.
//!
//! Per-recall observations land in `confidence_shadow_observations`
//! when `AI_MEMORY_CONFIDENCE_SHADOW=1`, sampled at the rate carried
//! by `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE` (0.0..=1.0; default
//! 1.0 when shadow is enabled).
//!
//! Audit-honest contract: shadow mode **never silently overrides** the
//! caller's confidence. The recall ranker still uses the caller value
//! downstream; the derived value is only persisted for later
//! calibration. This is the load-bearing property that lets operators
//! safely turn the engine on in production.
//!
//! # Surface
//!
//! * [`observe`] — write a single shadow row.
//! * [`should_sample`] — gate helper that consults the env-var pair
//!   (enabled flag + sample rate). Pulled out so tests can pass a
//!   deterministic RNG and avoid a hot-path thread-local lookup.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::models::ConfidenceSignals;

/// Environment-variable opt-in for shadow mode.
pub const ENV_SHADOW: &str = "AI_MEMORY_CONFIDENCE_SHADOW";
/// Optional sample rate (0.0..=1.0). When unset, defaults to 1.0
/// while shadow is enabled — every recall touch lands a row.
pub const ENV_SHADOW_SAMPLE_RATE: &str = "AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE";

/// Returns true when [`ENV_SHADOW`] is set to `"1"`.
#[must_use]
pub fn shadow_enabled() -> bool {
    std::env::var(ENV_SHADOW).is_ok_and(|v| v == "1")
}

/// Resolve the configured sample rate. Parses [`ENV_SHADOW_SAMPLE_RATE`]
/// as a float in `[0.0, 1.0]`; defaults to 1.0 when unset or malformed.
#[must_use]
pub fn sample_rate() -> f64 {
    std::env::var(ENV_SHADOW_SAMPLE_RATE)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(1.0)
}

/// Decide whether to sample a recall touch for shadow observation.
///
/// Combines [`shadow_enabled`] with [`sample_rate`] and a caller-
/// supplied uniform-`[0, 1)` random number. Pulled apart so tests can
/// inject a deterministic value without touching the global RNG.
#[must_use]
pub fn should_sample(uniform_0_1: f64) -> bool {
    if !shadow_enabled() {
        return false;
    }
    uniform_0_1 < sample_rate()
}

/// Append one row to `confidence_shadow_observations`.
///
/// Returns the substrate-generated row id on success. The caller is
/// expected to have already gated on [`should_sample`]; this function
/// always writes when called (so a deterministic test can exercise the
/// table without env-var dance).
///
/// `recall_outcome` is `Some("recalled")` / `Some("skipped")` /
/// `None`. The recall ranker stamps the value once it knows whether
/// the candidate landed in the top-K or was dropped.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the INSERT fails.
pub fn observe(
    conn: &Connection,
    memory_id: &str,
    namespace: &str,
    caller_confidence: f64,
    derived_confidence: f64,
    signals: &ConfidenceSignals,
    recall_outcome: Option<&str>,
) -> Result<i64> {
    let signals_json =
        serde_json::to_string(signals).context("serialise ConfidenceSignals envelope")?;
    let observed_at = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO confidence_shadow_observations
            (memory_id, namespace, caller_confidence, derived_confidence,
             signals, recall_outcome, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            memory_id,
            namespace,
            caller_confidence,
            derived_confidence,
            signals_json,
            recall_outcome,
            observed_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Pull every shadow observation in `namespace` newer than `since`
/// (RFC3339). When `since` is `None`, returns all rows. Used by the
/// calibration sweep and by tests. The result vector is ordered by
/// `observed_at ASC` for stable replays.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the SELECT fails.
pub fn observations_since(
    conn: &Connection,
    namespace: Option<&str>,
    since: Option<&str>,
) -> Result<Vec<ShadowObservation>> {
    let sql = "SELECT id, memory_id, namespace, caller_confidence, derived_confidence,
                      signals, recall_outcome, observed_at
               FROM confidence_shadow_observations
               WHERE (?1 IS NULL OR namespace = ?1)
                 AND (?2 IS NULL OR observed_at >= ?2)
               ORDER BY observed_at ASC, id ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![namespace, since], |row| {
        Ok(ShadowObservation {
            id: row.get(0)?,
            memory_id: row.get(1)?,
            namespace: row.get(2)?,
            caller_confidence: row.get(3)?,
            derived_confidence: row.get(4)?,
            signals_json: row.get(5)?,
            recall_outcome: row.get(6)?,
            observed_at: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One row of `confidence_shadow_observations` as exposed to readers.
///
/// `signals_json` stays as a raw string because the calibration sweep
/// usually doesn't need to deserialise it (the (namespace, source) key
/// is enough). Callers that want the typed envelope can parse it
/// themselves via `serde_json::from_str::<ConfidenceSignals>`.
#[derive(Debug, Clone)]
pub struct ShadowObservation {
    pub id: i64,
    pub memory_id: String,
    pub namespace: String,
    pub caller_confidence: f64,
    pub derived_confidence: f64,
    pub signals_json: String,
    pub recall_outcome: Option<String>,
    pub observed_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ConfidenceSignals;
    use crate::storage::open as open_storage;

    fn open_tmp() -> (Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("test.db");
        let _ = open_storage(&path).expect("open storage");
        let conn = Connection::open(&path).expect("open conn");
        (conn, dir)
    }

    fn signals_fixture() -> ConfidenceSignals {
        ConfidenceSignals {
            source_age_days: 7.0,
            atom_derivation: false,
            prior_corroboration_count: 2,
            freshness_factor: 0.84,
            baseline_per_source: 0.5,
        }
    }

    #[test]
    fn observe_appends_row() {
        let (conn, _dir) = open_tmp();
        // Seed a memory row so the FK constraint resolves.
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at)
             VALUES ('m1', 'mid', 'ns', 't', 'c', '2026-05-15T00:00:00Z', '2026-05-15T00:00:00Z')",
            [],
        )
        .expect("seed mem");
        let id =
            observe(&conn, "m1", "ns", 0.9, 0.6, &signals_fixture(), None).expect("observe ok");
        assert!(id > 0);
        let rows = observations_since(&conn, Some("ns"), None).expect("read back");
        assert_eq!(rows.len(), 1);
        assert!((rows[0].caller_confidence - 0.9).abs() < f64::EPSILON);
        assert!((rows[0].derived_confidence - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn observations_filter_by_namespace() {
        let (conn, _dir) = open_tmp();
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at)
             VALUES ('m1', 'mid', 'ns_a', 't1', 'c', '2026-05-15T00:00:00Z', '2026-05-15T00:00:00Z')",
            [],
        )
        .expect("seed mem a");
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at)
             VALUES ('m2', 'mid', 'ns_b', 't2', 'c', '2026-05-15T00:00:00Z', '2026-05-15T00:00:00Z')",
            [],
        )
        .expect("seed mem b");
        observe(&conn, "m1", "ns_a", 0.9, 0.6, &signals_fixture(), None).unwrap();
        observe(&conn, "m2", "ns_b", 0.8, 0.5, &signals_fixture(), None).unwrap();
        let a = observations_since(&conn, Some("ns_a"), None).expect("read ns_a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].namespace, "ns_a");
        let all = observations_since(&conn, None, None).expect("read all");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn should_sample_off_by_default() {
        unsafe { std::env::remove_var(ENV_SHADOW) };
        assert!(!should_sample(0.0));
    }

    #[test]
    fn sample_rate_clamps_input_and_defaults_to_one() {
        unsafe { std::env::remove_var(ENV_SHADOW_SAMPLE_RATE) };
        assert!((sample_rate() - 1.0).abs() < f64::EPSILON);
        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "0.5") };
        assert!((sample_rate() - 0.5).abs() < f64::EPSILON);
        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "2.0") };
        assert!((sample_rate() - 1.0).abs() < f64::EPSILON);
        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "-1.0") };
        assert!((sample_rate() - 0.0).abs() < f64::EPSILON);
        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "garbage") };
        assert!((sample_rate() - 1.0).abs() < f64::EPSILON);
        unsafe { std::env::remove_var(ENV_SHADOW_SAMPLE_RATE) };
    }
}
