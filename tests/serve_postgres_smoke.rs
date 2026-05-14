// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::collapsible_if, clippy::doc_markdown, clippy::too_many_lines)]
//! v0.7.0 Wave-3 — `ai-memory serve --store-url postgres://…` smoke test.
//!
//! Boots an in-process HTTP daemon backed by [`PostgresStore`] (the same
//! adapter `serve` resolves at startup) and exercises the day-1 portable
//! HTTP surface end-to-end: store, get, list, search, link, list-links,
//! delete, capabilities. Closes the loop on the Wave-3 deliverable —
//! every refactored handler is asserted to round-trip cleanly through
//! the SAL trait against a live Postgres database.
//!
//! ## Gating
//!
//! Requires both:
//!
//! - `feature = "sal-postgres"` so the adapter compiles.
//! - `AI_MEMORY_TEST_POSTGRES_URL` set at run time to a fresh,
//!   disposable database. The test bootstraps its own schema via the
//!   adapter's `INIT_SCHEMA` side-effect and leaves no junk rows
//!   behind, but it does not drop the database — operators are
//!   expected to point at a per-CI-run scratch database.
//!
//! Without either, the test `eprintln!`s a skip message and returns
//! cleanly — matching the pattern used by `tests/sal_contract.rs` and
//! `tests/postgres_schema_parity.rs`.
//!
//! ## What this proves
//!
//! - `serve --store-url postgres://...` boots, binds, serves health.
//! - `/api/v1/capabilities.storage_backend == "postgres"`.
//! - The eight Wave-3-migrated endpoints round-trip through the SAL
//!   trait against the live Postgres adapter:
//!   `POST/GET/PUT/DELETE /api/v1/memories(/:id)`,
//!   `GET /api/v1/memories`, `GET /api/v1/search`,
//!   `POST /api/v1/links`, `GET /api/v1/memories/:id/links`.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::store::MemoryStore;
use ai_memory::store::postgres::PostgresStore;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock};

/// Returns Some(url) when the live-PG fixture is configured, None otherwise.
fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

/// Pick a free local port. Mirrors the helper used by
/// `tests/integration.rs::test_daemon_cmd_serve_responds_to_health_then_terminates`.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// Build the `AppState` for a postgres-backed in-process daemon.
///
/// `db` carries a fresh `:memory:` SQLite connection — the legacy
/// direct-rusqlite handlers reference it for things like the WAL
/// checkpoint loop, but every Wave-3-migrated handler routes through
/// `app.store` which is the live PostgresStore.
async fn build_postgres_app_state(url: &str) -> AppState {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).expect("scratch sqlite");
    let path = std::path::PathBuf::from(":memory:");
    let db: Db = Arc::new(Mutex::new((conn, path, ResolvedTtl::default(), true)));
    let store: Arc<dyn MemoryStore> = Arc::new(
        PostgresStore::connect(url)
            .await
            .expect("connect postgres adapter"),
    );
    AppState {
        db,
        embedder: Arc::new(None),
        vector_index: Arc::new(Mutex::new(None)),
        federation: Arc::new(None),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(None),
        family_embeddings: Arc::new(RwLock::new(Some(Vec::new()))),
        storage_backend: StorageBackend::Postgres,
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_postgres_smoke_round_trip() {
    let Some(url) = postgres_url() else {
        eprintln!(
            "skipping serve_postgres_smoke_round_trip: \
             AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let app_state = build_postgres_app_state(&url).await;

    // Spin up the daemon in-process. The shared `build_router` is the
    // same route table `serve()` binds in production.
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_daemon = shutdown.clone();
    let addr_for_daemon = addr.clone();
    let handle = tokio::spawn(async move {
        ai_memory::daemon_runtime::serve_http_with_shutdown(
            &addr_for_daemon,
            api_key_state,
            app_state,
            shutdown_for_daemon,
        )
        .await
    });

    // Wait for the listener to come up. ~5s budget with 100ms backoff.
    let mut ready = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(resp) = reqwest::get(&format!("http://{addr}/api/v1/health")).await {
            if resp.status() == reqwest::StatusCode::OK {
                ready = true;
                break;
            }
        }
    }
    assert!(
        ready,
        "postgres-backed serve health probe never returned 200 — \
         in-process HTTP daemon failed to bind"
    );

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // ------------------------------------------------------------------
    // 1) /capabilities — surfaces `storage_backend == "postgres"`.
    // ------------------------------------------------------------------
    let caps: Value = client
        .get(format!("{base}/api/v1/capabilities"))
        .send()
        .await
        .expect("capabilities GET")
        .json()
        .await
        .expect("capabilities body");
    assert_eq!(
        caps["storage_backend"], "postgres",
        "capabilities must surface storage_backend=postgres on a PG-backed daemon"
    );

    // ------------------------------------------------------------------
    // 2) POST /memories — store via the SAL trait.
    // ------------------------------------------------------------------
    let unique = format!("smoke-{}", uuid::Uuid::new_v4());
    let create = client
        .post(format!("{base}/api/v1/memories"))
        .json(&json!({
            "tier": "long",
            "namespace": unique.clone(),
            "title": "wave3-smoke",
            "content": "v0.7.0 Wave-3 smoke memory — postgres-backed daemon round-trip",
            "tags": ["smoke", "wave3"],
            "priority": 7,
            "confidence": 0.95,
            "source": "smoke-test",
            "metadata": {}
        }))
        .send()
        .await
        .expect("create POST");
    assert_eq!(create.status(), reqwest::StatusCode::CREATED);
    let create_body: Value = create.json().await.expect("create body");
    let mem_id = create_body["id"]
        .as_str()
        .expect("created memory has id")
        .to_string();

    // ------------------------------------------------------------------
    // 3) GET /memories/:id — fetch via the SAL trait.
    // ------------------------------------------------------------------
    let got: Value = client
        .get(format!("{base}/api/v1/memories/{mem_id}"))
        .send()
        .await
        .expect("get GET")
        .json()
        .await
        .expect("get body");
    assert_eq!(got["memory"]["id"], mem_id);
    assert_eq!(got["memory"]["title"], "wave3-smoke");
    assert!(got["links"].is_array());

    // ------------------------------------------------------------------
    // 4) GET /memories — list via the SAL trait, filtered to our ns.
    // ------------------------------------------------------------------
    let list: Value = client
        .get(format!("{base}/api/v1/memories"))
        .query(&[("namespace", unique.as_str())])
        .send()
        .await
        .expect("list GET")
        .json()
        .await
        .expect("list body");
    assert!(list["memories"].is_array());
    assert!(list["count"].as_u64().unwrap_or(0) >= 1);

    // ------------------------------------------------------------------
    // 5) GET /search — search via the SAL trait.
    // ------------------------------------------------------------------
    let search: Value = client
        .get(format!("{base}/api/v1/search"))
        .query(&[("q", "wave3-smoke")])
        .send()
        .await
        .expect("search GET")
        .json()
        .await
        .expect("search body");
    assert!(search["results"].is_array());

    // ------------------------------------------------------------------
    // 6) PUT /memories/:id — update via the SAL trait.
    // ------------------------------------------------------------------
    let upd = client
        .put(format!("{base}/api/v1/memories/{mem_id}"))
        .json(&json!({
            "title": "wave3-smoke-updated",
            "priority": 9,
        }))
        .send()
        .await
        .expect("update PUT");
    assert_eq!(upd.status(), reqwest::StatusCode::OK);

    // Re-fetch to confirm.
    let got2: Value = client
        .get(format!("{base}/api/v1/memories/{mem_id}"))
        .send()
        .await
        .expect("get-after-update")
        .json()
        .await
        .expect("get-after-update body");
    assert_eq!(got2["memory"]["title"], "wave3-smoke-updated");

    // ------------------------------------------------------------------
    // 7) POST /links + GET /memories/:id/links — link via the SAL trait.
    // ------------------------------------------------------------------
    let other = client
        .post(format!("{base}/api/v1/memories"))
        .json(&json!({
            "tier": "long",
            "namespace": unique.clone(),
            "title": "wave3-smoke-target",
            "content": "second memory for link round-trip",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "smoke-test",
            "metadata": {}
        }))
        .send()
        .await
        .expect("create target POST");
    assert_eq!(other.status(), reqwest::StatusCode::CREATED);
    let other_body: Value = other.json().await.expect("create target body");
    let other_id = other_body["id"].as_str().expect("target id").to_string();

    let link = client
        .post(format!("{base}/api/v1/links"))
        .json(&json!({
            "source_id": mem_id.clone(),
            "target_id": other_id.clone(),
            "relation": "related_to",
        }))
        .send()
        .await
        .expect("link POST");
    assert_eq!(link.status(), reqwest::StatusCode::CREATED);
    let link_body: Value = link.json().await.expect("link body");
    assert_eq!(link_body["linked"], true);
    assert!(matches!(
        link_body["attest_level"].as_str(),
        Some("unsigned" | "self_signed")
    ));

    let edges: Value = client
        .get(format!("{base}/api/v1/memories/{mem_id}/links"))
        .send()
        .await
        .expect("links GET")
        .json()
        .await
        .expect("links body");
    let arr = edges["links"].as_array().expect("links array");
    assert!(
        arr.iter().any(|e| e["target_id"] == other_id.as_str()),
        "freshly-created link must round-trip through list_links"
    );

    // ------------------------------------------------------------------
    // 8) DELETE /memories/:id — delete via the SAL trait.
    // ------------------------------------------------------------------
    let del = client
        .delete(format!("{base}/api/v1/memories/{mem_id}"))
        .send()
        .await
        .expect("delete DELETE");
    assert_eq!(del.status(), reqwest::StatusCode::OK);
    let del_body: Value = del.json().await.expect("delete body");
    assert_eq!(del_body["deleted"], true);
    let _ = client
        .delete(format!("{base}/api/v1/memories/{other_id}"))
        .send()
        .await;

    // ------------------------------------------------------------------
    // Tear down. Notify shutdown; wait for the daemon task to exit.
    // ------------------------------------------------------------------
    shutdown.notify_one();
    let join = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    match join {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => panic!("serve_http_with_shutdown errored: {e}"),
        Ok(Err(e)) => panic!("daemon task panicked: {e}"),
        Err(elapsed) => {
            panic!("daemon failed to terminate within 5s of shutdown notify: {elapsed}")
        }
    }
}
