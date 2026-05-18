// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]

//! v0.7.0 Provenance Gap 1 (issue #884) — optimistic-concurrency
//! regression pin. Two concurrent updates against the same memory
//! must produce exactly one winner; the loser receives a typed
//! `VersionConflict` envelope naming both the expected + current
//! `version` so it can re-read and retry.
//!
//! Mirrors the schema v45 migration arm: every memory row carries a
//! `version BIGINT NOT NULL DEFAULT 1` column, bumped on every
//! mutation through `storage::update`. The MCP `memory_update` tool
//! and the HTTP `PUT /memories/:id` handler both honor an
//! `expected_version` gate.

use ai_memory::db;
use ai_memory::models::{Memory, Tier};
use ai_memory::storage::VersionConflict;
use std::path::Path;

fn open_test_db() -> rusqlite::Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

fn make_memory(title: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        content: format!("content for {title}"),
        namespace: "concurrency-test".to_string(),
        tier: Tier::Mid,
        created_at: now.clone(),
        updated_at: now,
        ..Default::default()
    }
}

#[test]
fn new_row_lands_at_version_one() {
    let conn = open_test_db();
    let mem = make_memory("v1-default");
    let id = db::insert(&conn, &mem).expect("insert");
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(
        stored.version, 1,
        "fresh row must default to version=1 per SQL DEFAULT"
    );
}

#[test]
fn in_place_update_bumps_version_monotonically() {
    let conn = open_test_db();
    let mem = make_memory("bumps");
    let id = db::insert(&conn, &mem).expect("insert");
    for expected_after in 2..=5_i64 {
        let (found, _) = db::update(
            &conn,
            &id,
            None,
            Some(&format!("body-{expected_after}")),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("update");
        assert!(found);
        let stored = db::get(&conn, &id).expect("get").expect("present");
        assert_eq!(stored.version, expected_after);
    }
}

#[test]
fn expected_version_match_succeeds_and_bumps() {
    let conn = open_test_db();
    let mem = make_memory("match-gate");
    let id = db::insert(&conn, &mem).expect("insert");
    let (found, _) = db::update_with_expected_version(
        &conn,
        &id,
        None,
        Some("patched body"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(1),
    )
    .expect("update must succeed with matching expected_version");
    assert!(found);
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(stored.version, 2, "version bumped after successful update");
    assert_eq!(stored.content, "patched body");
}

#[test]
fn expected_version_mismatch_returns_conflict_envelope() {
    let conn = open_test_db();
    let mem = make_memory("mismatch-gate");
    let id = db::insert(&conn, &mem).expect("insert");
    // First caller wins with expected_version=1.
    db::update_with_expected_version(
        &conn,
        &id,
        None,
        Some("winner write"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(1),
    )
    .expect("first update must succeed");
    // Second caller still believes the stored version is 1 (the
    // value they read before the winner's write landed). The gate
    // must refuse with a typed VersionConflict carrying both
    // expected + current so the caller knows exactly how far they
    // are behind.
    let err = db::update_with_expected_version(
        &conn,
        &id,
        None,
        Some("loser write"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(1),
    )
    .expect_err("second update must fail with VersionConflict");
    let vc = err
        .downcast_ref::<VersionConflict>()
        .expect("typed VersionConflict envelope");
    assert_eq!(vc.id, id);
    assert_eq!(vc.expected, 1);
    assert_eq!(vc.current, 2);
    // Stored content must reflect the WINNER's write, not the
    // loser's payload. Last-write-wins is exactly what this gate
    // prevents.
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(stored.content, "winner write");
    assert_eq!(stored.version, 2);
}

#[test]
fn two_concurrent_updates_produce_exactly_one_winner() {
    let conn = open_test_db();
    let mem = make_memory("two-callers");
    let id = db::insert(&conn, &mem).expect("insert");

    // Both callers read the SAME baseline version=1.
    let baseline = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(baseline.version, 1);

    // Caller A writes with expected_version=1 — wins.
    let outcome_a = db::update_with_expected_version(
        &conn,
        &id,
        None,
        Some("body from A"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(baseline.version),
    );
    // Caller B writes with the SAME expected_version=1 — loses.
    let outcome_b = db::update_with_expected_version(
        &conn,
        &id,
        None,
        Some("body from B"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(baseline.version),
    );

    let a_ok = outcome_a.is_ok();
    let b_ok = outcome_b.is_ok();
    assert!(
        a_ok ^ b_ok,
        "exactly one writer must win: a_ok={a_ok} b_ok={b_ok}"
    );

    // The loser's error must be a typed VersionConflict (not a
    // generic SQL error / silent overwrite).
    let loser_err = if a_ok { outcome_b } else { outcome_a };
    let err = loser_err.expect_err("loser must surface error");
    let vc = err
        .downcast_ref::<VersionConflict>()
        .expect("typed VersionConflict on the loser");
    assert_eq!(vc.expected, 1);
    assert_eq!(vc.current, 2);

    let final_row = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(final_row.version, 2);
    // Final body matches the winner exactly.
    let expected_body = if a_ok { "body from A" } else { "body from B" };
    assert_eq!(final_row.content, expected_body);
}

#[test]
fn no_expected_version_preserves_last_write_wins_legacy_contract() {
    // Pre-Gap-1 callers that never pass expected_version still get
    // the historical in-place mutation semantics — the gate is
    // strictly opt-in. Two updates back-to-back both succeed.
    let conn = open_test_db();
    let mem = make_memory("legacy");
    let id = db::insert(&conn, &mem).expect("insert");
    let (found_a, _) = db::update(
        &conn,
        &id,
        None,
        Some("first"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("legacy update 1");
    assert!(found_a);
    let (found_b, _) = db::update(
        &conn,
        &id,
        None,
        Some("second"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("legacy update 2");
    assert!(found_b);
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(stored.content, "second");
    // version still bumped twice — the counter exists on every row
    // regardless of whether the caller opts into the gate.
    assert_eq!(stored.version, 3);
}
