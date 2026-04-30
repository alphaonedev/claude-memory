// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! P5 (G9) — webhook lifecycle event coverage.
//!
//! v0.6.0.0 only fired webhooks on `memory_store`. P5 wires
//! `dispatch_event_with_details` into the four other lifecycle paths —
//! promote, delete, link, consolidate — and gates each subscriber on
//! an optional structured `event_types` opt-in list. These tests use
//! wiremock to stand up an in-process HTTP listener, register a
//! subscription pointing at it, drive the dispatcher with the same
//! payload shape the production handlers use, and assert the right
//! events land at the right URLs.
//!
//! The handler-internal call sites (handle_promote / handle_delete /
//! handle_link / handle_consolidate) are private so we exercise the
//! `subscriptions::dispatch_event_with_details` entry point directly
//! with the *exact* event-name + namespace + agent_id shape each
//! handler now uses (see src/mcp.rs in the corresponding handler).
//! That keeps the test tightly scoped to the coverage gap G9 closes
//! without re-spinning the full MCP stdio harness.
//!
//! Acceptance (per REMEDIATIONv0631 §P5):
//!   - webhook_fires_on_promote
//!   - webhook_fires_on_delete
//!   - webhook_fires_on_link_created
//!   - webhook_fires_on_consolidate
//!   - subscriber_filtered_to_store_does_not_get_delete

use ai_memory::subscriptions::{
    self, ConsolidatedEventDetails, DeleteEventDetails, LinkCreatedEventDetails, NewSubscription,
    PromoteEventDetails,
};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::NamedTempFile;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Stand up a fresh on-disk SQLite at a tempfile path with the
/// production schema applied (incl. P5 migration v17).
fn fresh_db() -> (NamedTempFile, PathBuf) {
    let f = NamedTempFile::new().expect("tempfile");
    let p = f.path().to_path_buf();
    let _ = ai_memory::db::open(&p).expect("db::open");
    (f, p)
}

/// Insert a wildcard subscription pointing at `mock_url`. Returns the
/// new subscription id.
fn subscribe_all(db_path: &std::path::Path, mock_url: &str) -> String {
    let conn = Connection::open(db_path).expect("open db");
    subscriptions::insert(
        &conn,
        &NewSubscription {
            url: mock_url,
            events: "*",
            secret: Some("p5-test-secret"),
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("p5-test"),
            event_types: None,
        },
    )
    .expect("insert subscription")
}

/// Insert a subscription with a structured `event_types` opt-in list.
fn subscribe_event_types(
    db_path: &std::path::Path,
    mock_url: &str,
    event_types: &[String],
) -> String {
    let conn = Connection::open(db_path).expect("open db");
    subscriptions::insert(
        &conn,
        &NewSubscription {
            url: mock_url,
            events: "*",
            secret: None,
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("p5-test"),
            event_types: Some(event_types),
        },
    )
    .expect("insert subscription")
}

/// Wait up to ~5 s for the wiremock server to receive at least one
/// request matching `event_name` in its JSON body.
async fn wait_for_event(server: &MockServer, event_name: &str) -> Option<Request> {
    for _ in 0..50 {
        let received = server.received_requests().await.unwrap_or_default();
        for req in received {
            if let Ok(body_str) = std::str::from_utf8(&req.body)
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(body_str)
                && val["event"].as_str() == Some(event_name)
            {
                return Some(req);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Wait until at least `n` requests have landed at the mock, then
/// return the parsed JSON bodies.
async fn collect_event_bodies(server: &MockServer, n: usize) -> Vec<serde_json::Value> {
    for _ in 0..50 {
        let received = server.received_requests().await.unwrap_or_default();
        if received.len() >= n {
            return received
                .into_iter()
                .filter_map(|r| {
                    std::str::from_utf8(&r.body)
                        .ok()
                        .and_then(|s| serde_json::from_str(s).ok())
                })
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let received = server.received_requests().await.unwrap_or_default();
    received
        .into_iter()
        .filter_map(|r| {
            std::str::from_utf8(&r.body)
                .ok()
                .and_then(|s| serde_json::from_str(s).ok())
        })
        .collect()
}

// ---------------------------------------------------------------------
// 1. Promote — vertical AND tier modes both emit memory_promote.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn webhook_fires_on_promote() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (_keep, db_path) = fresh_db();
    let url = format!("{}/hook", server.uri());
    let _sub_id = subscribe_all(&db_path, &url);

    // Mirror the handle_promote (tier mode) call shape.
    let details = serde_json::to_value(PromoteEventDetails {
        mode: "tier".to_string(),
        tier: Some("long".to_string()),
        to_namespace: None,
        clone_id: None,
    })
    .ok();
    {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::dispatch_event_with_details(
            &conn,
            "memory_promote",
            "memory-under-test",
            "ns-promote",
            Some("agent-X"),
            &db_path,
            details,
        );
    }

    let req = wait_for_event(&server, "memory_promote")
        .await
        .expect("memory_promote webhook should reach mock");

    let body: serde_json::Value =
        serde_json::from_slice(&req.body).expect("dispatch body must be JSON");
    assert_eq!(body["event"], "memory_promote");
    assert_eq!(body["memory_id"], "memory-under-test");
    assert_eq!(body["namespace"], "ns-promote");
    assert_eq!(body["agent_id"], "agent-X");
    // Details flattened into the envelope.
    assert_eq!(body["mode"], "tier");
    assert_eq!(body["tier"], "long");
    // Signature header always set when subscriber has a secret.
    assert!(req.headers.get("x-ai-memory-signature").is_some());
    assert!(req.headers.get("x-ai-memory-timestamp").is_some());
}

// ---------------------------------------------------------------------
// 2. Delete — emits memory_delete with the pre-delete title + tier.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn webhook_fires_on_delete() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (_keep, db_path) = fresh_db();
    let url = format!("{}/hook", server.uri());
    let _sub_id = subscribe_all(&db_path, &url);

    let details = serde_json::to_value(DeleteEventDetails {
        title: "deleted-title".to_string(),
        tier: "mid".to_string(),
    })
    .ok();
    {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::dispatch_event_with_details(
            &conn,
            "memory_delete",
            "deleted-id",
            "ns-delete",
            Some("agent-deleter"),
            &db_path,
            details,
        );
    }

    let req = wait_for_event(&server, "memory_delete")
        .await
        .expect("memory_delete webhook should reach mock");
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["event"], "memory_delete");
    assert_eq!(body["memory_id"], "deleted-id");
    assert_eq!(body["namespace"], "ns-delete");
    assert_eq!(body["title"], "deleted-title");
    assert_eq!(body["tier"], "mid");
}

// ---------------------------------------------------------------------
// 3. Link — emits memory_link_created with target + relation.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn webhook_fires_on_link_created() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (_keep, db_path) = fresh_db();
    let url = format!("{}/hook", server.uri());
    let _sub_id = subscribe_all(&db_path, &url);

    let details = serde_json::to_value(LinkCreatedEventDetails {
        target_id: "target-mem".to_string(),
        relation: "supersedes".to_string(),
    })
    .ok();
    {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::dispatch_event_with_details(
            &conn,
            "memory_link_created",
            "source-mem",
            "ns-link",
            Some("agent-linker"),
            &db_path,
            details,
        );
    }

    let req = wait_for_event(&server, "memory_link_created")
        .await
        .expect("memory_link_created webhook should reach mock");
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["event"], "memory_link_created");
    assert_eq!(body["memory_id"], "source-mem");
    assert_eq!(body["target_id"], "target-mem");
    assert_eq!(body["relation"], "supersedes");
}

// ---------------------------------------------------------------------
// 4. Consolidate — emits memory_consolidated with source ids + count.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn webhook_fires_on_consolidate() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (_keep, db_path) = fresh_db();
    let url = format!("{}/hook", server.uri());
    let _sub_id = subscribe_all(&db_path, &url);

    let source_ids = vec![
        "src-a".to_string(),
        "src-b".to_string(),
        "src-c".to_string(),
    ];
    let details = serde_json::to_value(ConsolidatedEventDetails {
        source_ids: source_ids.clone(),
        source_count: source_ids.len(),
    })
    .ok();
    {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::dispatch_event_with_details(
            &conn,
            "memory_consolidated",
            "new-consolidated-id",
            "ns-consolidate",
            Some("agent-consolidator"),
            &db_path,
            details,
        );
    }

    let req = wait_for_event(&server, "memory_consolidated")
        .await
        .expect("memory_consolidated webhook should reach mock");
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["event"], "memory_consolidated");
    assert_eq!(body["memory_id"], "new-consolidated-id");
    assert_eq!(body["namespace"], "ns-consolidate");
    assert_eq!(body["source_count"], 3);
    let got_sources = body["source_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(got_sources, source_ids);
}

// ---------------------------------------------------------------------
// 5. Per-event-type filter — narrow subscriber misses out-of-scope
//    events but receives the ones it opted into.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn subscriber_filtered_to_store_does_not_get_delete() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (_keep, db_path) = fresh_db();
    let url = format!("{}/hook", server.uri());

    // Opt-in to memory_store ONLY.
    let _sub_id = subscribe_event_types(&db_path, &url, &["memory_store".to_string()]);

    {
        let conn = Connection::open(&db_path).unwrap();
        // Fire a store event — should be delivered.
        subscriptions::dispatch_event_with_details(
            &conn,
            "memory_store",
            "stored-mem",
            "ns",
            Some("agent"),
            &db_path,
            None,
        );
        // Fire a delete event — must be filtered out.
        let delete_details = serde_json::to_value(DeleteEventDetails {
            title: "should-not-fire".to_string(),
            tier: "long".to_string(),
        })
        .ok();
        subscriptions::dispatch_event_with_details(
            &conn,
            "memory_delete",
            "deleted-mem",
            "ns",
            Some("agent"),
            &db_path,
            delete_details,
        );
    }

    // Wait for the store event to land. The delete must NOT show up.
    let _ = wait_for_event(&server, "memory_store")
        .await
        .expect("store event must reach narrow subscriber");

    // Give any delete dispatch a chance to fire (it shouldn't), then
    // check that no delete event landed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bodies = collect_event_bodies(&server, 1).await;
    let delete_count = bodies
        .iter()
        .filter(|b| b["event"].as_str() == Some("memory_delete"))
        .count();
    assert_eq!(
        delete_count, 0,
        "subscriber filtered to memory_store must NOT receive memory_delete"
    );
    let store_count = bodies
        .iter()
        .filter(|b| b["event"].as_str() == Some("memory_store"))
        .count();
    assert!(
        store_count >= 1,
        "subscriber filtered to memory_store SHOULD receive memory_store"
    );
}

// ---------------------------------------------------------------------
// 6. End-to-end via list_by_event — all-events default + opt-in
//    subscribers both surface for memory_promote.
// ---------------------------------------------------------------------

#[test]
fn list_by_event_returns_default_and_matching_optin() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    // Default subscriber (event_types = None ⇒ matches everything).
    let id_default = subscriptions::insert(
        &conn,
        &NewSubscription {
            url: "https://example.com/all",
            events: "*",
            secret: None,
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("default"),
            event_types: None,
        },
    )
    .unwrap();

    // Narrow opt-in for memory_promote only.
    let promote_only = vec!["memory_promote".to_string()];
    let id_narrow = subscriptions::insert(
        &conn,
        &NewSubscription {
            url: "https://example.com/narrow",
            events: "*",
            secret: None,
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("narrow"),
            event_types: Some(&promote_only),
        },
    )
    .unwrap();

    // Narrow opt-in for memory_delete only — must NOT match
    // memory_promote.
    let delete_only = vec!["memory_delete".to_string()];
    let _id_other = subscriptions::insert(
        &conn,
        &NewSubscription {
            url: "https://example.com/other",
            events: "*",
            secret: None,
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("other"),
            event_types: Some(&delete_only),
        },
    )
    .unwrap();

    let matched = subscriptions::list_by_event(&conn, "memory_promote").unwrap();
    let matched_ids: Vec<&str> = matched.iter().map(|s| s.id.as_str()).collect();
    assert!(
        matched_ids.contains(&id_default.as_str()),
        "default-events subscriber must surface for any event"
    );
    assert!(
        matched_ids.contains(&id_narrow.as_str()),
        "promote-only subscriber must surface for memory_promote"
    );
    assert_eq!(
        matched.len(),
        2,
        "delete-only subscriber must NOT surface for memory_promote"
    );
}

// ---------------------------------------------------------------------
// 7. Insert rejects unknown event types (defensive: catch typos at
//    subscribe time rather than silently never firing).
// ---------------------------------------------------------------------

#[test]
fn insert_rejects_unknown_event_type() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();
    let bad = vec!["memory_typo".to_string()];
    let res = subscriptions::insert(
        &conn,
        &NewSubscription {
            url: "https://example.com/hook",
            events: "*",
            secret: None,
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("test"),
            event_types: Some(&bad),
        },
    );
    assert!(res.is_err(), "unknown event type must be rejected");
    let err = res.err().unwrap().to_string();
    assert!(err.contains("unknown webhook event type"), "got {err}");
}
