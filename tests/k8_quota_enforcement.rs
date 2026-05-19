// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
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

mod common;
use common::fresh_db_tempfile_path as fresh_db;

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

/// H12 (#628 blocker) — concurrent writers must not each pass the
/// quota check and then both record_op past the cap. The
/// `check_and_record` API combines both operations into a single
/// `BEGIN IMMEDIATE` SQLite transaction so SQLite serialises every
/// other would-be writer behind the row lock. Spawn 10 threads each
/// trying to store one memory at a quota cap of 1; exactly 1 must
/// succeed and 9 must see `QUOTA_EXCEEDED`.
#[test]
fn k8_check_and_record_serialises_concurrent_writers_h12() {
    let (_keep, db_path) = fresh_db();

    // Seed the row with the default caps, then tighten memories cap
    // to 1. The first thread that wins the BEGIN IMMEDIATE lock will
    // commit a count of 1; every other thread must see QUOTA_EXCEEDED.
    {
        let conn = Connection::open(&db_path).unwrap();
        ai_memory::quotas::check_and_record(&conn, "race-agent", QuotaOp::Memory { bytes: 1 })
            .expect("seed insert");
        // Reset the counter back to zero so the cap-1 race below can
        // play out from a clean slate.
        conn.execute(
            "UPDATE agent_quotas SET
               max_memories_per_day = 1,
               current_memories_today = 0
             WHERE agent_id = ?1",
            params!["race-agent"],
        )
        .unwrap();
    }

    // Spawn 10 threads. Each opens its own connection to the shared
    // on-disk database, then races to call `check_and_record`. SQLite
    // WAL mode permits concurrent readers, but writers serialise on
    // the RESERVED lock acquired by `BEGIN IMMEDIATE` — exactly the
    // shape `check_and_record` relies on.
    let path = std::sync::Arc::new(db_path.clone());
    let mut handles = Vec::new();
    for _ in 0..10 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || -> bool {
            // Each thread retries on `SQLITE_BUSY` (the lock-waiter
            // signal) so the race is decided by quota state, not by
            // the OS scheduler dropping a busy retry. Cap retries to
            // avoid an infinite loop if something unexpected fails.
            let conn = {
                let c = Connection::open(&*p).expect("open");
                c.busy_timeout(std::time::Duration::from_secs(5))
                    .expect("set busy timeout");
                c
            };
            matches!(
                ai_memory::quotas::check_and_record(
                    &conn,
                    "race-agent",
                    QuotaOp::Memory { bytes: 1 },
                ),
                Ok(()),
            )
        }));
    }

    let mut successes = 0;
    let mut failures = 0;
    for h in handles {
        if h.join().expect("thread join") {
            successes += 1;
        } else {
            failures += 1;
        }
    }

    assert_eq!(
        successes, 1,
        "exactly one thread must commit past the cap-1 quota; got {successes} successes / {failures} failures"
    );
    assert_eq!(
        failures, 9,
        "the other nine threads must see QUOTA_EXCEEDED; got {successes} successes / {failures} failures"
    );

    // The persisted counter must read exactly 1 — no double-increment
    // could have slipped past the BEGIN IMMEDIATE lock.
    let conn = Connection::open(&db_path).unwrap();
    let s = ai_memory::quotas::get_status(&conn, "race-agent").unwrap();
    assert_eq!(
        s.current_memories_today, 1,
        "counter should be exactly 1 after the race"
    );
}

/// H12 — `refund_op` rolls back a successfully-recorded op when the
/// downstream insert fails. Callers use this to keep the quota
/// counter coherent with the actual successful-write count.
#[test]
fn k8_refund_op_decrements_counters_h12() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    ai_memory::quotas::check_and_record(&conn, "refund-agent", QuotaOp::Memory { bytes: 100 })
        .unwrap();
    let pre = ai_memory::quotas::get_status(&conn, "refund-agent").unwrap();
    assert_eq!(pre.current_memories_today, 1);
    assert_eq!(pre.current_storage_bytes, 100);

    ai_memory::quotas::refund_op(&conn, "refund-agent", QuotaOp::Memory { bytes: 100 }).unwrap();
    let post = ai_memory::quotas::get_status(&conn, "refund-agent").unwrap();
    assert_eq!(post.current_memories_today, 0);
    assert_eq!(post.current_storage_bytes, 0);

    // Saturating: extra refunds must not push counters below zero.
    ai_memory::quotas::refund_op(&conn, "refund-agent", QuotaOp::Memory { bytes: 100 }).unwrap();
    let saturated = ai_memory::quotas::get_status(&conn, "refund-agent").unwrap();
    assert_eq!(saturated.current_memories_today, 0);
    assert_eq!(saturated.current_storage_bytes, 0);
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
