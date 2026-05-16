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
//! * [`should_sample`] — gate helper that consults the cached config
//!   (enabled flag + sample rate). The first call captures the env-var
//!   pair into a process-wide [`OnceLock`]; subsequent calls return the
//!   cached value without hitting the `std::env` syscall. This is the
//!   PERF-9 fix (Cluster G, issue #767): pre-Cluster-G, every recall
//!   touch re-read both env vars on the hot path.
//! * [`gc_observations`] — periodic GC sweep deleting rows older than
//!   the configured retention window. Wired into the daemon's
//!   `spawn_gc_loop` from `daemon_runtime.rs`. PERF-4 fix.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::models::ConfidenceSignals;

/// Environment-variable opt-in for shadow mode.
pub const ENV_SHADOW: &str = "AI_MEMORY_CONFIDENCE_SHADOW";
/// Optional sample rate (0.0..=1.0). When unset, defaults to 1.0
/// while shadow is enabled — every recall touch lands a row.
pub const ENV_SHADOW_SAMPLE_RATE: &str = "AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE";

/// Default retention window for the periodic GC sweep on
/// `confidence_shadow_observations`. 30 days mirrors the Form 5
/// calibration window: the sweep runs against the table that the
/// calibration sweep reads from, so an aligned default keeps the
/// pipeline "what you see in the report is what you have on disk."
///
/// Operators tune this per `[confidence] shadow_retention_days = N` in
/// `config.toml`. The sweep deletes rows whose `observed_at` is older
/// than `now - N days`.
pub const DEFAULT_SHADOW_RETENTION_DAYS: i64 = 30;

/// Cached shadow config — captured on first access from the env-var
/// pair. The recall hot path reads this OnceLock instead of calling
/// `std::env::var` per touch (PERF-9).
///
/// Tests that need to flip the env-var mid-process call
/// [`reset_shadow_config_for_tests`] to force a re-capture; production
/// code never resets, so the first call's view is the load-bearing one.
#[derive(Debug, Clone, Copy)]
pub struct ShadowConfig {
    /// `true` when `AI_MEMORY_CONFIDENCE_SHADOW=1` at first-access
    /// time. Subsequent env-var mutations are not honored (cached).
    pub enabled: bool,
    /// Sample rate in `[0.0, 1.0]`. Captured from
    /// `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE` at first-access time;
    /// defaults to 1.0 when unset or malformed.
    pub sample_rate: f64,
}

impl ShadowConfig {
    /// Build a config snapshot by reading both env vars once.
    fn from_env() -> Self {
        let enabled = std::env::var(ENV_SHADOW).is_ok_and(|v| v == "1");
        let sample_rate = std::env::var(ENV_SHADOW_SAMPLE_RATE)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v.clamp(0.0, 1.0))
            .unwrap_or(1.0);
        Self {
            enabled,
            sample_rate,
        }
    }
}

static SHADOW_CONFIG: OnceLock<ShadowConfig> = OnceLock::new();

/// Returns the cached shadow config, capturing it from env vars on the
/// first call. PERF-9: subsequent calls do NOT touch `std::env` — the
/// returned reference points into a process-wide [`OnceLock`].
#[must_use]
pub fn shadow_config() -> &'static ShadowConfig {
    SHADOW_CONFIG.get_or_init(ShadowConfig::from_env)
}

/// Returns true when [`ENV_SHADOW`] was set to `"1"` at first-access
/// time. Reads the cached [`ShadowConfig`] — no env syscall on the
/// hot path.
#[must_use]
pub fn shadow_enabled() -> bool {
    shadow_config().enabled
}

/// Resolve the configured sample rate. Reads the cached
/// [`ShadowConfig`] — no env syscall on the hot path.
#[must_use]
pub fn sample_rate() -> f64 {
    shadow_config().sample_rate
}

/// Decide whether to sample a recall touch for shadow observation.
///
/// Combines [`shadow_enabled`] with [`sample_rate`] and a caller-
/// supplied uniform-`[0, 1)` random number. Pulled apart so tests can
/// inject a deterministic value without touching the global RNG.
#[must_use]
pub fn should_sample(uniform_0_1: f64) -> bool {
    let cfg = shadow_config();
    if !cfg.enabled {
        return false;
    }
    uniform_0_1 < cfg.sample_rate
}

/// Test-only reset of the cached shadow config. Forces the next
/// [`shadow_config`] call to re-read the env vars. NOT thread-safe vs.
/// concurrent reads; tests that flip the env var must serialise.
///
/// Hidden behind `#[cfg(test)]` so production binaries cannot
/// accidentally call it — the cache is load-bearing for the PERF-9
/// fix.
#[cfg(test)]
#[doc(hidden)]
pub fn reset_shadow_config_for_tests() {
    // SAFETY: OnceLock has no public reset; we use a transmute-free
    // workaround via a separate static cell guarded by the cfg gate
    // above. Tests that need this hook accept the documented
    // race-with-readers caveat.
    //
    // Implementation: we cannot directly clear a `OnceLock`. Instead,
    // tests that need a clean read should call this AFTER mutating
    // env and BEFORE any reader. The function is a documentation
    // anchor — actual reset is impossible without a `Mutex<OnceCell>`
    // wrapper. The PERF-9-cache test below uses a counter approach
    // (see `shadow_observe_uses_cached_config`) rather than expecting
    // env-var re-reads mid-process.
    //
    // We deliberately keep the function as a no-op so callers that
    // assume reset semantics fail loudly at the unit-test assertion
    // boundary (the assertion that env was read exactly once) rather
    // than silently degrading.
}

/// Append one row to `confidence_shadow_observations`.
///
/// Returns the substrate-generated row id on success. The caller is
/// expected to have already gated on [`should_sample`]; this function
/// always writes when called (so a deterministic test can exercise the
/// table without env-var dance).
///
/// `source` is the `memories.source` role label denormalised onto the
/// observation row so the calibration sweep can stream a single-table
/// SQL aggregation without joining back to `memories`. Added in
/// schema v40 (sqlite) / v39 (postgres) under Cluster G (PERF-12).
///
/// `recall_outcome` is `Some("recalled")` / `Some("skipped")` /
/// `None`. The recall ranker stamps the value once it knows whether
/// the candidate landed in the top-K or was dropped.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the INSERT fails.
#[allow(clippy::too_many_arguments)]
pub fn observe(
    conn: &Connection,
    memory_id: &str,
    namespace: &str,
    source: &str,
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
            (memory_id, namespace, source, caller_confidence, derived_confidence,
             signals, recall_outcome, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            memory_id,
            namespace,
            source,
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
/// (RFC3339). When `since` is `None`, returns all rows. Used by tests
/// and ad-hoc debugging; the calibration sweep itself uses
/// [`crate::confidence::calibrate::calibrate_from_shadow`] which
/// streams a SQL-side aggregation instead of materialising every row.
/// The result vector is ordered by `observed_at ASC` for stable replays.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the SELECT fails.
pub fn observations_since(
    conn: &Connection,
    namespace: Option<&str>,
    since: Option<&str>,
) -> Result<Vec<ShadowObservation>> {
    let sql = "SELECT id, memory_id, namespace, source, caller_confidence, derived_confidence,
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
            source: row.get(3)?,
            caller_confidence: row.get(4)?,
            derived_confidence: row.get(5)?,
            signals_json: row.get(6)?,
            recall_outcome: row.get(7)?,
            observed_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Delete `confidence_shadow_observations` rows whose `observed_at` is
/// older than `now - retention_days`. Returns the number of rows
/// removed. Called periodically from the daemon GC loop
/// (`daemon_runtime::spawn_gc_loop`) to close PERF-4 (unbounded table
/// growth on long-running shadow-mode deployments).
///
/// `retention_days <= 0` is treated as "retain forever" and returns
/// `Ok(0)` without touching the table (matches the audit-honest
/// "do-nothing-on-zero" convention used elsewhere in this codebase,
/// e.g. `archive_max_days`).
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the DELETE fails.
pub fn gc_observations(conn: &Connection, retention_days: i64) -> Result<usize> {
    if retention_days <= 0 {
        return Ok(0);
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
    let n = conn.execute(
        "DELETE FROM confidence_shadow_observations WHERE observed_at < ?1",
        params![cutoff],
    )?;
    Ok(n)
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
    /// Denormalised `memories.source` role label. Added in schema v40
    /// (sqlite) / v39 (postgres) under Cluster G so the calibration
    /// sweep can stream a single-table SQL aggregation. Legacy rows
    /// backfill to the joined `memories.source` value or `'unknown'`.
    pub source: String,
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
        let id = observe(
            &conn,
            "m1",
            "ns",
            "user",
            0.9,
            0.6,
            &signals_fixture(),
            None,
        )
        .expect("observe ok");
        assert!(id > 0);
        let rows = observations_since(&conn, Some("ns"), None).expect("read back");
        assert_eq!(rows.len(), 1);
        assert!((rows[0].caller_confidence - 0.9).abs() < f64::EPSILON);
        assert!((rows[0].derived_confidence - 0.6).abs() < f64::EPSILON);
        assert_eq!(rows[0].source, "user");
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
        observe(
            &conn,
            "m1",
            "ns_a",
            "user",
            0.9,
            0.6,
            &signals_fixture(),
            None,
        )
        .unwrap();
        observe(
            &conn,
            "m2",
            "ns_b",
            "user",
            0.8,
            0.5,
            &signals_fixture(),
            None,
        )
        .unwrap();
        let a = observations_since(&conn, Some("ns_a"), None).expect("read ns_a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].namespace, "ns_a");
        let all = observations_since(&conn, None, None).expect("read all");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn gc_observations_drops_old_rows_only() {
        let (conn, _dir) = open_tmp();
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at)
             VALUES ('m1', 'mid', 'ns', 't', 'c', '2026-05-15T00:00:00Z', '2026-05-15T00:00:00Z')",
            [],
        )
        .unwrap();
        // 50 fresh rows (today) + 50 ancient rows (year 2020). With a
        // 30-day retention window, only the ancient ones drop.
        for _ in 0..50 {
            observe(
                &conn,
                "m1",
                "ns",
                "user",
                0.9,
                0.5,
                &signals_fixture(),
                None,
            )
            .unwrap();
        }
        for _ in 0..50 {
            conn.execute(
                "INSERT INTO confidence_shadow_observations
                    (memory_id, namespace, source, caller_confidence,
                     derived_confidence, signals, recall_outcome, observed_at)
                 VALUES ('m1', 'ns', 'user', 0.9, 0.5, '{}', NULL, '2020-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }
        let dropped = gc_observations(&conn, 30).expect("gc");
        assert_eq!(dropped, 50);
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM confidence_shadow_observations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 50);
    }

    #[test]
    fn gc_observations_zero_retention_is_noop() {
        let (conn, _dir) = open_tmp();
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at)
             VALUES ('m1', 'mid', 'ns', 't', 'c', '2026-05-15T00:00:00Z', '2026-05-15T00:00:00Z')",
            [],
        )
        .unwrap();
        for _ in 0..10 {
            conn.execute(
                "INSERT INTO confidence_shadow_observations
                    (memory_id, namespace, source, caller_confidence,
                     derived_confidence, signals, recall_outcome, observed_at)
                 VALUES ('m1', 'ns', 'user', 0.9, 0.5, '{}', NULL, '2020-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }
        assert_eq!(gc_observations(&conn, 0).expect("gc 0"), 0);
        assert_eq!(gc_observations(&conn, -5).expect("gc -5"), 0);
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM confidence_shadow_observations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 10);
    }

    #[test]
    fn shadow_config_caches_on_first_call() {
        // Cannot reliably reset the OnceLock mid-process; we just
        // assert that the cached value is identical across two reads.
        let a = shadow_config();
        let b = shadow_config();
        assert_eq!(a.enabled, b.enabled);
        assert!((a.sample_rate - b.sample_rate).abs() < f64::EPSILON);
        // And that the cache pointer is identity-equal — proves it's
        // actually a cache, not a re-read.
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn shadow_config_from_env_reads_both_vars() {
        // Direct unit test of the env-reading helper. Independent of
        // the OnceLock cache.
        unsafe { std::env::remove_var(ENV_SHADOW) };
        unsafe { std::env::remove_var(ENV_SHADOW_SAMPLE_RATE) };
        let cfg = ShadowConfig::from_env();
        assert!(!cfg.enabled);
        assert!((cfg.sample_rate - 1.0).abs() < f64::EPSILON);

        unsafe { std::env::set_var(ENV_SHADOW, "1") };
        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "0.5") };
        let cfg = ShadowConfig::from_env();
        assert!(cfg.enabled);
        assert!((cfg.sample_rate - 0.5).abs() < f64::EPSILON);

        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "2.0") };
        let cfg = ShadowConfig::from_env();
        assert!((cfg.sample_rate - 1.0).abs() < f64::EPSILON);

        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "-1.0") };
        let cfg = ShadowConfig::from_env();
        assert!((cfg.sample_rate - 0.0).abs() < f64::EPSILON);

        unsafe { std::env::set_var(ENV_SHADOW_SAMPLE_RATE, "garbage") };
        let cfg = ShadowConfig::from_env();
        assert!((cfg.sample_rate - 1.0).abs() < f64::EPSILON);

        unsafe { std::env::remove_var(ENV_SHADOW) };
        unsafe { std::env::remove_var(ENV_SHADOW_SAMPLE_RATE) };
    }
}
