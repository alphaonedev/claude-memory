// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K7 — `memory_subscription_replay` MCP-tool wiring test.
//!
//! K6 implemented the handler in `src/subscriptions.rs` but deferred the
//! MCP registration (and dispatch wiring) to K7 to avoid colliding with
//! the v0.7 B1 tool-count cascade. K7 ships the registration; this
//! integration test pins the wired surface end-to-end:
//!
//! 1. `memory_subscription_replay` appears in `tool_definitions()` under
//!    the full profile (and is reachable for `--profile power`).
//! 2. The handler exposed at `crate::subscriptions::memory_subscription_replay`
//!    returns a stable, ordered envelope when fed a synthetic event
//!    series (`delivered_at` ascending, plus a `count` field equal to
//!    `events.len()`).
//!
//! The test deliberately avoids spinning the full MCP stdio harness —
//! the smoke matrix in `src/mcp.rs::mcp_tools_smoke_matrix` already
//! covers JSON-RPC dispatch parity. Here we exercise the replay
//! semantics that operators rely on for incident reconstruction.

use ai_memory::profile::{Family, Profile};
use ai_memory::subscriptions::{self, NewSubscription};
use rusqlite::Connection;
use tempfile::NamedTempFile;

/// Stand up a fresh on-disk `SQLite` at a tempfile path with the
/// production schema applied (incl. K6 `subscription_dlq` /
/// `subscription_events` migrations).
fn fresh_db() -> (NamedTempFile, std::path::PathBuf) {
    let f = NamedTempFile::new().expect("tempfile");
    let p = f.path().to_path_buf();
    let _ = ai_memory::db::open(&p).expect("db::open");
    (f, p)
}

#[test]
fn k7_replay_tool_registered_under_power_family() {
    // K7: the tool must resolve to Family::Power so `--profile power`
    // surfaces it. Source-anchored at src/profile.rs::Family::for_tool.
    assert_eq!(
        Family::for_tool("memory_subscription_replay"),
        Some(Family::Power),
        "memory_subscription_replay must live in Family::Power"
    );

    // Full profile loads it; core does not (it's an operator tool, not
    // a data-plane tool).
    assert!(Profile::full().loads("memory_subscription_replay"));
    assert!(Profile::power().loads("memory_subscription_replay"));
    assert!(!Profile::core().loads("memory_subscription_replay"));
}

#[test]
fn k7_replay_returns_ordered_envelope_for_synthetic_event_series() {
    let (_keep, db_path) = fresh_db();

    // 1. Register one subscription so we have a stable subscription_id
    //    to scope the replay against. URL doesn't need to resolve —
    //    we never dispatch, only seed the audit table directly.
    let sub_id = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::insert(
            &conn,
            &NewSubscription {
                url: "https://example.invalid/k7-replay",
                events: "*",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: Some("k7-replay-test"),
                event_types: None,
            },
        )
        .expect("insert subscription")
    };

    // 2. Seed three audit rows with strictly-increasing delivered_at
    //    timestamps so we can assert the replay order is stable.
    let series = [
        ("corr-001", "memory_store", "2026-04-01T00:00:00Z"),
        ("corr-002", "memory_promote", "2026-04-02T00:00:00Z"),
        ("corr-003", "memory_delete", "2026-04-03T00:00:00Z"),
    ];
    {
        let conn = Connection::open(&db_path).unwrap();
        for (corr, event, _) in &series {
            // record_subscription_event stamps `delivered_at = Utc::now`
            // internally — we override that below to get the deterministic
            // timestamps the ordering assertion needs.
            subscriptions::record_subscription_event(
                &db_path,
                &sub_id,
                corr,
                event,
                "{\"k7-replay\":true}",
            )
            .expect("seed subscription_events row");
            // Force the timestamp so the test is deterministic.
            conn.execute(
                "UPDATE subscription_events SET delivered_at = ?1 WHERE correlation_id = ?2",
                rusqlite::params![series.iter().find(|s| s.0 == *corr).unwrap().2, corr],
            )
            .unwrap();
        }
    }

    // 3. Replay everything since the epoch and pin the envelope shape.
    let envelope = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::memory_subscription_replay(&conn, &sub_id, "1970-01-01T00:00:00Z")
            .expect("replay should succeed")
    };

    // Envelope: { subscription_id, since, count, events: [...] }
    assert_eq!(envelope["subscription_id"].as_str(), Some(sub_id.as_str()));
    assert_eq!(envelope["since"].as_str(), Some("1970-01-01T00:00:00Z"));
    assert_eq!(envelope["count"].as_u64(), Some(3));

    let events = envelope["events"].as_array().expect("events array");
    assert_eq!(events.len(), 3);

    // Order is delivered_at ASC — same as the seed order.
    let observed: Vec<&str> = events
        .iter()
        .map(|e| e["correlation_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(observed, vec!["corr-001", "corr-002", "corr-003"]);

    // Replay with a `since` cutoff that excludes the first row.
    let envelope_partial = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::memory_subscription_replay(&conn, &sub_id, "2026-04-02T00:00:00Z")
            .expect("partial replay should succeed")
    };
    assert_eq!(envelope_partial["count"].as_u64(), Some(2));
    let partial: Vec<&str> = envelope_partial["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["correlation_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(partial, vec!["corr-002", "corr-003"]);
}

#[test]
fn k7_replay_unknown_subscription_returns_empty_envelope() {
    // Operator hits replay for a subscription that has no audit rows
    // (or never existed). The handler returns count=0 with an empty
    // events array — the operator-facing distinction between
    // "no events yet" and "unknown subscription" is recoverable from
    // the parallel `memory_list_subscriptions` view.
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();
    let envelope =
        subscriptions::memory_subscription_replay(&conn, "does-not-exist", "1970-01-01T00:00:00Z")
            .expect("replay must not fail on unknown subscription");
    assert_eq!(envelope["count"].as_u64(), Some(0));
    assert!(envelope["events"].as_array().unwrap().is_empty());
}
