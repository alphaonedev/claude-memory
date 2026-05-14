// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::match_wildcard_for_single_variants)]
//! v0.7.0 K10 — `GET /api/v1/approvals/stream` SSE endpoint.
//!
//! The endpoint subscribes to the process-wide approval bus
//! (`approvals::subscribe`) and emits SSE frames for every
//! `approval_requested` and `approval_decided` event published. We
//! exercise that contract two ways:
//!
//!   1. **In-process bus** — subscribe directly via
//!      `approvals::subscribe`, fire `approvals::publish`, assert the
//!      event arrives. This pins the load-bearing fan-out without
//!      depending on the SSE response-stream wiring (which is best
//!      tested via integration tests against a real bound socket).
//!   2. **Dispatcher hook** — call
//!      `subscriptions::dispatch_approval_requested` against a freshly
//!      seeded `pending_actions` row and assert an `ApprovalRequested`
//!      frame fires on a subscriber attached BEFORE the dispatch
//!      (broadcast channels do not replay history, so the subscribe
//!      ordering matters).

use ai_memory::approvals::{ApprovalEvent, publish, subscribe};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn subscribe_then_publish_round_trip() {
    let mut rx = subscribe();
    let evt = ApprovalEvent::ApprovalRequested {
        pending_id: "pa-sse-1".into(),
        action_type: "store".into(),
        namespace: "scratch".into(),
        requested_by: "alice".into(),
        requested_at: "2026-05-05T00:00:00Z".into(),
    };
    publish(evt.clone());
    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("recv timeout")
        .expect("recv channel closed");
    match received {
        ApprovalEvent::ApprovalRequested { pending_id, .. } => {
            assert_eq!(pending_id, "pa-sse-1");
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_approval_requested_fans_out_on_bus() {
    // Set up a fresh in-memory DB and seed a pending row so
    // dispatch_approval_requested can read it back.
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let payload = json!({"title": "k10-sse", "content": "x", "namespace": "ns-sse"});
    let pending_id = ai_memory::db::queue_pending_action(
        &conn,
        ai_memory::models::GovernedAction::Store,
        "ns-sse",
        None,
        "alice",
        &payload,
    )
    .expect("queue_pending_action");

    // Subscribe BEFORE firing — broadcast channels never replay.
    let mut rx = subscribe();

    // Fire the dispatcher; it publishes on the K10 approval bus AND
    // attempts the legacy webhook fan-out (which is a no-op here
    // because no subscriptions are registered).
    ai_memory::subscriptions::dispatch_approval_requested(
        &conn,
        &pending_id,
        std::path::Path::new(":memory:"),
    );

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("recv timeout")
        .expect("recv channel closed");
    match received {
        ApprovalEvent::ApprovalRequested {
            pending_id: pid,
            namespace,
            requested_by,
            action_type,
            ..
        } => {
            assert_eq!(pid, pending_id);
            assert_eq!(namespace, "ns-sse");
            assert_eq!(requested_by, "alice");
            assert_eq!(action_type, "store");
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn http_sse_endpoint_emits_event_to_attached_client() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let path = std::path::PathBuf::from(":memory:");
    let db: ai_memory::handlers::Db = std::sync::Arc::new(tokio::sync::Mutex::new((
        conn,
        path,
        ai_memory::config::ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: std::sync::Arc<dyn ai_memory::store::MemoryStore> = {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile for SqliteStore");
        let p = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        std::sync::Arc::new(
            ai_memory::store::sqlite::SqliteStore::open(&p).expect("open SqliteStore"),
        )
    };
    let app_state = ai_memory::handlers::AppState {
        db: db.clone(),
        embedder: std::sync::Arc::new(None),
        vector_index: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        federation: std::sync::Arc::new(None),
        tier_config: std::sync::Arc::new(ai_memory::config::FeatureTier::Keyword.config()),
        scoring: std::sync::Arc::new(ai_memory::config::ResolvedScoring::default()),
        profile: std::sync::Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: std::sync::Arc::new(None),
        active_keypair: std::sync::Arc::new(None),
        family_embeddings: std::sync::Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: std::sync::Arc::new(None),
        auto_tag_model: std::sync::Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: std::sync::Arc::new(None),
        deferred_audit_queue: std::sync::Arc::new(None),
    };
    let api_key_state = ai_memory::handlers::ApiKeyState { key: None };
    let router = ai_memory::build_router(api_key_state, app_state);

    // Initiate the SSE request. axum returns immediately with the
    // streaming body; we hold the response body and pull a single
    // chunk after publishing an event.
    // K10 review #628 blocker C2: SSE subscribers must self-identify
    // so the receive-side filter can scope events to the right tenant.
    // The pending row this test fires sets `requested_by="operator"`,
    // so the subscriber identifies as the same agent.
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/approvals/stream")
        .header("accept", "text/event-stream")
        .header("x-agent-id", "operator")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut body = resp.into_body();

    // Spawn a task that fires the dispatcher slightly after we begin
    // polling the body (so the subscriber inside the handler is
    // attached before the broadcast).
    let conn_for_publish = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let payload = json!({"title": "x", "content": "y", "namespace": "ns-sse-http"});
    let pending_id = ai_memory::db::queue_pending_action(
        &conn_for_publish,
        ai_memory::models::GovernedAction::Store,
        "ns-sse-http",
        None,
        "operator",
        &payload,
    )
    .expect("queue_pending_action");
    tokio::spawn(async move {
        // Small delay so the SSE handler attaches its subscriber
        // before the publish fires. 100ms is generous on CI hardware.
        tokio::time::sleep(Duration::from_millis(100)).await;
        ai_memory::subscriptions::dispatch_approval_requested(
            &conn_for_publish,
            &pending_id,
            std::path::Path::new(":memory:"),
        );
    });

    // Read until we get a non-empty data frame OR timeout.
    let mut buf = Vec::new();
    let read = async {
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(bytes) = frame.data_ref() {
                        buf.extend_from_slice(bytes);
                        let s = String::from_utf8_lossy(&buf);
                        if s.contains("approval_requested") {
                            return Ok::<(), String>(());
                        }
                    }
                }
                Some(Err(e)) => return Err(format!("body error: {e}")),
                None => return Err("body ended before event".into()),
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("SSE timeout — no approval_requested event in 5s");
    result.expect("SSE read failed");
    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.contains("event: approval_requested"),
        "expected SSE `event: approval_requested` line; got: {text}"
    );
}
