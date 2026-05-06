// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K8 — quota enforcement on the store + link write paths.
//!
//! K8 ships the per-agent quota substrate. The substrate-level checks
//! against [`crate::quotas::check_quota`] / [`crate::quotas::record_op`]
//! live in `src/quotas.rs::tests` and exercise the inline-roll +
//! daily-reset semantics directly. This integration test pins the
//! enforcement seam — store under limit succeeds, store at limit
//! returns a `QUOTA_EXCEEDED` diagnostic naming the limit hit.

use ai_memory::quotas::{
    self, DEFAULT_MAX_LINKS_PER_DAY, DEFAULT_MAX_MEMORIES_PER_DAY, DEFAULT_MAX_STORAGE_BYTES,
    QuotaCheckError, QuotaLimit, QuotaOp,
};
use rusqlite::{Connection, params};
use tempfile::NamedTempFile;

/// Stand up a fresh on-disk SQLite at a tempfile path with the
/// production schema applied (incl. the K8 `agent_quotas` migration).
fn fresh_db() -> (NamedTempFile, std::path::PathBuf) {
    let f = NamedTempFile::new().expect("tempfile");
    let p = f.path().to_path_buf();
    let _ = ai_memory::db::open(&p).expect("db::open");
    (f, p)
}

/// Tighten a row's caps so the test can hit the wall in O(1) calls.
fn tighten_caps(
    conn: &Connection,
    agent_id: &str,
    max_memories_per_day: i64,
    max_storage_bytes: i64,
    max_links_per_day: i64,
) {
    conn.execute(
        "UPDATE agent_quotas SET
           max_memories_per_day = ?1,
           max_storage_bytes    = ?2,
           max_links_per_day    = ?3
         WHERE agent_id = ?4",
        params![
            max_memories_per_day,
            max_storage_bytes,
            max_links_per_day,
            agent_id
        ],
    )
    .expect("tighten caps");
}

#[test]
fn k8_store_under_limit_returns_ok() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    // First call inserts the default row with the generous compiled
    // defaults; under those defaults a single 100-byte store passes.
    quotas::check_quota(&conn, "agent-under-limit", QuotaOp::Memory { bytes: 100 })
        .expect("under limit must succeed");

    let status = quotas::get_status(&conn, "agent-under-limit").unwrap();
    assert_eq!(status.max_memories_per_day, DEFAULT_MAX_MEMORIES_PER_DAY);
    assert_eq!(status.max_storage_bytes, DEFAULT_MAX_STORAGE_BYTES);
    assert_eq!(status.max_links_per_day, DEFAULT_MAX_LINKS_PER_DAY);
}

#[test]
fn k8_store_at_memories_per_day_limit_returns_quota_exceeded() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    // Seed the row by passing a check, then tighten the cap to 1 and
    // record one op so the next check trips memories_per_day.
    quotas::check_quota(&conn, "agent-mem", QuotaOp::Memory { bytes: 1 }).unwrap();
    tighten_caps(&conn, "agent-mem", 1, DEFAULT_MAX_STORAGE_BYTES, 1000);
    quotas::record_op(&conn, "agent-mem", QuotaOp::Memory { bytes: 1 }).unwrap();

    let err = quotas::check_quota(&conn, "agent-mem", QuotaOp::Memory { bytes: 1 })
        .expect_err("expected QUOTA_EXCEEDED");

    match err {
        QuotaCheckError::Quota(q) => {
            assert_eq!(q.limit, QuotaLimit::MemoriesPerDay);
            assert_eq!(q.max, 1);
            assert_eq!(q.current, 1);
            assert_eq!(q.agent_id, "agent-mem");
            // The Display impl includes the literal "QUOTA_EXCEEDED"
            // marker the MCP layer uses to surface the diagnostic name
            // to callers without parsing the message.
            let s = q.to_string();
            assert!(s.contains("QUOTA_EXCEEDED"), "expected marker in {s}");
            assert!(s.contains("memories_per_day"), "expected limit name in {s}");
        }
        QuotaCheckError::Sql(e) => panic!("expected QuotaError, got SQL error: {e}"),
    }
}

#[test]
fn k8_store_at_storage_bytes_limit_returns_quota_exceeded() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    // Seed the row, tighten the storage cap, then attempt a write that
    // would push current_storage_bytes past the cap.
    quotas::check_quota(&conn, "agent-bytes", QuotaOp::Memory { bytes: 1 }).unwrap();
    tighten_caps(&conn, "agent-bytes", 1000, 50, 1000);

    let err = quotas::check_quota(&conn, "agent-bytes", QuotaOp::Memory { bytes: 200 })
        .expect_err("expected QUOTA_EXCEEDED");
    match err {
        QuotaCheckError::Quota(q) => {
            assert_eq!(q.limit, QuotaLimit::StorageBytes);
            assert_eq!(q.max, 50);
        }
        QuotaCheckError::Sql(e) => panic!("expected QuotaError, got SQL error: {e}"),
    }
}

#[test]
fn k8_link_at_links_per_day_limit_returns_quota_exceeded() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    quotas::check_quota(&conn, "agent-links", QuotaOp::Link).unwrap();
    tighten_caps(&conn, "agent-links", 1000, DEFAULT_MAX_STORAGE_BYTES, 1);
    quotas::record_op(&conn, "agent-links", QuotaOp::Link).unwrap();

    let err = quotas::check_quota(&conn, "agent-links", QuotaOp::Link)
        .expect_err("expected QUOTA_EXCEEDED");
    match err {
        QuotaCheckError::Quota(q) => {
            assert_eq!(q.limit, QuotaLimit::LinksPerDay);
            assert_eq!(q.max, 1);
        }
        QuotaCheckError::Sql(e) => panic!("expected QuotaError, got SQL error: {e}"),
    }
}

#[test]
fn k8_record_op_after_check_increments_counters() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    // Three memory writes + two link writes against the same agent.
    for _ in 0..3 {
        quotas::check_quota(&conn, "agent-record", QuotaOp::Memory { bytes: 10 }).unwrap();
        quotas::record_op(&conn, "agent-record", QuotaOp::Memory { bytes: 10 }).unwrap();
    }
    for _ in 0..2 {
        quotas::check_quota(&conn, "agent-record", QuotaOp::Link).unwrap();
        quotas::record_op(&conn, "agent-record", QuotaOp::Link).unwrap();
    }
    let status = quotas::get_status(&conn, "agent-record").unwrap();
    assert_eq!(status.current_memories_today, 3);
    assert_eq!(status.current_storage_bytes, 30);
    assert_eq!(status.current_links_today, 2);
}
