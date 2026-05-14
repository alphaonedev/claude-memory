// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Track K, Task K8 — per-agent rate limits + storage caps.
//!
//! Each registered agent gets a single row in the `agent_quotas`
//! table tracking three rolling-window counters (memories written today,
//! storage bytes consumed lifetime, links written today) against three
//! limits (`max_memories_per_day`, `max_storage_bytes`,
//! `max_links_per_day`). The `store_memory` + `memory_link` write paths
//! consult [`check_quota`] before committing; on exceeded limit the
//! call returns a [`QuotaError`] naming the limit that was hit, which
//! the MCP layer maps to a `QUOTA_EXCEEDED` diagnostic.
//!
//! Daily counters reset at UTC midnight via [`reset_daily`], driven by
//! the K8 sweep loop wired into `daemon_runtime::bootstrap_serve` —
//! same lifecycle shape as the K2 pending-actions sweeper and the I3
//! transcript-lifecycle sweeper.
//!
//! Compiled defaults: 1000 memories/day, 100 MiB storage cap, 5000
//! links/day. Defaults are deliberately generous so the K8 substrate is
//! invisible to small-scale operations; tuning down is per-deployment.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Default daily memory store ceiling per agent. Generous; tune down
/// per-deployment by overwriting the row's `max_memories_per_day` after
/// it auto-inserts on first use.
pub const DEFAULT_MAX_MEMORIES_PER_DAY: i64 = 1000;

/// Default lifetime storage cap per agent (100 MiB). Counts the
/// (title + content + metadata) byte length of every memory the agent
/// writes; not reset by the daily sweep.
pub const DEFAULT_MAX_STORAGE_BYTES: i64 = 100 * 1024 * 1024;

/// Default daily link creation ceiling per agent. Same shape as the
/// memory ceiling; reset to 0 at UTC midnight.
pub const DEFAULT_MAX_LINKS_PER_DAY: i64 = 5000;

/// Which write operation to charge against the agent's quota.
///
/// Variants:
/// - [`QuotaOp::Memory`] — one memory store. Charges 1 against
///   `current_memories_today` and `bytes` against `current_storage_bytes`.
/// - [`QuotaOp::Link`] — one link create. Charges 1 against
///   `current_links_today`. Storage is unaffected (links are a single
///   row keyed on a 3-tuple, not user-supplied bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaOp {
    /// Storing one memory of `bytes` payload size. The byte count is
    /// the sum of (title + content + metadata) lengths — same shape the
    /// `current_storage_bytes` counter accumulates.
    Memory { bytes: i64 },
    /// Creating one link. Single-row insert; no storage delta.
    Link,
}

/// Which limit was hit. The MCP error string surfaces this name so a
/// caller can switch on it without parsing the human-readable message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaLimit {
    /// `current_memories_today >= max_memories_per_day` after the
    /// pending op would post.
    MemoriesPerDay,
    /// `current_storage_bytes + op.bytes > max_storage_bytes`.
    StorageBytes,
    /// `current_links_today >= max_links_per_day` after the pending op
    /// would post.
    LinksPerDay,
}

impl QuotaLimit {
    /// Canonical lower-snake-case name for diagnostic strings + the
    /// MCP wire format.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MemoriesPerDay => "memories_per_day",
            Self::StorageBytes => "storage_bytes",
            Self::LinksPerDay => "links_per_day",
        }
    }
}

/// Failure returned by [`check_quota`] when a write would push the
/// agent's counters past one of the three limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaError {
    /// Agent whose quota was exceeded.
    pub agent_id: String,
    /// Which limit was hit.
    pub limit: QuotaLimit,
    /// The current value of the counter the limit applies to.
    pub current: i64,
    /// The configured ceiling.
    pub max: i64,
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "QUOTA_EXCEEDED: agent {} hit {} (current={}, max={})",
            self.agent_id,
            self.limit.as_str(),
            self.current,
            self.max,
        )
    }
}

impl std::error::Error for QuotaError {}

/// Snapshot of one agent's quota row, returned by [`get_status`] and
/// surfaced over the MCP `memory_quota_status` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaStatus {
    pub agent_id: String,
    pub max_memories_per_day: i64,
    pub max_storage_bytes: i64,
    pub max_links_per_day: i64,
    pub current_memories_today: i64,
    pub current_storage_bytes: i64,
    pub current_links_today: i64,
    pub day_started_at: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Auto-insert a default quota row for an agent that doesn't have one
/// yet, then return the row. Idempotent — concurrent calls converge on
/// a single row because `agent_id` is the PRIMARY KEY.
fn ensure_row(conn: &Connection, agent_id: &str) -> Result<QuotaStatus> {
    if let Some(row) = load_row(conn, agent_id)? {
        return Ok(row);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let day = day_bucket(&now);
    conn.execute(
        "INSERT OR IGNORE INTO agent_quotas
         (agent_id, max_memories_per_day, max_storage_bytes, max_links_per_day,
          current_memories_today, current_storage_bytes, current_links_today,
          day_started_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 0, 0, 0, ?5, ?6, ?6)",
        params![
            agent_id,
            DEFAULT_MAX_MEMORIES_PER_DAY,
            DEFAULT_MAX_STORAGE_BYTES,
            DEFAULT_MAX_LINKS_PER_DAY,
            day,
            now,
        ],
    )
    .context("failed to insert default quota row")?;
    load_row(conn, agent_id)?
        .context("quota row missing immediately after insert (concurrent delete?)")
}

/// Load a quota row by agent_id, returning `None` if the row does not
/// exist. Pure read — does not insert defaults.
fn load_row(conn: &Connection, agent_id: &str) -> Result<Option<QuotaStatus>> {
    conn.query_row(
        "SELECT agent_id, max_memories_per_day, max_storage_bytes, max_links_per_day,
                current_memories_today, current_storage_bytes, current_links_today,
                day_started_at, created_at, updated_at
         FROM agent_quotas WHERE agent_id = ?1",
        params![agent_id],
        |r| {
            Ok(QuotaStatus {
                agent_id: r.get(0)?,
                max_memories_per_day: r.get(1)?,
                max_storage_bytes: r.get(2)?,
                max_links_per_day: r.get(3)?,
                current_memories_today: r.get(4)?,
                current_storage_bytes: r.get(5)?,
                current_links_today: r.get(6)?,
                day_started_at: r.get(7)?,
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
            })
        },
    )
    .optional()
    .context("failed to load agent quota row")
}

/// Return the YYYY-MM-DD bucket for an RFC3339 UTC timestamp. Used to
/// compare `day_started_at` against "today" without crossing into a
/// chrono date type — the SQL column is RFC3339 string-typed.
fn day_bucket(rfc3339: &str) -> String {
    rfc3339.get(..10).unwrap_or(rfc3339).to_string()
}

/// v0.7 K8 — pre-write quota check. Auto-inserts the default row on
/// first call for an agent. If the agent's `day_started_at` rolled
/// over since the last write, the counters are zeroed inline (the
/// sweeper is the bulk path; this path keeps the per-write quota
/// honest even if the sweeper hasn't fired yet).
///
/// On a clean check, returns `Ok(())`. On a quota breach, returns
/// `Err(QuotaError)` naming the limit that was hit and the
/// counter/ceiling values at the moment of the check.
///
/// # Errors
///
/// - [`QuotaError`] when one of the three limits would be exceeded by
///   the pending op.
/// - Wrapped SQL errors when the substrate read fails.
pub fn check_quota(
    conn: &Connection,
    agent_id: &str,
    op: QuotaOp,
) -> std::result::Result<(), QuotaCheckError> {
    let row = ensure_row(conn, agent_id).map_err(QuotaCheckError::Sql)?;

    // Inline daily-bucket roll: if the stored bucket isn't today, treat
    // the daily counters as 0 for this check. The sweeper performs the
    // matching SQL UPDATE so a downstream `get_status` reports zeros
    // even if no further writes happen until midnight.
    let today = day_bucket(&chrono::Utc::now().to_rfc3339());
    let stored_day = day_bucket(&row.day_started_at);
    let (memories_today, links_today) = if stored_day == today {
        (row.current_memories_today, row.current_links_today)
    } else {
        (0, 0)
    };

    match op {
        QuotaOp::Memory { bytes } => {
            if memories_today + 1 > row.max_memories_per_day {
                return Err(QuotaCheckError::Quota(QuotaError {
                    agent_id: agent_id.to_string(),
                    limit: QuotaLimit::MemoriesPerDay,
                    current: memories_today,
                    max: row.max_memories_per_day,
                }));
            }
            if row.current_storage_bytes + bytes > row.max_storage_bytes {
                return Err(QuotaCheckError::Quota(QuotaError {
                    agent_id: agent_id.to_string(),
                    limit: QuotaLimit::StorageBytes,
                    current: row.current_storage_bytes,
                    max: row.max_storage_bytes,
                }));
            }
        }
        QuotaOp::Link => {
            if links_today + 1 > row.max_links_per_day {
                return Err(QuotaCheckError::Quota(QuotaError {
                    agent_id: agent_id.to_string(),
                    limit: QuotaLimit::LinksPerDay,
                    current: links_today,
                    max: row.max_links_per_day,
                }));
            }
        }
    }

    Ok(())
}

/// Wire-shape error for [`check_quota`] — separates the "the agent
/// hit the limit" case from "the substrate read failed" so callers can
/// surface the former as a `QUOTA_EXCEEDED` diagnostic and the latter
/// as a 500-class internal error.
#[derive(Debug)]
pub enum QuotaCheckError {
    /// The pending op would exceed one of the three limits.
    Quota(QuotaError),
    /// The substrate read failed (DB error, missing migration, etc.).
    Sql(anyhow::Error),
}

impl std::fmt::Display for QuotaCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quota(q) => std::fmt::Display::fmt(q, f),
            Self::Sql(e) => write!(f, "quota check substrate error: {e}"),
        }
    }
}

impl std::error::Error for QuotaCheckError {}

/// v0.7 K8 / H12 (#628 blocker) — atomic check + record. Combines the
/// quota check with the counter increment under a single
/// `BEGIN IMMEDIATE` SQLite transaction so concurrent writers cannot
/// each pass the check and then both increment the counter past the
/// cap. `BEGIN IMMEDIATE` acquires a `RESERVED` lock on the database
/// at the start of the transaction; SQLite serialises every other
/// would-be writer behind the lock until COMMIT/ROLLBACK, which is
/// the SQLite analogue of `SELECT ... FOR UPDATE` against the single
/// `agent_quotas` row.
///
/// On a clean check + increment, returns `Ok(())`. On a quota breach,
/// returns `Err(QuotaError)` naming the limit that was hit and the
/// counter / ceiling values at the moment of the check; the
/// transaction is rolled back so no counter mutation persists.
///
/// Callers replace the previous two-step `check_quota(...)?;
/// op(...)?; record_op(...)?` pattern with `check_and_record(...)?;
/// op(...)?;` (then `refund_op(...)` on op-failure if needed) so the
/// gap between check and record cannot be raced.
///
/// # Errors
///
/// - [`QuotaCheckError::Quota`] when one of the three limits would be
///   exceeded by the pending op.
/// - [`QuotaCheckError::Sql`] when the substrate read or write fails.
pub fn check_and_record(
    conn: &Connection,
    agent_id: &str,
    op: QuotaOp,
) -> std::result::Result<(), QuotaCheckError> {
    // Make sure the row exists OUTSIDE the immediate transaction;
    // `INSERT OR IGNORE` itself is atomic and contention-free.
    let _ = ensure_row(conn, agent_id).map_err(QuotaCheckError::Sql)?;

    // BEGIN IMMEDIATE — acquires a RESERVED lock immediately. This is
    // the SQLite shape of "SELECT ... FOR UPDATE": no other connection
    // can begin a write transaction until we COMMIT or ROLLBACK. The
    // window between SELECT and UPDATE inside the transaction is
    // therefore safe from another writer's UPDATE racing past us.
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| QuotaCheckError::Sql(anyhow::anyhow!("BEGIN IMMEDIATE failed: {e}")))?;

    let result: std::result::Result<(), QuotaCheckError> = (|| {
        let row = load_row(conn, agent_id)
            .map_err(QuotaCheckError::Sql)?
            .ok_or_else(|| {
                QuotaCheckError::Sql(anyhow::anyhow!(
                    "quota row vanished mid-transaction for agent {agent_id}"
                ))
            })?;

        // Inline daily-bucket roll: if the stored bucket isn't today,
        // the daily counters are treated as zero for the check AND
        // the UPDATE below resets them.
        let now = chrono::Utc::now().to_rfc3339();
        let today = day_bucket(&now);
        let stored_day = day_bucket(&row.day_started_at);
        let day_rolled = stored_day != today;
        let (memories_today, links_today) = if day_rolled {
            (0, 0)
        } else {
            (row.current_memories_today, row.current_links_today)
        };

        match op {
            QuotaOp::Memory { bytes } => {
                if memories_today + 1 > row.max_memories_per_day {
                    return Err(QuotaCheckError::Quota(QuotaError {
                        agent_id: agent_id.to_string(),
                        limit: QuotaLimit::MemoriesPerDay,
                        current: memories_today,
                        max: row.max_memories_per_day,
                    }));
                }
                if row.current_storage_bytes + bytes > row.max_storage_bytes {
                    return Err(QuotaCheckError::Quota(QuotaError {
                        agent_id: agent_id.to_string(),
                        limit: QuotaLimit::StorageBytes,
                        current: row.current_storage_bytes,
                        max: row.max_storage_bytes,
                    }));
                }
                if day_rolled {
                    conn.execute(
                        "UPDATE agent_quotas SET
                           current_memories_today = 1,
                           current_links_today = 0,
                           current_storage_bytes = current_storage_bytes + ?1,
                           day_started_at = ?2,
                           updated_at = ?2
                         WHERE agent_id = ?3",
                        params![bytes, now, agent_id],
                    )
                    .map_err(|e| QuotaCheckError::Sql(anyhow::anyhow!("update failed: {e}")))?;
                } else {
                    conn.execute(
                        "UPDATE agent_quotas SET
                           current_memories_today = current_memories_today + 1,
                           current_storage_bytes = current_storage_bytes + ?1,
                           updated_at = ?2
                         WHERE agent_id = ?3",
                        params![bytes, now, agent_id],
                    )
                    .map_err(|e| QuotaCheckError::Sql(anyhow::anyhow!("update failed: {e}")))?;
                }
            }
            QuotaOp::Link => {
                if links_today + 1 > row.max_links_per_day {
                    return Err(QuotaCheckError::Quota(QuotaError {
                        agent_id: agent_id.to_string(),
                        limit: QuotaLimit::LinksPerDay,
                        current: links_today,
                        max: row.max_links_per_day,
                    }));
                }
                if day_rolled {
                    conn.execute(
                        "UPDATE agent_quotas SET
                           current_memories_today = 0,
                           current_links_today = 1,
                           day_started_at = ?1,
                           updated_at = ?1
                         WHERE agent_id = ?2",
                        params![now, agent_id],
                    )
                    .map_err(|e| QuotaCheckError::Sql(anyhow::anyhow!("update failed: {e}")))?;
                } else {
                    conn.execute(
                        "UPDATE agent_quotas SET
                           current_links_today = current_links_today + 1,
                           updated_at = ?1
                         WHERE agent_id = ?2",
                        params![now, agent_id],
                    )
                    .map_err(|e| QuotaCheckError::Sql(anyhow::anyhow!("update failed: {e}")))?;
                }
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| QuotaCheckError::Sql(anyhow::anyhow!("quota commit failed: {e}")))?;
            Ok(())
        }
        Err(e) => {
            // Rollback is best-effort — even if it fails, the
            // transaction is implicitly aborted on connection drop.
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// v0.7 K8 / H12 — refund a previously-recorded op. Used by callers
/// that have already incremented the counters via
/// [`check_and_record`] but whose downstream insert failed AFTER the
/// quota commit. Decrements the same counters [`check_and_record`]
/// incremented; storage bytes is decremented for `QuotaOp::Memory`.
///
/// Counters never go below zero (saturating) so a buggy double-refund
/// cannot poison the substrate.
///
/// # Errors
///
/// Wrapped SQL errors on update failure.
pub fn refund_op(conn: &Connection, agent_id: &str, op: QuotaOp) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    match op {
        QuotaOp::Memory { bytes } => {
            conn.execute(
                "UPDATE agent_quotas SET
                   current_memories_today = MAX(current_memories_today - 1, 0),
                   current_storage_bytes = MAX(current_storage_bytes - ?1, 0),
                   updated_at = ?2
                 WHERE agent_id = ?3",
                params![bytes, now, agent_id],
            )?;
        }
        QuotaOp::Link => {
            conn.execute(
                "UPDATE agent_quotas SET
                   current_links_today = MAX(current_links_today - 1, 0),
                   updated_at = ?1
                 WHERE agent_id = ?2",
                params![now, agent_id],
            )?;
        }
    }
    Ok(())
}

/// v0.7 K8 — record a successful write against the agent's quota
/// counters. Called AFTER the underlying insert succeeds so a failed
/// store does not consume quota.
///
/// **DEPRECATED for new code paths**: prefer [`check_and_record`]
/// which combines the check + record into a single atomic transaction
/// (closes H12 TOCTOU). `record_op` remains for callers (and tests)
/// that bypass the check phase entirely.
///
/// If the stored `day_started_at` rolled over since the row was last
/// touched, the daily counters are reset before the new op posts —
/// matching the inline-roll semantics in [`check_quota`] so the two
/// stay coherent without an intervening sweep.
///
/// # Errors
///
/// Wrapped SQL errors on update failure.
pub fn record_op(conn: &Connection, agent_id: &str, op: QuotaOp) -> Result<()> {
    // ensure_row is idempotent so callers that skip check_quota (none
    // today, but defensive) still produce a coherent counter.
    let row = ensure_row(conn, agent_id)?;
    let now = chrono::Utc::now().to_rfc3339();
    let today = day_bucket(&now);
    let stored_day = day_bucket(&row.day_started_at);
    let day_rolled = stored_day != today;

    match op {
        QuotaOp::Memory { bytes } => {
            if day_rolled {
                conn.execute(
                    "UPDATE agent_quotas SET
                       current_memories_today = 1,
                       current_links_today = 0,
                       current_storage_bytes = current_storage_bytes + ?1,
                       day_started_at = ?2,
                       updated_at = ?2
                     WHERE agent_id = ?3",
                    params![bytes, now, agent_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE agent_quotas SET
                       current_memories_today = current_memories_today + 1,
                       current_storage_bytes = current_storage_bytes + ?1,
                       updated_at = ?2
                     WHERE agent_id = ?3",
                    params![bytes, now, agent_id],
                )?;
            }
        }
        QuotaOp::Link => {
            if day_rolled {
                conn.execute(
                    "UPDATE agent_quotas SET
                       current_memories_today = 0,
                       current_links_today = 1,
                       day_started_at = ?1,
                       updated_at = ?1
                     WHERE agent_id = ?2",
                    params![now, agent_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE agent_quotas SET
                       current_links_today = current_links_today + 1,
                       updated_at = ?1
                     WHERE agent_id = ?2",
                    params![now, agent_id],
                )?;
            }
        }
    }
    Ok(())
}

/// v0.7 K8 — daily counter reset. Zeros `current_memories_today` +
/// `current_links_today` for every row whose `day_started_at` is not
/// the current UTC date. Driven by the K8 sweep loop on a 60-second
/// cadence; the inline-roll branch in [`check_quota`] / [`record_op`]
/// is the per-write fallback so the substrate stays honest even if
/// the sweeper is delayed.
///
/// Returns the number of rows that were reset (0 when no agent has
/// crossed midnight since the previous sweep).
///
/// # Errors
///
/// Wrapped SQL errors on update failure.
pub fn reset_daily(conn: &Connection) -> Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    let today = day_bucket(&now);
    let affected = conn.execute(
        "UPDATE agent_quotas SET
           current_memories_today = 0,
           current_links_today = 0,
           day_started_at = ?1,
           updated_at = ?1
         WHERE substr(day_started_at, 1, 10) <> ?2",
        params![now, today],
    )?;
    Ok(affected)
}

/// v0.7 K8 — read the current quota row for an agent, auto-inserting a
/// default row if none exists. Backs the `memory_quota_status` MCP
/// tool.
///
/// # Errors
///
/// Wrapped SQL errors on read failure.
pub fn get_status(conn: &Connection, agent_id: &str) -> Result<QuotaStatus> {
    ensure_row(conn, agent_id)
}

/// v0.7 K8 — read every quota row in the substrate. Backs the
/// `memory_quota_status` MCP tool when the operator omits the
/// `agent_id` parameter (operator-facing surface).
///
/// # Errors
///
/// Wrapped SQL errors on read failure.
pub fn list_status(conn: &Connection) -> Result<Vec<QuotaStatus>> {
    let mut stmt = conn
        .prepare(
            "SELECT agent_id, max_memories_per_day, max_storage_bytes, max_links_per_day,
                    current_memories_today, current_storage_bytes, current_links_today,
                    day_started_at, created_at, updated_at
             FROM agent_quotas ORDER BY agent_id ASC",
        )
        .context("failed to prepare quota list query")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(QuotaStatus {
                agent_id: r.get(0)?,
                max_memories_per_day: r.get(1)?,
                max_storage_bytes: r.get(2)?,
                max_links_per_day: r.get(3)?,
                current_memories_today: r.get(4)?,
                current_storage_bytes: r.get(5)?,
                current_links_today: r.get(6)?,
                day_started_at: r.get(7)?,
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
            })
        })
        .context("failed to query quota rows")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("failed to materialize quota row")?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory");
        // Apply the K8 substrate via the production migration. We
        // can't call db::open() on `:memory:` because it sets pragmas
        // that need a real path, so we hand-apply the substrate.
        conn.execute_batch(include_str!(
            "../migrations/sqlite/0022_v07_agent_quotas.sql"
        ))
        .expect("apply K8 migration");
        conn
    }

    #[test]
    fn check_quota_under_limit_returns_ok() {
        let conn = fresh_db();
        assert!(check_quota(&conn, "agent-a", QuotaOp::Memory { bytes: 100 }).is_ok());
    }

    #[test]
    fn check_quota_at_memory_limit_returns_quota_exceeded() {
        let conn = fresh_db();
        // Tighten the cap to 1 so a single store-then-store sequence
        // hits the wall.
        conn.execute(
            "UPDATE agent_quotas SET max_memories_per_day = 1 WHERE agent_id = ?1",
            params!["agent-a"],
        )
        .ok();
        // First call inserts the default row; tighten after.
        check_quota(&conn, "agent-a", QuotaOp::Memory { bytes: 1 }).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET max_memories_per_day = 1 WHERE agent_id = ?1",
            params!["agent-a"],
        )
        .unwrap();
        record_op(&conn, "agent-a", QuotaOp::Memory { bytes: 1 }).unwrap();
        let err = check_quota(&conn, "agent-a", QuotaOp::Memory { bytes: 1 }).unwrap_err();
        match err {
            QuotaCheckError::Quota(q) => {
                assert_eq!(q.limit, QuotaLimit::MemoriesPerDay);
                assert_eq!(q.max, 1);
            }
            QuotaCheckError::Sql(e) => panic!("expected QuotaError, got SQL: {e}"),
        }
    }

    #[test]
    fn check_quota_storage_bytes_limit_fires() {
        let conn = fresh_db();
        check_quota(&conn, "agent-b", QuotaOp::Memory { bytes: 1 }).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET max_storage_bytes = 100 WHERE agent_id = ?1",
            params!["agent-b"],
        )
        .unwrap();
        let err = check_quota(&conn, "agent-b", QuotaOp::Memory { bytes: 200 }).unwrap_err();
        match err {
            QuotaCheckError::Quota(q) => assert_eq!(q.limit, QuotaLimit::StorageBytes),
            QuotaCheckError::Sql(e) => panic!("expected QuotaError, got SQL: {e}"),
        }
    }

    #[test]
    fn check_quota_links_per_day_limit_fires() {
        let conn = fresh_db();
        check_quota(&conn, "agent-c", QuotaOp::Link).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET max_links_per_day = 1, current_links_today = 1
             WHERE agent_id = ?1",
            params!["agent-c"],
        )
        .unwrap();
        let err = check_quota(&conn, "agent-c", QuotaOp::Link).unwrap_err();
        match err {
            QuotaCheckError::Quota(q) => assert_eq!(q.limit, QuotaLimit::LinksPerDay),
            QuotaCheckError::Sql(e) => panic!("expected QuotaError, got SQL: {e}"),
        }
    }

    #[test]
    fn record_op_increments_counters() {
        let conn = fresh_db();
        record_op(&conn, "agent-d", QuotaOp::Memory { bytes: 42 }).unwrap();
        let s = get_status(&conn, "agent-d").unwrap();
        assert_eq!(s.current_memories_today, 1);
        assert_eq!(s.current_storage_bytes, 42);
        record_op(&conn, "agent-d", QuotaOp::Link).unwrap();
        let s2 = get_status(&conn, "agent-d").unwrap();
        assert_eq!(s2.current_links_today, 1);
    }

    #[test]
    fn reset_daily_zeros_stale_rows_only() {
        let conn = fresh_db();
        record_op(&conn, "agent-e", QuotaOp::Memory { bytes: 10 }).unwrap();
        record_op(&conn, "agent-f", QuotaOp::Link).unwrap();
        // Roll agent-e's day_started_at back to yesterday.
        conn.execute(
            "UPDATE agent_quotas SET day_started_at = '2020-01-01T00:00:00+00:00'
             WHERE agent_id = ?1",
            params!["agent-e"],
        )
        .unwrap();
        let n = reset_daily(&conn).unwrap();
        assert_eq!(n, 1, "exactly one stale row should be reset");
        let s_e = get_status(&conn, "agent-e").unwrap();
        assert_eq!(s_e.current_memories_today, 0);
        let s_f = get_status(&conn, "agent-f").unwrap();
        assert_eq!(
            s_f.current_links_today, 1,
            "fresh row must not be touched by the daily reset"
        );
        // Storage is lifetime, never reset.
        assert_eq!(s_e.current_storage_bytes, 10);
    }

    #[test]
    fn list_status_returns_all_rows_sorted() {
        let conn = fresh_db();
        record_op(&conn, "z-agent", QuotaOp::Memory { bytes: 1 }).unwrap();
        record_op(&conn, "a-agent", QuotaOp::Memory { bytes: 1 }).unwrap();
        record_op(&conn, "m-agent", QuotaOp::Memory { bytes: 1 }).unwrap();
        let rows = list_status(&conn).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["a-agent", "m-agent", "z-agent"]);
    }

    #[test]
    fn get_status_auto_inserts_default_row() {
        let conn = fresh_db();
        let s = get_status(&conn, "fresh-agent").unwrap();
        assert_eq!(s.max_memories_per_day, DEFAULT_MAX_MEMORIES_PER_DAY);
        assert_eq!(s.max_storage_bytes, DEFAULT_MAX_STORAGE_BYTES);
        assert_eq!(s.max_links_per_day, DEFAULT_MAX_LINKS_PER_DAY);
        assert_eq!(s.current_memories_today, 0);
    }

    #[test]
    fn quota_limit_as_str_returns_expected_canonical_form() {
        assert_eq!(QuotaLimit::MemoriesPerDay.as_str(), "memories_per_day");
        assert_eq!(QuotaLimit::StorageBytes.as_str(), "storage_bytes");
        assert_eq!(QuotaLimit::LinksPerDay.as_str(), "links_per_day");
    }

    #[test]
    fn quota_error_display_format_contract() {
        let err = QuotaError {
            agent_id: "alice".to_string(),
            limit: QuotaLimit::StorageBytes,
            current: 1024,
            max: 2048,
        };
        let s = format!("{err}");
        assert!(s.contains("QUOTA_EXCEEDED"));
        assert!(s.contains("alice"));
        assert!(s.contains("storage_bytes"));
        assert!(s.contains("current=1024"));
        assert!(s.contains("max=2048"));
        // Trait surface: std::error::Error
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn quota_check_error_display_quota_variant_delegates_to_inner() {
        let err = QuotaCheckError::Quota(QuotaError {
            agent_id: "bob".to_string(),
            limit: QuotaLimit::MemoriesPerDay,
            current: 99,
            max: 100,
        });
        let s = format!("{err}");
        assert!(s.contains("QUOTA_EXCEEDED"));
        assert!(s.contains("memories_per_day"));
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn quota_check_error_display_sql_variant_wraps_substrate_error() {
        let err = QuotaCheckError::Sql(anyhow::anyhow!("boom"));
        let s = format!("{err}");
        assert!(s.contains("quota check substrate error"));
        assert!(s.contains("boom"));
    }

    #[test]
    fn check_and_record_under_limit_increments_counters() {
        let conn = fresh_db();
        check_and_record(&conn, "agent-cr-a", QuotaOp::Memory { bytes: 50 }).unwrap();
        let s = get_status(&conn, "agent-cr-a").unwrap();
        assert_eq!(s.current_memories_today, 1);
        assert_eq!(s.current_storage_bytes, 50);
        check_and_record(&conn, "agent-cr-a", QuotaOp::Link).unwrap();
        let s2 = get_status(&conn, "agent-cr-a").unwrap();
        assert_eq!(s2.current_links_today, 1);
    }

    #[test]
    fn check_and_record_at_memories_limit_returns_quota_error_and_rolls_back() {
        let conn = fresh_db();
        check_and_record(&conn, "agent-cr-b", QuotaOp::Memory { bytes: 1 }).unwrap();
        // Tighten the cap so the next write would exceed.
        conn.execute(
            "UPDATE agent_quotas SET max_memories_per_day = 1 WHERE agent_id = ?1",
            params!["agent-cr-b"],
        )
        .unwrap();
        let err = check_and_record(&conn, "agent-cr-b", QuotaOp::Memory { bytes: 1 }).unwrap_err();
        match err {
            QuotaCheckError::Quota(q) => {
                assert_eq!(q.limit, QuotaLimit::MemoriesPerDay);
            }
            QuotaCheckError::Sql(e) => panic!("expected Quota, got SQL: {e}"),
        }
        // Counter NOT incremented (rollback).
        let s = get_status(&conn, "agent-cr-b").unwrap();
        assert_eq!(s.current_memories_today, 1);
    }

    #[test]
    fn check_and_record_storage_limit_returns_quota_error() {
        let conn = fresh_db();
        check_and_record(&conn, "agent-cr-c", QuotaOp::Memory { bytes: 1 }).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET max_storage_bytes = 100 WHERE agent_id = ?1",
            params!["agent-cr-c"],
        )
        .unwrap();
        let err = check_and_record(&conn, "agent-cr-c", QuotaOp::Memory { bytes: 1000 })
            .expect_err("storage cap should fire");
        match err {
            QuotaCheckError::Quota(q) => assert_eq!(q.limit, QuotaLimit::StorageBytes),
            QuotaCheckError::Sql(e) => panic!("expected quota, got SQL: {e}"),
        }
    }

    #[test]
    fn check_and_record_links_limit_returns_quota_error() {
        let conn = fresh_db();
        check_and_record(&conn, "agent-cr-d", QuotaOp::Link).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET max_links_per_day = 1 WHERE agent_id = ?1",
            params!["agent-cr-d"],
        )
        .unwrap();
        let err = check_and_record(&conn, "agent-cr-d", QuotaOp::Link)
            .expect_err("links cap should fire");
        match err {
            QuotaCheckError::Quota(q) => assert_eq!(q.limit, QuotaLimit::LinksPerDay),
            QuotaCheckError::Sql(e) => panic!("expected quota, got SQL: {e}"),
        }
    }

    #[test]
    fn check_and_record_day_roll_branch_for_memory_zeros_daily_counters() {
        let conn = fresh_db();
        // Seed yesterday's row with non-zero counters.
        check_and_record(&conn, "agent-cr-e", QuotaOp::Memory { bytes: 10 }).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET day_started_at = '2020-01-01T00:00:00+00:00',
                current_memories_today = 999, current_links_today = 7
             WHERE agent_id = ?1",
            params!["agent-cr-e"],
        )
        .unwrap();
        check_and_record(&conn, "agent-cr-e", QuotaOp::Memory { bytes: 5 }).unwrap();
        let s = get_status(&conn, "agent-cr-e").unwrap();
        // Day rolled: memories reset to 1 (this one), links reset to 0.
        assert_eq!(s.current_memories_today, 1);
        assert_eq!(s.current_links_today, 0);
        // Storage is lifetime, accumulates.
        assert_eq!(s.current_storage_bytes, 15);
    }

    #[test]
    fn check_and_record_day_roll_branch_for_link_resets_daily_counters() {
        let conn = fresh_db();
        check_and_record(&conn, "agent-cr-f", QuotaOp::Link).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET day_started_at = '2020-01-01T00:00:00+00:00',
                current_memories_today = 50, current_links_today = 8
             WHERE agent_id = ?1",
            params!["agent-cr-f"],
        )
        .unwrap();
        check_and_record(&conn, "agent-cr-f", QuotaOp::Link).unwrap();
        let s = get_status(&conn, "agent-cr-f").unwrap();
        assert_eq!(s.current_memories_today, 0);
        assert_eq!(s.current_links_today, 1);
    }

    #[test]
    fn refund_op_memory_decrements_counters_saturating_to_zero() {
        let conn = fresh_db();
        check_and_record(&conn, "agent-rf-a", QuotaOp::Memory { bytes: 200 }).unwrap();
        refund_op(&conn, "agent-rf-a", QuotaOp::Memory { bytes: 200 }).unwrap();
        let s = get_status(&conn, "agent-rf-a").unwrap();
        assert_eq!(s.current_memories_today, 0);
        assert_eq!(s.current_storage_bytes, 0);
        // Saturating: double-refund stays at 0.
        refund_op(&conn, "agent-rf-a", QuotaOp::Memory { bytes: 200 }).unwrap();
        let s2 = get_status(&conn, "agent-rf-a").unwrap();
        assert_eq!(s2.current_memories_today, 0);
        assert_eq!(s2.current_storage_bytes, 0);
    }

    #[test]
    fn refund_op_link_decrements_counter_saturating_to_zero() {
        let conn = fresh_db();
        check_and_record(&conn, "agent-rf-b", QuotaOp::Link).unwrap();
        refund_op(&conn, "agent-rf-b", QuotaOp::Link).unwrap();
        let s = get_status(&conn, "agent-rf-b").unwrap();
        assert_eq!(s.current_links_today, 0);
        // Saturating.
        refund_op(&conn, "agent-rf-b", QuotaOp::Link).unwrap();
        let s2 = get_status(&conn, "agent-rf-b").unwrap();
        assert_eq!(s2.current_links_today, 0);
    }

    #[test]
    fn record_op_day_roll_branch_for_memory() {
        let conn = fresh_db();
        record_op(&conn, "agent-ro-a", QuotaOp::Memory { bytes: 100 }).unwrap();
        // Roll day backwards + populate non-zero counters.
        conn.execute(
            "UPDATE agent_quotas SET day_started_at = '2020-01-01T00:00:00+00:00',
                current_memories_today = 50, current_links_today = 4
             WHERE agent_id = ?1",
            params!["agent-ro-a"],
        )
        .unwrap();
        record_op(&conn, "agent-ro-a", QuotaOp::Memory { bytes: 5 }).unwrap();
        let s = get_status(&conn, "agent-ro-a").unwrap();
        assert_eq!(s.current_memories_today, 1);
        assert_eq!(s.current_links_today, 0);
        assert_eq!(s.current_storage_bytes, 105);
    }

    #[test]
    fn record_op_day_roll_branch_for_link() {
        let conn = fresh_db();
        record_op(&conn, "agent-ro-b", QuotaOp::Link).unwrap();
        conn.execute(
            "UPDATE agent_quotas SET day_started_at = '2020-01-01T00:00:00+00:00',
                current_memories_today = 7, current_links_today = 9
             WHERE agent_id = ?1",
            params!["agent-ro-b"],
        )
        .unwrap();
        record_op(&conn, "agent-ro-b", QuotaOp::Link).unwrap();
        let s = get_status(&conn, "agent-ro-b").unwrap();
        assert_eq!(s.current_memories_today, 0);
        assert_eq!(s.current_links_today, 1);
    }

    #[test]
    fn quota_status_serde_roundtrip() {
        let conn = fresh_db();
        let s = get_status(&conn, "ser-agent").unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let parsed: QuotaStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, "ser-agent");
        assert_eq!(parsed.max_memories_per_day, DEFAULT_MAX_MEMORIES_PER_DAY);
    }

    #[test]
    fn check_quota_day_roll_branch_treats_daily_as_zero() {
        let conn = fresh_db();
        check_quota(&conn, "agent-cq-roll", QuotaOp::Memory { bytes: 1 }).unwrap();
        // Roll day backwards with high counters.
        conn.execute(
            "UPDATE agent_quotas SET day_started_at = '2020-01-01T00:00:00+00:00',
                current_memories_today = 99999, current_links_today = 99999
             WHERE agent_id = ?1",
            params!["agent-cq-roll"],
        )
        .unwrap();
        // Despite the inflated stored counter, the inline roll resets the
        // effective counter to 0 so the check passes.
        assert!(check_quota(&conn, "agent-cq-roll", QuotaOp::Memory { bytes: 1 }).is_ok());
        assert!(check_quota(&conn, "agent-cq-roll", QuotaOp::Link).is_ok());
    }
}
