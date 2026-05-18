// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K8 — daily reset of `agent_quotas.current_*_today` columns.
//!
//! The K8 sweep loop ([`crate::daemon_runtime::spawn_agent_quota_reset_loop`])
//! periodically calls [`crate::quotas::reset_daily`] to zero
//! `current_memories_today` + `current_links_today` for every row whose
//! `day_started_at` predates the current UTC date. This test pins the
//! reset SQL semantics directly:
//!
//! 1. Two rows seeded — one with `day_started_at` rolled back to "yesterday",
//!    one fresh.
//! 2. `reset_daily` fires.
//! 3. The stale row's daily counters are zeroed; the fresh row is untouched.
//! 4. `current_storage_bytes` (lifetime) is preserved on both.

use ai_memory::quotas::{self, QuotaOp};
use rusqlite::{Connection, params};

mod common;
use common::fresh_db_tempfile_path as fresh_db;

#[test]
fn k8_daily_reset_zeros_stale_rows_only() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    // Seed two agents with non-zero daily counters + non-zero storage.
    for op in [
        QuotaOp::Memory { bytes: 17 },
        QuotaOp::Link,
        QuotaOp::Memory { bytes: 23 },
    ] {
        quotas::record_op(&conn, "agent-stale", op).unwrap();
    }
    for op in [QuotaOp::Memory { bytes: 99 }, QuotaOp::Link] {
        quotas::record_op(&conn, "agent-fresh", op).unwrap();
    }

    // Roll agent-stale's day_started_at back to a deterministic
    // yesterday — the reset SQL compares the stored YYYY-MM-DD prefix
    // against today, so any pre-2026 RFC3339 is unambiguously "stale".
    conn.execute(
        "UPDATE agent_quotas SET day_started_at = '2020-01-01T00:00:00+00:00'
         WHERE agent_id = ?1",
        params!["agent-stale"],
    )
    .unwrap();

    let stale_before = quotas::get_status(&conn, "agent-stale").unwrap();
    assert!(
        stale_before.current_memories_today > 0,
        "precondition: stale row must have non-zero current_memories_today before reset"
    );
    let fresh_before = quotas::get_status(&conn, "agent-fresh").unwrap();
    assert_eq!(fresh_before.current_memories_today, 1);
    assert_eq!(fresh_before.current_links_today, 1);

    // Drive the sweep — same call the K8 sweep loop makes every 60s.
    let reset_count = quotas::reset_daily(&conn).unwrap();
    assert_eq!(
        reset_count, 1,
        "exactly one stale row should reset (agent-stale); got {reset_count}"
    );

    // agent-stale: daily counters zeroed; storage preserved (lifetime).
    let stale_after = quotas::get_status(&conn, "agent-stale").unwrap();
    assert_eq!(stale_after.current_memories_today, 0);
    assert_eq!(stale_after.current_links_today, 0);
    assert_eq!(
        stale_after.current_storage_bytes, 40,
        "lifetime storage must NOT be reset by daily sweep (17+23=40)"
    );
    // day_started_at must roll forward to today's date.
    let today = chrono::Utc::now().to_rfc3339();
    assert_eq!(
        &stale_after.day_started_at[..10],
        &today[..10],
        "day_started_at must advance to today after a reset"
    );

    // agent-fresh: untouched.
    let fresh_after = quotas::get_status(&conn, "agent-fresh").unwrap();
    assert_eq!(fresh_after.current_memories_today, 1);
    assert_eq!(fresh_after.current_links_today, 1);
    assert_eq!(fresh_after.current_storage_bytes, 99);
}

#[test]
fn k8_daily_reset_idempotent_when_no_rows_stale() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    quotas::record_op(&conn, "agent-x", QuotaOp::Memory { bytes: 1 }).unwrap();
    quotas::record_op(&conn, "agent-y", QuotaOp::Link).unwrap();

    let n1 = quotas::reset_daily(&conn).unwrap();
    let n2 = quotas::reset_daily(&conn).unwrap();
    assert_eq!(n1, 0, "no stale rows on first sweep");
    assert_eq!(n2, 0, "still no stale rows on the immediate re-sweep");

    let x = quotas::get_status(&conn, "agent-x").unwrap();
    assert_eq!(x.current_memories_today, 1);
    let y = quotas::get_status(&conn, "agent-y").unwrap();
    assert_eq!(y.current_links_today, 1);
}
