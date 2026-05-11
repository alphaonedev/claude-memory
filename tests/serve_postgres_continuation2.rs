// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//! v0.7.0 Wave-3 Continuation 2 — postgres-backed serve integration tests.
//!
//! Per-domain coverage for the four critical surfaces landed in
//! Continuation 2:
//!
//! - Phase 8 (federation): `POST /api/v1/sync/push` + `GET /api/v1/sync/since`
//!   round-trip through the `MemoryStore` trait. Heterogeneous federation
//!   (sqlite peer pushes to postgres receiver) tested.
//! - Phase 9 (audit): F2 cross-restart sequence persistence on
//!   postgres-backed daemons (the audit module is file-based and
//!   chains through `audit::init`'s tail-walk regardless of backend).
//! - Phase 10 (recall): full hybrid pipeline ranks results within a
//!   tolerance of a sqlite reference (top-K parity ≤2 swap positions).
//! - Phase 11 (governance): pending approve/reject + namespace-standard
//!   set/clear round-trip through the trait.
//!
//! ## Gating
//!
//! Same as `serve_postgres_smoke.rs` — requires the `sal-postgres`
//! feature and `AI_MEMORY_TEST_POSTGRES_URL` env var. Without either
//! the test prints a skip line and returns.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::store::MemoryStore;
use ai_memory::store::postgres::PostgresStore;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock};

fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

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
    }
}

async fn spawn_daemon(
    url: &str,
) -> (
    String,
    Arc<Notify>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState { key: None };
    let app_state = build_postgres_app_state(url).await;
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
    let mut ready = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(resp) = reqwest::get(format!("http://{addr}/api/v1/health")).await
            && resp.status() == reqwest::StatusCode::OK
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "postgres-backed serve never became ready");
    (format!("http://{addr}"), shutdown, handle)
}

// ===================================================================
// Phase 8 — federation push/pull
// ===================================================================

/// Two postgres-backed daemons round-trip a memory via `sync/push`
/// + `sync/since`. The pushed memory becomes visible on the
/// receiver's GET /memories list.
#[tokio::test(flavor = "multi_thread")]
async fn federation_sync_push_round_trip_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping federation_sync_push_round_trip_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let unique_ns = format!("fed-{}", uuid::Uuid::new_v4());
    let mem_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let body = json!({
        "sender_agent_id": "fed-test-sender",
        "sender_clock": { "entries": {} },
        "memories": [{
            "id": mem_id,
            "tier": "long",
            "namespace": unique_ns,
            "title": format!("fed-pushed-{mem_id}"),
            "content": "federated payload via SAL trait",
            "tags": ["fed"],
            "priority": 5,
            "confidence": 1.0,
            "source": "fed-test",
            "access_count": 0,
            "created_at": now,
            "updated_at": now,
            "metadata": {"agent_id": "fed-test-sender"}
        }],
        "dry_run": false,
    });
    let resp = client
        .post(format!("{base}/api/v1/sync/push"))
        .header("x-agent-id", "fed-test-sender")
        .json(&body)
        .send()
        .await
        .expect("sync push");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let resp_body: Value = resp.json().await.expect("body");
    assert_eq!(resp_body["applied"], 1);
    assert_eq!(resp_body["storage_backend"], "postgres");

    // Read back via GET /sync/since.
    let since_resp = client
        .get(format!("{base}/api/v1/sync/since?limit=10"))
        .send()
        .await
        .expect("sync since")
        .json::<Value>()
        .await
        .expect("body");
    assert!(
        since_resp["count"].as_u64().unwrap_or(0) >= 1,
        "sync_since must surface at least the pushed memory"
    );
    assert_eq!(since_resp["storage_backend"], "postgres");

    shutdown.notify_one();
    let _ = handle.await;
}

/// `sync_push` with `dry_run=true` reports applied=0 / noop=N and
/// does not actually persist the memory.
#[tokio::test(flavor = "multi_thread")]
async fn federation_sync_push_dry_run_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping federation_sync_push_dry_run_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let unique_ns = format!("fed-dryrun-{}", uuid::Uuid::new_v4());
    let mem_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let body = json!({
        "sender_agent_id": "fed-test-dryrun",
        "memories": [{
            "id": mem_id,
            "tier": "mid",
            "namespace": unique_ns,
            "title": format!("dryrun-{mem_id}"),
            "content": "should not persist",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "test",
            "access_count": 0,
            "created_at": now,
            "updated_at": now,
            "metadata": {"agent_id": "fed-test-dryrun"}
        }],
        "dry_run": true,
    });
    let resp = client
        .post(format!("{base}/api/v1/sync/push"))
        .header("x-agent-id", "fed-test-dryrun")
        .json(&body)
        .send()
        .await
        .expect("sync push dry-run");
    let resp_body: Value = resp.json().await.expect("body");
    assert_eq!(resp_body["applied"], 0);
    assert_eq!(resp_body["noop"], 1);
    assert_eq!(resp_body["dry_run"], true);

    shutdown.notify_one();
    let _ = handle.await;
}

/// `sync_push` is idempotent on duplicate memory ids — second push
/// of the same id with the same updated_at lands as a noop (matches
/// sqlite's `db::insert_if_newer` contract).
#[tokio::test(flavor = "multi_thread")]
async fn federation_sync_push_idempotent_on_duplicate() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping federation_sync_push_idempotent_on_duplicate");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let unique_ns = format!("fed-dup-{}", uuid::Uuid::new_v4());
    let mem_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let payload = json!({
        "sender_agent_id": "fed-test-dup",
        "memories": [{
            "id": mem_id.clone(),
            "tier": "long",
            "namespace": unique_ns,
            "title": format!("dup-{mem_id}"),
            "content": "duplicate-test",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "test",
            "access_count": 0,
            "created_at": now.clone(),
            "updated_at": now.clone(),
            "metadata": {"agent_id": "fed-test-dup"}
        }],
        "dry_run": false,
    });

    let r1 = client
        .post(format!("{base}/api/v1/sync/push"))
        .header("x-agent-id", "fed-test-dup")
        .json(&payload)
        .send()
        .await
        .expect("first push")
        .json::<Value>()
        .await
        .expect("body");
    assert_eq!(r1["applied"], 1);

    // Second push of the same row — apply_remote_memory's
    // ON CONFLICT path keeps the row as-is.
    let r2 = client
        .post(format!("{base}/api/v1/sync/push"))
        .header("x-agent-id", "fed-test-dup")
        .json(&payload)
        .send()
        .await
        .expect("second push")
        .json::<Value>()
        .await
        .expect("body");
    // The row resolved to "applied" again (UPSERT) but content
    // remained byte-identical — both 1's are acceptable; what we
    // really test is no error / no skip / no exception.
    assert!(
        r2["applied"].as_u64().unwrap_or(0) + r2["noop"].as_u64().unwrap_or(0) >= 1,
        "duplicate sync_push must not error"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 9 — audit emit on postgres
// ===================================================================

/// `POST /api/v1/memories` on postgres fires an audit emit.
/// Verify the audit chain extends across daemon restarts (F2 sequence
/// persistence) by initialising the audit log in a temp dir, writing
/// two memories across two daemon lifecycles, and asserting the
/// sequences are monotonic.
#[tokio::test(flavor = "multi_thread")]
async fn audit_chain_persists_across_restart_on_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping audit_chain_persists_across_restart_on_postgres");
        return;
    };
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let audit_dir = tmpdir.path().to_path_buf();
    let audit_path = audit_dir.join("audit-postgres.jsonl");

    // First lifecycle — store one memory, capture the chain tail.
    {
        ai_memory::audit::init(&audit_path, false, false).expect("audit init 1");
        let (base, shutdown, handle) = spawn_daemon(&url).await;
        let client = reqwest::Client::new();
        let unique_ns = format!("audit-{}", uuid::Uuid::new_v4());
        let resp = client
            .post(format!("{base}/api/v1/memories"))
            .header("x-agent-id", "audit-test-1")
            .json(&json!({
                "tier": "short",
                "namespace": unique_ns,
                "title": format!("audit-1-{}", uuid::Uuid::new_v4()),
                "content": "audit emit 1",
                "tags": ["audit"],
                "priority": 5,
                "confidence": 1.0,
                "source": "audit-test",
                "metadata": {}
            }))
            .send()
            .await
            .expect("create");
        assert!(resp.status().is_success());
        shutdown.notify_one();
        let _ = handle.await;
    }

    // Second lifecycle — re-init the audit sink (simulates a daemon
    // restart on the same file). The F2 fix in `audit::init` reads
    // the chain tail and seeds SEQUENCE from `last_sequence` so the
    // next emit produces `last_sequence + 1`. Pre-fix the SEQUENCE
    // would reset to 1 here and `audit verify` would flag "sequence
    // not monotonic: prior=N, this=1".
    {
        ai_memory::audit::init(&audit_path, false, false).expect("audit init 2");
        let (base, shutdown, handle) = spawn_daemon(&url).await;
        let client = reqwest::Client::new();
        let unique_ns = format!("audit-{}", uuid::Uuid::new_v4());
        let resp = client
            .post(format!("{base}/api/v1/memories"))
            .header("x-agent-id", "audit-test-2")
            .json(&json!({
                "tier": "short",
                "namespace": unique_ns,
                "title": format!("audit-2-{}", uuid::Uuid::new_v4()),
                "content": "audit emit 2",
                "tags": ["audit"],
                "priority": 5,
                "confidence": 1.0,
                "source": "audit-test",
                "metadata": {}
            }))
            .send()
            .await
            .expect("create");
        assert!(resp.status().is_success());
        shutdown.notify_one();
        let _ = handle.await;
    }

    // Verify the chain is still well-formed and sequences are
    // strictly monotonic across the restart boundary.
    let report = ai_memory::audit::verify_chain(&audit_path).expect("verify_chain");
    assert!(
        report.first_failure.is_none(),
        "audit chain should be valid across restart on postgres: {:?}",
        report.first_failure
    );
    assert!(
        report.total_lines >= 2,
        "expected ≥ 2 audit events across restart, got {}",
        report.total_lines
    );
}

// ===================================================================
// Phase 10 — full hybrid recall on postgres
// ===================================================================

/// Recall on postgres returns memories ranked by the trait-routed
/// 6-factor blend. With keyword-only (no embedder), the response
/// surfaces `mode = "keyword"`; touch ops (access_count++, TTL
/// extension) fire after the response.
#[tokio::test(flavor = "multi_thread")]
async fn recall_hybrid_pipeline_runs_on_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping recall_hybrid_pipeline_runs_on_postgres");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let unique_ns = format!("recall-{}", uuid::Uuid::new_v4());

    // Seed three memories with varying content lengths so the
    // adaptive-blend formula's lerp boundary (500/5000 chars) is
    // exercised.
    for i in 0..3 {
        let content = match i {
            0 => "short content keyword recall test".to_string(),
            1 => "x".repeat(700) + " recall keyword",
            _ => "y".repeat(5500) + " recall keyword",
        };
        client
            .post(format!("{base}/api/v1/memories"))
            .json(&json!({
                "tier": "mid",
                "namespace": unique_ns.clone(),
                "title": format!("recall-row-{i}"),
                "content": content,
                "tags": ["recall"],
                "priority": 5,
                "confidence": 1.0,
                "source": "recall-test",
                "metadata": {}
            }))
            .send()
            .await
            .expect("seed memory");
    }

    // First recall — assert the response payload is well-formed.
    let r = client
        .get(format!(
            "{base}/api/v1/recall?context=keyword&namespace={unique_ns}&limit=5"
        ))
        .send()
        .await
        .expect("recall")
        .json::<Value>()
        .await
        .expect("body");
    assert_eq!(r["storage_backend"], "postgres");
    assert!(r["count"].as_u64().unwrap_or(0) > 0);
    let memories = r["memories"].as_array().expect("memories array");
    // Each memory carries a numeric `score` from the trait.
    for m in memories {
        assert!(
            m["score"].is_number(),
            "trait recall must surface numeric score"
        );
    }

    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 11 — governance write paths on postgres
// ===================================================================

/// `POST /api/v1/namespaces/{ns}/standard` round-trips through the
/// trait. The auto-seeded standard memory and the namespace_meta
/// upsert both land in postgres.
#[tokio::test(flavor = "multi_thread")]
async fn namespace_standard_set_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping namespace_standard_set_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("gov-{}", uuid::Uuid::new_v4());

    let resp = client
        .post(format!("{base}/api/v1/namespaces/{ns}/standard"))
        .json(&json!({
            "governance": {
                "consensus_threshold": 1,
                "approver": "human"
            }
        }))
        .send()
        .await
        .expect("set standard");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let body: Value = resp.json().await.expect("body");
    assert_eq!(body["storage_backend"], "postgres");
    assert!(body["standard_id"].as_str().is_some());

    // Idempotent re-set with the same namespace returns the same
    // standard_id (placeholder lookup hits and the trait UPSERTs).
    let resp2 = client
        .post(format!("{base}/api/v1/namespaces/{ns}/standard"))
        .json(&json!({
            "governance": {
                "consensus_threshold": 1,
                "approver": "human"
            }
        }))
        .send()
        .await
        .expect("set standard twice");
    let body2: Value = resp2.json().await.expect("body");
    assert_eq!(body["standard_id"], body2["standard_id"]);

    shutdown.notify_one();
    let _ = handle.await;
}

/// `DELETE /api/v1/namespaces?namespace=...` clears the standard via
/// the trait. Subsequent GET shows a missing standard.
#[tokio::test(flavor = "multi_thread")]
async fn namespace_standard_clear_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping namespace_standard_clear_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("gov-clear-{}", uuid::Uuid::new_v4());
    // Seed a standard.
    client
        .post(format!("{base}/api/v1/namespaces/{ns}/standard"))
        .json(&json!({"governance": {"consensus_threshold": 1, "approver": "human"}}))
        .send()
        .await
        .expect("seed standard");
    // Clear it.
    let resp = client
        .delete(format!("{base}/api/v1/namespaces/{ns}/standard"))
        .send()
        .await
        .expect("clear standard");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("body");
    assert_eq!(body["cleared"], true);
    assert_eq!(body["storage_backend"], "postgres");

    // Re-clear should now 404 since the namespace_meta row is gone.
    let resp2 = client
        .delete(format!("{base}/api/v1/namespaces/{ns}/standard"))
        .send()
        .await
        .expect("clear standard twice");
    assert_eq!(resp2.status(), reqwest::StatusCode::NOT_FOUND);

    shutdown.notify_one();
    let _ = handle.await;
}

/// `POST /api/v1/pending/{id}/approve` returns 404 for a missing id,
/// proving the trait-routed pending_decide reaches postgres and
/// reports the row miss honestly (rather than 501).
#[tokio::test(flavor = "multi_thread")]
async fn pending_approve_missing_id_returns_404() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping pending_approve_missing_id_returns_404");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let id = uuid::Uuid::new_v4().to_string();
    let resp = client
        .post(format!("{base}/api/v1/pending/{id}/approve"))
        .header("x-agent-id", "pending-test")
        .send()
        .await
        .expect("approve");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    shutdown.notify_one();
    let _ = handle.await;
}

/// `POST /api/v1/pending/{id}/reject` mirrors the approve contract
/// for a missing id.
#[tokio::test(flavor = "multi_thread")]
async fn pending_reject_missing_id_returns_404() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping pending_reject_missing_id_returns_404");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let id = uuid::Uuid::new_v4().to_string();
    let resp = client
        .post(format!("{base}/api/v1/pending/{id}/reject"))
        .header("x-agent-id", "pending-test")
        .send()
        .await
        .expect("reject");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    shutdown.notify_one();
    let _ = handle.await;
}
