// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K7 — `memory_subscription_dlq_list` MCP-tool wiring test.
//!
//! K6 shipped the writer (`subscriptions::record_dlq` lands a row when
//! the [200ms, 1s, 5s] retry ladder is exhausted) but no inspector
//! tool. K7 ships the inspector — `memory_subscription_dlq_list` lives
//! in `Family::Power` and surfaces DLQ rows ordered by `id` ascending
//! so an operator scanning the DLQ sees the oldest unhandled failure
//! first.
//!
//! The test seeds three DLQ rows directly (avoiding the network +
//! retry-ladder latency) and pins:
//!   - the underlying `subscriptions::list_dlq` returns rows in
//!     insertion order (the contract `memory_subscription_dlq_list`
//!     wraps);
//!   - the `subscription_id` filter scopes the result correctly;
//!   - the omitted-filter case returns the full set.

use ai_memory::profile::{Family, Profile};
use ai_memory::subscriptions::{self, NewSubscription};
use rusqlite::Connection;

mod common;
use common::fresh_db_tempfile_path as fresh_db;

fn subscribe(db_path: &std::path::Path, url: &str) -> String {
    let conn = Connection::open(db_path).unwrap();
    subscriptions::insert(
        &conn,
        &NewSubscription {
            url,
            events: "*",
            secret: None,
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("k7-dlq-test"),
            event_types: None,
        },
    )
    .expect("insert subscription")
}

#[test]
fn k7_dlq_list_tool_registered_under_power_family() {
    assert_eq!(
        Family::for_tool("memory_subscription_dlq_list"),
        Some(Family::Power),
        "memory_subscription_dlq_list must live in Family::Power"
    );
    assert!(Profile::full().loads("memory_subscription_dlq_list"));
    assert!(Profile::power().loads("memory_subscription_dlq_list"));
    assert!(!Profile::core().loads("memory_subscription_dlq_list"));
}

#[test]
fn k7_dlq_list_returns_three_rows_ordered_by_insertion() {
    let (_keep, db_path) = fresh_db();
    let sub_a = subscribe(&db_path, "https://example.invalid/a");
    let sub_b = subscribe(&db_path, "https://example.invalid/b");

    // Seed three DLQ rows: two for sub_a, one for sub_b. The K6 writer
    // signature pins the column ordering (correlation_id, event_type,
    // payload, retry_count, last_error, first_failed_at, last_failed_at).
    let series = [
        (
            sub_a.as_str(),
            "corr-a-001",
            "memory_store",
            r#"{"id":"a-001"}"#,
            4i64,
            "http-500",
        ),
        (
            sub_a.as_str(),
            "corr-a-002",
            "memory_promote",
            r#"{"id":"a-002"}"#,
            4,
            "ack-corr-mismatch",
        ),
        (
            sub_b.as_str(),
            "corr-b-001",
            "memory_delete",
            r#"{"id":"b-001"}"#,
            4,
            "network: timeout",
        ),
    ];
    for (sub_id, corr, event, payload, retries, err) in &series {
        subscriptions::record_dlq(
            &db_path,
            sub_id,
            corr,
            event,
            payload,
            *retries,
            err,
            "2026-04-01T00:00:00Z",
            "2026-04-01T00:00:06Z",
        )
        .expect("seed DLQ row");
    }

    // Unfiltered: all three rows back in insertion order.
    let rows = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::list_dlq(&conn, None).expect("list_dlq")
    };
    assert_eq!(rows.len(), 3);
    let observed: Vec<&str> = rows.iter().map(|r| r.correlation_id.as_str()).collect();
    assert_eq!(observed, vec!["corr-a-001", "corr-a-002", "corr-b-001"]);

    // Filtered to sub_a: two rows, both belonging to sub_a, in
    // insertion order.
    let rows_a = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::list_dlq(&conn, Some(&sub_a)).expect("list_dlq sub_a")
    };
    assert_eq!(rows_a.len(), 2);
    for r in &rows_a {
        assert_eq!(r.subscription_id, sub_a);
    }
    let order_a: Vec<&str> = rows_a.iter().map(|r| r.correlation_id.as_str()).collect();
    assert_eq!(order_a, vec!["corr-a-001", "corr-a-002"]);

    // Filtered to sub_b: one row.
    let rows_b = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::list_dlq(&conn, Some(&sub_b)).expect("list_dlq sub_b")
    };
    assert_eq!(rows_b.len(), 1);
    assert_eq!(rows_b[0].correlation_id, "corr-b-001");
    assert_eq!(rows_b[0].retry_count, 4);
    assert_eq!(rows_b[0].last_error, "network: timeout");
}

#[test]
fn k7_dlq_list_empty_when_no_rows_seeded() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();
    let rows = subscriptions::list_dlq(&conn, None).expect("list_dlq empty");
    assert!(rows.is_empty(), "fresh DB must have zero DLQ rows");
}
