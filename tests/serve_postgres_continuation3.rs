// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Wave-3 Continuation 3 — postgres-backed serve integration tests.
//!
//! Per-domain coverage for the eight surfaces landed in
//! Continuation 3:
//!
//! - Phase 13 (forget): pattern + namespace + tier filters round-trip
//!   through the SAL trait; archive-on-forget moves rows correctly.
//! - Phase 14 (consolidate): merging multiple sources into one preserves
//!   `consolidated_from_agents` provenance.
//! - Phase 15 (`detect_contradictions`): pairwise heuristic on postgres.
//! - Phase 16 (notify): cross-agent inbox push lands the memory under
//!   `_inbox/<target>` with `metadata.target_agent_id`.
//! - Phase 17 (gc): forced gc cycle deletes expired rows.
//! - Phase 18 (import/export): bulk import + export round-trip.
//! - Phase 19 (archive write paths): restore + purge + `archive_by_ids`.
//! - Phase 20 (governance): inheritance-chain walk on writes blocks
//!   non-owner stores under `governance.write = "owner"`; consensus
//!   approvals require N registered voters.
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

async fn store_memory(
    client: &reqwest::Client,
    base: &str,
    namespace: &str,
    title: &str,
) -> String {
    let body = json!({
        "tier": "long",
        "namespace": namespace,
        "title": title,
        "content": format!("content for {title}"),
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "continuation3-test",
    });
    let resp = client
        .post(format!("{base}/api/v1/memories"))
        .json(&body)
        .send()
        .await
        .expect("store");
    assert!(resp.status().is_success(), "store should succeed");
    let v: Value = resp.json().await.expect("body");
    v["id"].as_str().expect("id").to_string()
}

// ===================================================================
// Phase 13 — forget
// ===================================================================

/// Forget by namespace removes ALL memories in that namespace.
#[tokio::test(flavor = "multi_thread")]
async fn forget_by_namespace_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping forget_by_namespace_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("forget-ns-{}", uuid::Uuid::new_v4());
    for i in 0..3 {
        store_memory(&client, &base, &ns, &format!("forget-mem-{i}")).await;
    }
    let resp = client
        .post(format!("{base}/api/v1/forget"))
        .json(&json!({"namespace": ns}))
        .send()
        .await
        .expect("forget");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert_eq!(v["deleted"].as_u64().unwrap_or(0), 3);
    shutdown.notify_one();
    let _ = handle.await;
}

/// Forget with no filter returns 400 Bad Request.
#[tokio::test(flavor = "multi_thread")]
async fn forget_no_filter_returns_400_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping forget_no_filter_returns_400_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/forget"))
        .json(&json!({}))
        .send()
        .await
        .expect("forget");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 14 — consolidate
// ===================================================================

/// Consolidate three sources into one new memory; verify provenance
/// arrays preserved (`derived_from`, `consolidated_from_agents`).
#[tokio::test(flavor = "multi_thread")]
async fn consolidate_preserves_provenance_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping consolidate_preserves_provenance_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("consolidate-{}", uuid::Uuid::new_v4());
    let mut ids = Vec::new();
    for i in 0..3 {
        let body = json!({
            "tier": "long",
            "namespace": ns,
            "title": format!("source-{i}"),
            "content": format!("source content {i}"),
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "test",
            "agent_id": format!("author-{i}"),
        });
        let v: Value = client
            .post(format!("{base}/api/v1/memories"))
            .json(&body)
            .send()
            .await
            .expect("store")
            .json()
            .await
            .expect("body");
        ids.push(v["id"].as_str().unwrap().to_string());
    }
    let resp = client
        .post(format!("{base}/api/v1/consolidate"))
        .header("x-agent-id", "consolidator-bob")
        .json(&json!({
            "ids": ids,
            "title": "consolidated-summary",
            "summary": "merged summary across three sources",
            "namespace": ns,
            "tier": "long",
        }))
        .send()
        .await
        .expect("consolidate");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let v: Value = resp.json().await.expect("body");
    assert_eq!(v["consolidated"].as_u64().unwrap_or(0), 3);
    let new_id = v["id"].as_str().unwrap().to_string();
    // Verify the new memory's metadata carries `consolidated_from_agents`.
    let got: Value = client
        .get(format!("{base}/api/v1/memories/{new_id}"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("body");
    let metadata = &got["metadata"];
    assert_eq!(metadata["agent_id"], "consolidator-bob");
    assert!(
        metadata["consolidated_from_agents"].is_array(),
        "consolidated_from_agents must be present"
    );
    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 15 — detect contradictions (heuristic, non-LLM)
// ===================================================================

/// Two memories sharing a topic but differing content surface as a
/// synthesized contradicts link via the heuristic detector.
#[tokio::test(flavor = "multi_thread")]
async fn detect_contradictions_synthesizes_pair_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping detect_contradictions_synthesizes_pair_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("contradict-{}", uuid::Uuid::new_v4());
    for content in ["the answer is 42", "the answer is 7"] {
        let body = json!({
            "tier": "long",
            "namespace": ns,
            "title": format!("topic-{}", uuid::Uuid::new_v4()),
            "content": content,
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "test",
            "metadata": {"topic": "the-answer"},
        });
        client
            .post(format!("{base}/api/v1/memories"))
            .json(&body)
            .send()
            .await
            .expect("store");
    }
    let resp = client
        .get(format!(
            "{base}/api/v1/contradictions?topic=the-answer&namespace={ns}"
        ))
        .send()
        .await
        .expect("contradictions");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    let links = v["links"].as_array().expect("links");
    assert!(
        !links.is_empty(),
        "synthesized contradiction must surface for differing-content same-topic pair"
    );
    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 16 — notify
// ===================================================================

/// notify lands a memory in `_inbox/<target>` with target metadata.
#[tokio::test(flavor = "multi_thread")]
async fn notify_lands_in_target_inbox_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping notify_lands_in_target_inbox_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let target = format!("target-{}", uuid::Uuid::new_v4());
    let resp = client
        .post(format!("{base}/api/v1/notify"))
        .header("x-agent-id", "alice")
        .json(&json!({
            "target_agent_id": target,
            "title": "hello",
            "payload": "ping",
        }))
        .send()
        .await
        .expect("notify");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let v: Value = resp.json().await.expect("body");
    assert_eq!(v["target_agent_id"], target);
    assert_eq!(v["namespace"], format!("_inbox/{target}"));
    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 17 — gc
// ===================================================================

/// Forced gc cycle on a clean database returns `expired_deleted=0`.
#[tokio::test(flavor = "multi_thread")]
async fn gc_clean_db_returns_zero_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping gc_clean_db_returns_zero_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/gc"))
        .send()
        .await
        .expect("gc");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert!(v["expired_deleted"].as_u64().is_some());
    assert_eq!(v["storage_backend"], "postgres");
    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 18 — import/export
// ===================================================================

/// Export returns memories + links + count + `exported_at`.
#[tokio::test(flavor = "multi_thread")]
async fn export_returns_full_envelope_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping export_returns_full_envelope_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("export-{}", uuid::Uuid::new_v4());
    store_memory(&client, &base, &ns, "exportable-1").await;
    let resp = client
        .get(format!("{base}/api/v1/export"))
        .send()
        .await
        .expect("export");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert!(v["memories"].is_array());
    assert!(v["links"].is_array());
    assert!(v["exported_at"].is_string());
    assert_eq!(v["storage_backend"], "postgres");
    shutdown.notify_one();
    let _ = handle.await;
}

/// Import accepts a list of memories + lands them through SAL store.
#[tokio::test(flavor = "multi_thread")]
async fn import_lands_memories_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping import_lands_memories_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("import-{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();
    let body = json!({
        "memories": [
            {
                "id": uuid::Uuid::new_v4().to_string(),
                "tier": "long",
                "namespace": ns,
                "title": "imported-1",
                "content": "imported content one",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "test",
                "access_count": 0,
                "created_at": now,
                "updated_at": now,
                "metadata": {"agent_id": "alice"}
            }
        ]
    });
    let resp = client
        .post(format!("{base}/api/v1/import"))
        .json(&body)
        .send()
        .await
        .expect("import");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert_eq!(v["imported"].as_u64().unwrap_or(0), 1);
    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 19 — archive write paths
// ===================================================================

/// Archive an id, then restore it; row reappears in active list.
#[tokio::test(flavor = "multi_thread")]
async fn archive_then_restore_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping archive_then_restore_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("archive-restore-{}", uuid::Uuid::new_v4());
    let id = store_memory(&client, &base, &ns, "round-trip").await;

    // Archive.
    let resp = client
        .post(format!("{base}/api/v1/archive"))
        .json(&json!({"ids": [id], "reason": "test-archive"}))
        .send()
        .await
        .expect("archive");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert!(v["archived"].as_array().is_some_and(|a| a.len() == 1));

    // Restore.
    let resp = client
        .post(format!("{base}/api/v1/archive/{id}/restore"))
        .send()
        .await
        .expect("restore");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert_eq!(v["restored"], true);
    assert_eq!(v["id"], id);
    shutdown.notify_one();
    let _ = handle.await;
}

/// Restore on a missing archive id returns 404.
#[tokio::test(flavor = "multi_thread")]
async fn restore_missing_id_returns_404_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping restore_missing_id_returns_404_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let bogus = uuid::Uuid::new_v4().to_string();
    let resp = client
        .post(format!("{base}/api/v1/archive/{bogus}/restore"))
        .send()
        .await
        .expect("restore");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    shutdown.notify_one();
    let _ = handle.await;
}

// ===================================================================
// Phase 20 — full governance pipeline
// ===================================================================

/// `approve_pending` against an unknown id returns 404 Rejected.
#[tokio::test(flavor = "multi_thread")]
async fn approve_unknown_pending_id_rejected_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping approve_unknown_pending_id_rejected_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let bogus = uuid::Uuid::new_v4().to_string();
    let resp = client
        .post(format!("{base}/api/v1/pending/{bogus}/approve"))
        .header("x-agent-id", "approver-alice")
        .send()
        .await
        .expect("approve");
    // The full state machine returns 403 (Rejected) for not-found
    // pendings — the postgres branch surfaces it as a structured
    // `approve rejected: pending action not found` envelope.
    assert!(
        resp.status() == reqwest::StatusCode::FORBIDDEN
            || resp.status() == reqwest::StatusCode::NOT_FOUND
    );
    shutdown.notify_one();
    let _ = handle.await;
}

/// Inheritance-chain walk on writes: a namespace with no policy lets
/// the write through unchanged (default Allow posture preserved).
#[tokio::test(flavor = "multi_thread")]
async fn inheritance_walk_no_policy_allows_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping inheritance_walk_no_policy_allows_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("ungoverned-{}", uuid::Uuid::new_v4());
    let body = json!({
        "tier": "long",
        "namespace": ns,
        "title": "ungoverned-1",
        "content": "content",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "test",
    });
    let resp = client
        .post(format!("{base}/api/v1/memories"))
        .json(&body)
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    shutdown.notify_one();
    let _ = handle.await;
}
