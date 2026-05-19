// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Gap 3 (#886) — `recall_observations` TTL pruner.
//!
//! The recall ledger is high-volume (one row per (recall_id,
//! memory_id) pair on every `memory_recall` call). Without pruning,
//! it grows linearly with recall traffic — fine for forensic-window
//! diagnostics, ruinous for steady-state storage. This module
//! provides a TTL-based pruner: rows whose `observed_at` is older
//! than `AI_MEMORY_OBSERVATIONS_TTL_DAYS` (default 7) are deleted.
//!
//! The pruner is safe to invoke concurrently with `record_recall`
//! and `mark_consumed`; SQLite serialises writes through the single
//! connection mutex the daemon already holds.

use anyhow::Result;
use rusqlite::{Connection, params};

/// Environment variable controlling the TTL window in days. Unset /
/// invalid → [`DEFAULT_TTL_DAYS`]. Negative or zero values keep the
/// default (a runtime "delete everything" mode is intentionally NOT
/// exposed — operators can drop the table directly if they need
/// that).
pub const TTL_ENV_VAR: &str = "AI_MEMORY_OBSERVATIONS_TTL_DAYS";

/// Default TTL window — one week. Matches the operator agreement
/// in the Gap 3 (#886) playbook: long enough for "did the agent
/// use what we surfaced last sprint?" retrospectives, short enough
/// to keep the table bounded.
pub const DEFAULT_TTL_DAYS: i64 = 7;

/// Resolve the active TTL window in days, consulting the env var
/// and falling back to [`DEFAULT_TTL_DAYS`] on any failure.
#[must_use]
pub fn ttl_days() -> i64 {
    std::env::var(TTL_ENV_VAR)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_TTL_DAYS)
}

/// Delete every `recall_observations` row whose `observed_at` is
/// older than the configured TTL. Returns the number of rows
/// pruned.
///
/// # Errors
///
/// Returns the underlying `rusqlite::Error` on SQL failure.
pub fn prune(conn: &Connection) -> Result<usize> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(ttl_days())).to_rfc3339();
    let n = conn.execute(
        "DELETE FROM recall_observations WHERE observed_at < ?1",
        params![cutoff],
    )?;
    Ok(n)
}

/// Variant of [`prune`] that uses an explicit cutoff timestamp
/// instead of consulting the environment + clock. Exists for tests
/// so the cutoff is deterministic and replayable.
///
/// # Errors
///
/// Returns the underlying `rusqlite::Error` on SQL failure.
pub fn prune_before(conn: &Connection, cutoff_rfc3339: &str) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM recall_observations WHERE observed_at < ?1",
        params![cutoff_rfc3339],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observations::{Candidate, record_recall};
    use rusqlite::Connection;

    fn fresh() -> Connection {
        // Go through `storage::open` so SCHEMA is applied before the
        // migration ladder fires (the ladder ALTERs columns on the
        // memories table created by SCHEMA).
        crate::storage::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn seed_memory(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO memories \
                (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES (?1, 'long', 'test', ?2, 'content', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
            params![id, format!("title-{id}")],
        )
        .expect("seed memory");
    }

    #[test]
    fn ttl_days_falls_back_when_env_unset() {
        // SAFETY: single-threaded test; no concurrent env access.
        unsafe {
            std::env::remove_var(TTL_ENV_VAR);
        }
        assert_eq!(ttl_days(), DEFAULT_TTL_DAYS);
    }

    #[test]
    fn prune_before_deletes_only_old_rows() {
        let conn = fresh();
        seed_memory(&conn, "m1");
        seed_memory(&conn, "m2");
        record_recall(
            &conn,
            "r1",
            &[Candidate {
                memory_id: "m1",
                retriever: "hybrid",
                rank: 1,
                score: 0.9,
            }],
        )
        .unwrap();
        // Forge an old observed_at on m1's row.
        conn.execute(
            "UPDATE recall_observations SET observed_at = ?1 WHERE memory_id = 'm1'",
            params!["2020-01-01T00:00:00Z"],
        )
        .unwrap();
        record_recall(
            &conn,
            "r1",
            &[Candidate {
                memory_id: "m2",
                retriever: "hybrid",
                rank: 2,
                score: 0.8,
            }],
        )
        .unwrap();

        let pruned = prune_before(&conn, "2024-01-01T00:00:00Z").unwrap();
        assert_eq!(pruned, 1, "only the 2020-stamped row should be pruned");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM recall_observations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }
}
