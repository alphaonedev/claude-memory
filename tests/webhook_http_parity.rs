// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.6.4-017 — G9 HTTP webhook parity.
//!
//! v0.6.3.1 P5 wired four lifecycle webhooks (`memory_delete`,
//! `memory_promote`, `memory_link_created`, `memory_consolidated`) into
//! the **MCP path** at `mcp.rs:2227,2327,2372,2569,2723`. The HTTP
//! handlers were left silent — `grep "dispatch_event" src/handlers.rs`
//! returned zero matches in v0.6.3.1. This file is the source-anchored
//! finding turned into acceptance tests:
//!
//! 1. `webhook_fires_on_http_delete`
//! 2. `webhook_fires_on_http_promote`
//! 3. `webhook_fires_on_http_link_created`
//! 4. `webhook_fires_on_http_consolidate`
//!
//! Each test drives a live wiremock subscriber via the production
//! `build_router` + `Router::oneshot` harness so the assertion runs
//! against the real handler code, not a mocked dispatch.
//!
//! ## Why on-disk SQLite (not `:memory:`)
//!
//! `dispatch_event_with_details` spawns a worker thread for each
//! subscriber that re-opens the database at the recorded `db_path` so
//! it can update `dispatch_count` / `failure_count` on the
//! `subscriptions` row without holding the foreground caller's
//! connection. `:memory:` databases are connection-private — the
//! spawned thread would be unable to find any subscriptions and the
//! tests would silently pass without exercising the HTTP path. We use
//! a `NamedTempFile` so the spawned dispatch thread shares the same
//! WAL-mode SQLite file the foreground request used.
//!
//! ## Why a tiny `tokio::time::sleep`
//!
//! The dispatch thread is `std::thread::spawn`-detached; the request
//! returns before the wiremock receives the POST. We poll the
//! `MockServer::received_requests` list with a short backoff rather
//! than picking an arbitrary fixed sleep — keeps the suite fast while
//! tolerating slow CI.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db};
use ai_memory::subscriptions::{self, NewSubscription};

// --------------------------------------------------------------------
// Test harness
// --------------------------------------------------------------------

struct HttpHarness {
    router: axum::Router,
    db_path: std::path::PathBuf,
    _tempfile: NamedTempFile,
}

impl HttpHarness {
    fn new() -> Self {
        let f = NamedTempFile::new().expect("tempfile");
        let db_path = f.path().to_path_buf();
        // Open + run all migrations.
        let _ = ai_memory::db::open(&db_path).expect("db::open");

        // Build the in-process router exactly the way `serve()` does.
        let conn = ai_memory::db::open(&db_path).expect("reopen for AppState");
        let db: Db = Arc::new(Mutex::new((
            conn,
            db_path.clone(),
            ResolvedTtl::default(),
            true,
        )));
        let app_state = AppState {
            db,
            embedder: Arc::new(None),
            vector_index: Arc::new(Mutex::new(None)),
            federation: Arc::new(None),
            tier_config: Arc::new(FeatureTier::Keyword.config()),
            scoring: Arc::new(ResolvedScoring::default()),
            profile: Arc::new(ai_memory::profile::Profile::core()),
            mcp_config: Arc::new(None),
        };
        let api_key_state = ApiKeyState { key: None };
        let router = ai_memory::build_router(api_key_state, app_state);

        Self {
            router,
            db_path,
            _tempfile: f,
        }
    }

    async fn json_post(&self, p: &str, body: Value) -> (StatusCode, Value) {
        self.req("POST", p, Some(body)).await
    }

    async fn json_delete(&self, p: &str) -> (StatusCode, Value) {
        self.req("DELETE", p, None).await
    }

    async fn req(&self, method_str: &str, p: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method_str).uri(p);
        let req = if let Some(b) = body {
            builder = builder.header("content-type", "application/json");
            builder
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap()
        } else {
            builder.body(Body::empty()).unwrap()
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }
}

/// Insert a wildcard subscription pointing at the wiremock URL via a
/// fresh DB connection (the dispatch thread will reopen anyway).
fn subscribe_all(db_path: &Path, mock_url: &str) -> String {
    let conn = Connection::open(db_path).expect("open db");
    subscriptions::insert(
        &conn,
        &NewSubscription {
            url: mock_url,
            events: "*",
            secret: Some("v064-017-test"),
            namespace_filter: None,
            agent_filter: None,
            created_by: Some("v0.6.4-017"),
            event_types: None,
        },
    )
    .expect("insert subscription")
}

/// Wait up to `total` for the mock server to receive at least one
/// request, polling every 25 ms. Returns the captured requests.
async fn wait_for_dispatch(mock: &MockServer, total: Duration) -> Vec<wiremock::Request> {
    let deadline = std::time::Instant::now() + total;
    loop {
        let received = mock.received_requests().await.unwrap_or_default();
        if !received.is_empty() {
            return received;
        }
        if std::time::Instant::now() >= deadline {
            return received;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Stand up a wiremock that always returns 200 OK on POST `/hook`.
async fn fresh_mock() -> (MockServer, String) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    let url = format!("{}/hook", mock.uri());
    (mock, url)
}

/// Insert a memory directly via the DB layer (skipping the HTTP create
/// path so the test isolates the lifecycle event under test). Returns
/// the inserted memory's id.
fn seed_memory(db_path: &Path, title: &str, namespace: &str) -> String {
    let conn = Connection::open(db_path).expect("open db");
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories (id, title, content, tier, namespace, tags, priority, \
         confidence, source, metadata, created_at, updated_at, access_count) \
         VALUES (?1, ?2, ?3, 'mid', ?4, '[]', 5, 1.0, 'cli', '{}', ?5, ?5, 0)",
        rusqlite::params![id, title, "test content", namespace, now],
    )
    .expect("insert memory");
    id
}

// --------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------

#[tokio::test]
async fn webhook_fires_on_http_delete() {
    let harness = HttpHarness::new();
    let (mock, hook_url) = fresh_mock().await;
    let _sub_id = subscribe_all(&harness.db_path, &hook_url);

    let mem_id = seed_memory(&harness.db_path, "delete-target", "v064-017");
    let (status, _body) = harness
        .json_delete(&format!("/api/v1/memories/{mem_id}"))
        .await;
    assert_eq!(status, StatusCode::OK, "delete should succeed");

    let received = wait_for_dispatch(&mock, Duration::from_secs(2)).await;
    assert!(
        !received.is_empty(),
        "expected memory_delete webhook to fire on HTTP DELETE; \
         received nothing within 2s"
    );

    // Validate the event payload shape matches the MCP precedent.
    let body: Value = serde_json::from_slice(&received[0].body).expect("payload is valid JSON");
    assert_eq!(body["event"], "memory_delete");
    assert_eq!(body["memory_id"], mem_id);
    assert_eq!(body["namespace"], "v064-017");
    // DispatchPayload uses `#[serde(flatten)]` for details, so the
    // details fields land on the top-level envelope (not nested).
    assert_eq!(
        body["title"], "delete-target",
        "DeleteEventDetails.title must come from the pre-delete snapshot"
    );
    assert_eq!(body["tier"], "mid");
}

#[tokio::test]
async fn webhook_fires_on_http_promote() {
    let harness = HttpHarness::new();
    let (mock, hook_url) = fresh_mock().await;
    subscribe_all(&harness.db_path, &hook_url);

    let mem_id = seed_memory(&harness.db_path, "promote-target", "v064-017");
    let (status, _body) = harness
        .json_post(&format!("/api/v1/memories/{mem_id}/promote"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "promote should succeed");

    let received = wait_for_dispatch(&mock, Duration::from_secs(2)).await;
    assert!(
        !received.is_empty(),
        "expected memory_promote webhook to fire on HTTP promote"
    );

    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["event"], "memory_promote");
    assert_eq!(body["memory_id"], mem_id);
    // HTTP only does tier promotion. Vertical mode is MCP-only.
    // Details are flattened into the top-level envelope.
    assert_eq!(body["mode"], "tier");
    assert_eq!(body["tier"], "long");
}

#[tokio::test]
async fn webhook_fires_on_http_link_created() {
    let harness = HttpHarness::new();
    let (mock, hook_url) = fresh_mock().await;
    subscribe_all(&harness.db_path, &hook_url);

    let src_id = seed_memory(&harness.db_path, "link-source", "v064-017");
    let dst_id = seed_memory(&harness.db_path, "link-target", "v064-017");
    let (status, _body) = harness
        .json_post(
            "/api/v1/links",
            json!({
                "source_id": src_id,
                "target_id": dst_id,
                "relation": "related_to"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "link should be created");

    let received = wait_for_dispatch(&mock, Duration::from_secs(2)).await;
    assert!(
        !received.is_empty(),
        "expected memory_link_created webhook to fire on HTTP link"
    );

    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["event"], "memory_link_created");
    assert_eq!(body["memory_id"], src_id, "outer memory_id is the source");
    // Details flattened.
    assert_eq!(body["target_id"], dst_id);
    assert_eq!(body["relation"], "related_to");
    assert_eq!(body["namespace"], "v064-017");
}

#[tokio::test]
async fn webhook_fires_on_http_consolidate() {
    let harness = HttpHarness::new();
    let (mock, hook_url) = fresh_mock().await;
    subscribe_all(&harness.db_path, &hook_url);

    let src_a = seed_memory(&harness.db_path, "consolidate-a", "v064-017");
    let src_b = seed_memory(&harness.db_path, "consolidate-b", "v064-017");
    let (status, body) = harness
        .json_post(
            "/api/v1/consolidate",
            json!({
                "ids": [src_a, src_b],
                "title": "consolidated-result",
                "summary": "merged a + b",
                "namespace": "v064-017"
            }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "consolidate should succeed; body = {body}"
    );
    let new_id = body["id"]
        .as_str()
        .expect("response carries new id")
        .to_string();

    let received = wait_for_dispatch(&mock, Duration::from_secs(2)).await;
    assert!(
        !received.is_empty(),
        "expected memory_consolidated webhook to fire on HTTP consolidate"
    );

    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["event"], "memory_consolidated");
    assert_eq!(body["memory_id"], new_id);
    // Details flattened.
    assert_eq!(body["source_count"], 2);
    assert!(body["source_ids"].is_array(), "source_ids must be an array");
}
