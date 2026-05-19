// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Cross-tenant subscription authorization regression suite (issues
//! #870, #872, #874).
//!
//! Background (security-review, 2026-05-18). The subscription wire
//! surface had three cross-tenant authorization gaps:
//!
//! - **#870** (security-high). `memory_unsubscribe` deleted by id with
//!   no ownership check, so any registered agent could remove any
//!   other agent's webhook by id-guessing or by exfiltrating the id
//!   from a leaked list response.
//! - **#872** (security-high). `memory_list_subscriptions` (MCP) and
//!   `GET /api/v1/subscriptions` (HTTP) returned every tenant's rows
//!   in a single response, leaking the entire webhook fleet of every
//!   other tenant.
//! - **#874** (security-medium). The HTTP handler trusted the
//!   caller-supplied `?agent_id=<id>` query parameter as the
//!   authentication source — so a peer with `X-Agent-Id: alice` could
//!   simply pass `?agent_id=bob` and read bob's subscriptions.
//!
//! Fix: `subscriptions::delete` / `subscriptions::list` grew an
//! `Option<&str>` owner argument. When `Some`, the underlying SQL is
//! scoped by `created_by = ?`. The MCP and HTTP handlers always pass
//! `Some(<authenticated caller>)`. The HTTP handler also resolves the
//! caller through the **header** only (X-Agent-Id) — the query
//! parameter is degraded to a refinement that must match the header,
//! else 403.
//!
//! Regression cases (one per finding):
//!
//! 1. `subscribe_as_a_then_unsubscribe_as_b_fails_870` — alice
//!    subscribes; bob attempts to delete by id; the row is NOT
//!    removed.
//! 2. `list_as_a_returns_only_a_rows_872` — alice and bob each
//!    subscribe; alice's `list` call returns exactly her own row.
//! 3. `http_query_param_spoofing_rejected_874` — header is alice,
//!    query is `agent_id=bob` → 403 from the HTTP listing and
//!    unsubscribe handlers.

use ai_memory::subscriptions::{self, NewSubscription};
use rusqlite::Connection;
use tempfile::NamedTempFile;

mod common;
use common::fresh_db_tempfile_path as fresh_db;

fn fresh_conn() -> (NamedTempFile, Connection) {
    let (keep, path) = fresh_db();
    let conn = Connection::open(&path).expect("open db");
    (keep, conn)
}

fn insert_owned(conn: &Connection, url: &str, owner: &str) -> String {
    subscriptions::insert(
        conn,
        &NewSubscription {
            url,
            events: "*",
            secret: Some("test-secret"),
            namespace_filter: None,
            agent_filter: None,
            created_by: Some(owner),
            event_types: None,
        },
    )
    .expect("insert subscription")
}

// ---------------------------------------------------------------------------
// #870 — `subscriptions::delete(_, _, Some(<caller>))` must refuse to
// remove a row whose `created_by` is a different tenant.
// ---------------------------------------------------------------------------

#[test]
fn subscribe_as_a_then_unsubscribe_as_b_fails_870() {
    let (_keep, conn) = fresh_conn();
    // alice owns the subscription.
    let alice_sub = insert_owned(&conn, "https://example.com/alice", "alice");

    // bob tries to delete alice's row by id. The owner-scoped DELETE
    // must skip it.
    let removed_by_bob = subscriptions::delete(&conn, &alice_sub, Some("bob"))
        .expect("delete returns Ok even on a no-op");
    assert!(
        !removed_by_bob,
        "#870: bob must not be able to remove alice's subscription"
    );

    // Sanity-check: the row is still present (full-scan list).
    let all = subscriptions::list(&conn, None).expect("list");
    assert_eq!(all.len(), 1, "row must survive bob's deletion attempt");
    assert_eq!(all[0].id, alice_sub);

    // alice can still remove her own row.
    let removed_by_alice =
        subscriptions::delete(&conn, &alice_sub, Some("alice")).expect("alice delete ok");
    assert!(
        removed_by_alice,
        "alice must be able to remove her own subscription"
    );
}

// ---------------------------------------------------------------------------
// #872 — `subscriptions::list(_, Some(<caller>))` must only return rows
// the caller owns.
// ---------------------------------------------------------------------------

#[test]
fn list_as_a_returns_only_a_rows_872() {
    let (_keep, conn) = fresh_conn();
    let alice_sub = insert_owned(&conn, "https://example.com/alice", "alice");
    let _bob_sub = insert_owned(&conn, "https://example.com/bob", "bob");

    let alice_view = subscriptions::list(&conn, Some("alice")).expect("alice list");
    assert_eq!(
        alice_view.len(),
        1,
        "#872: alice must only see her own row, got: {:?}",
        alice_view.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
    assert_eq!(alice_view[0].id, alice_sub);
    assert_eq!(alice_view[0].created_by.as_deref(), Some("alice"));

    let bob_view = subscriptions::list(&conn, Some("bob")).expect("bob list");
    assert_eq!(
        bob_view.len(),
        1,
        "#872: bob must only see his own row, got: {:?}",
        bob_view.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
    assert_eq!(bob_view[0].created_by.as_deref(), Some("bob"));

    // Operator path (no caller scope) sees both — dispatch fan-out and
    // operator inventory depend on this.
    let global = subscriptions::list(&conn, None).expect("global list");
    assert_eq!(global.len(), 2);
}

// ---------------------------------------------------------------------------
// #874 — HTTP layer: spoofing `?agent_id=` query param must NOT
// authenticate the caller. Header (X-Agent-Id) is the only trusted
// authentication source.
// ---------------------------------------------------------------------------
//
// We exercise the HTTP handlers via the in-process axum router fixture
// rather than reaching into the handler functions directly so the
// extractor wiring (Query, HeaderMap) is part of the regression
// coverage.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};

fn build_router_fixture() -> (axum::Router, NamedTempFile) {
    let f = NamedTempFile::new().expect("tempfile");
    let db_path = f.path().to_path_buf();
    let _ = ai_memory::db::open(&db_path).expect("db::open");
    let conn = ai_memory::db::open(&db_path).expect("reopen for AppState");
    let db: Db = Arc::new(Mutex::new((
        conn,
        db_path.clone(),
        ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: Arc<dyn ai_memory::store::MemoryStore> =
        Arc::new(ai_memory::store::sqlite::SqliteStore::open(&db_path).expect("open SqliteStore"));
    let app_state = AppState {
        db,
        embedder: Arc::new(None),
        vector_index: Arc::new(Mutex::new(None)),
        federation: Arc::new(None),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(None),
        family_embeddings: Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: Arc::new(ai_memory::identity::replay::ReplayCache::default()),
        verify_require_nonce: false,
        federation_nonce_cache: std::sync::Arc::new(
            ai_memory::identity::replay::FederationNonceCache::default(),
        ),
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    };
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, f)
}

async fn get_with_headers(
    router: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Body::empty()).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

async fn delete_with_headers(
    router: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method("DELETE").uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Body::empty()).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

#[tokio::test]
async fn http_query_param_spoofing_rejected_874() {
    let (router, _f) = build_router_fixture();

    // GET — header says alice, query says bob → must be 403.
    let (status, body) = get_with_headers(
        &router,
        "/api/v1/subscriptions?agent_id=bob",
        &[("x-agent-id", "alice")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "#874: query agent_id spoof on listing must be 403, got {status} body={body}"
    );

    // DELETE — same mismatch shape on the unsubscribe surface.
    let (status, body) = delete_with_headers(
        &router,
        "/api/v1/subscriptions?id=some-id&agent_id=bob",
        &[("x-agent-id", "alice")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "#874: query agent_id spoof on unsubscribe must be 403, got {status} body={body}"
    );

    // Sanity: when the query param MATCHES the authenticated header,
    // the request proceeds (empty DB, so 200 with no rows / OK with
    // removed=false).
    let (status, _body) = get_with_headers(
        &router,
        "/api/v1/subscriptions?agent_id=alice",
        &[("x-agent-id", "alice")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "matching query+header must proceed (no 403)"
    );
}
