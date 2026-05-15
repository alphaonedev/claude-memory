// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 5 (issue #758) — MCP handler for
//! `memory_calibrate_confidence`.
//!
//! Operator-callable equivalent of the `ai-memory calibrate confidence
//! --from-shadow` CLI driver. Reads
//! `confidence_shadow_observations` for the last `days` days (default
//! 30) and emits a [`crate::confidence::calibrate::CalibrationReport`]
//! envelope with per-(namespace, source) baselines.
//!
//! Family::Power surface — operator/observability, not data-plane.

use serde_json::{Value, json};

use crate::confidence::calibrate::{DEFAULT_WINDOW_DAYS, calibrate_from_shadow};

/// Wire shape:
///
/// ```json
/// {
///   "report": {
///     "window_days": 30,
///     "total_observations": 42,
///     "baselines": [
///       { "namespace": "ns", "source": "user", "count": 12,
///         "median": 0.62, "mean": 0.61, "buckets": [0,0,1,2,3,3,2,1,0,0] }
///     ]
///   }
/// }
/// ```
///
/// Errors:
/// * `days must be a positive integer` — caller passed `days <= 0`.
/// * `memory_calibrate_confidence substrate error: ...` — SQL error.
pub(super) fn handle_calibrate_confidence(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let days = params
        .get("days")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_WINDOW_DAYS);
    if days <= 0 {
        return Err("days must be a positive integer".to_string());
    }

    let report = calibrate_from_shadow(conn, days, chrono::Utc::now())
        .map_err(|e| format!("memory_calibrate_confidence substrate error: {e}"))?;

    Ok(json!({ "report": report }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open as open_storage;
    use rusqlite::Connection;
    use serde_json::json;

    fn open_tmp() -> (Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("test.db");
        let _ = open_storage(&path).expect("open storage");
        let conn = Connection::open(&path).expect("open conn");
        (conn, dir)
    }

    #[test]
    fn empty_db_returns_empty_baselines() {
        let (conn, _dir) = open_tmp();
        let v = handle_calibrate_confidence(&conn, &json!({})).expect("ok");
        assert_eq!(v["report"]["total_observations"], 0);
        assert!(v["report"]["baselines"].as_array().unwrap().is_empty());
    }

    #[test]
    fn rejects_non_positive_days() {
        let (conn, _dir) = open_tmp();
        let err = handle_calibrate_confidence(&conn, &json!({"days": 0})).expect_err("must reject");
        assert!(err.contains("positive integer"));
        let err =
            handle_calibrate_confidence(&conn, &json!({"days": -1})).expect_err("must reject");
        assert!(err.contains("positive integer"));
    }

    #[test]
    fn default_days_used_when_omitted() {
        let (conn, _dir) = open_tmp();
        let v = handle_calibrate_confidence(&conn, &json!({})).expect("ok");
        assert_eq!(
            v["report"]["window_days"].as_i64().unwrap(),
            DEFAULT_WINDOW_DAYS
        );
    }
}
