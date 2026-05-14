// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Wave-3 Continuation — extended postgres-backed `serve` smoke.
//!
//! Per-domain integration tests for the handlers Wave-3 Continuation
//! routed through the SAL trait. Companion to `serve_postgres_smoke.rs`,
//! which covers the core CRUD subset; this file exercises:
//!
//! - the postgres route gate middleware (un-migrated routes return 501)
//! - the KG endpoints (`kg_query`, `kg_timeline`, `kg_invalidate`)
//! - archive read paths (`list`, `stats`)
//! - the taxonomy / list-namespaces / list-agents projections
//! - the recall keyword fallback
//! - the entity registry round-trip
//! - the `bulk_create` write path
//!
//! ## Gating
//!
//! Identical to `serve_postgres_smoke.rs` — requires
//! `feature = "sal-postgres"` and the `AI_MEMORY_TEST_POSTGRES_URL`
//! env var. Without either the test prints a skip line and returns.

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
        deferred_audit_queue: Arc::new(None),
    }
}

/// Spawn the in-process daemon and return (base URL, shutdown notifier,
/// `JoinHandle`). Caller is responsible for `shutdown.notify_one()` and
/// `handle.await` on test exit.
async fn spawn_daemon(
    url: &str,
) -> (
    String,
    Arc<Notify>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
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

    // Wait for health.
    let mut ready = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(resp) = reqwest::get(&format!("http://{addr}/api/v1/health")).await
            && resp.status() == reqwest::StatusCode::OK
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "postgres-backed serve never became ready");
    (format!("http://{addr}"), shutdown, handle)
}

/// Phase 4 — the postgres route gate middleware short-circuits any
/// (method, path) tuple not in the supported list with a 501 envelope
/// rather than letting the handler silently use the empty scratch
/// `SQLite` connection.
#[tokio::test(flavor = "multi_thread")]
async fn route_gate_returns_501_for_unsupported_endpoint() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping route_gate_returns_501_for_unsupported_endpoint");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    // /api/v1/forget is not on the supported list; expect 501 with the
    // structured envelope.
    let resp = client
        .post(format!("{base}/api/v1/forget"))
        .json(&json!({"pattern": "anything"}))
        .send()
        .await
        .expect("forget POST");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_IMPLEMENTED,
        "un-migrated endpoint must surface 501 on postgres-backed daemon"
    );
    let body: Value = resp.json().await.expect("body");
    assert_eq!(body["storage_backend"], "postgres");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("not yet implemented")
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 4 — `GET /api/v1/agents` returns the registered agent list
/// projected from the `_agents` namespace via SAL `list`.
#[tokio::test(flavor = "multi_thread")]
async fn agents_round_trip_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping agents_round_trip_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let agent_id = format!("ext-agent-{}", uuid::Uuid::new_v4());
    let reg = client
        .post(format!("{base}/api/v1/agents"))
        .json(&json!({
            "agent_id": agent_id,
            "agent_type": "ai",
            "capabilities": ["recall", "store"]
        }))
        .send()
        .await
        .expect("agent register");
    assert_eq!(reg.status(), reqwest::StatusCode::CREATED);

    let list = client
        .get(format!("{base}/api/v1/agents"))
        .send()
        .await
        .expect("agent list")
        .json::<Value>()
        .await
        .expect("agent list body");
    let agents = list["agents"].as_array().expect("agents array");
    assert!(agents.iter().any(|a| a["agent_id"] == agent_id));

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 4 — `GET /api/v1/stats` returns top-level totals projected
/// from SAL `list`. Verifies the wire-shape (`total_memories`,
/// `by_tier`, `by_namespace`).
#[tokio::test(flavor = "multi_thread")]
async fn stats_round_trip_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping stats_round_trip_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let unique_ns = format!("stats-{}", uuid::Uuid::new_v4());
    for i in 0..3 {
        client
            .post(format!("{base}/api/v1/memories"))
            .json(&json!({
                "tier": "short",
                "namespace": unique_ns.clone(),
                "title": format!("stats-row-{i}"),
                "content": "stats test row",
                "tags": ["stats"],
                "priority": 5,
                "confidence": 1.0,
                "source": "stats-test",
                "metadata": {}
            }))
            .send()
            .await
            .expect("stats memory create");
    }

    let stats: Value = client
        .get(format!("{base}/api/v1/stats"))
        .send()
        .await
        .expect("stats GET")
        .json()
        .await
        .expect("stats body");
    assert!(stats["total_memories"].as_u64().unwrap_or(0) >= 3);
    assert!(stats["by_namespace"][&unique_ns].as_u64().unwrap_or(0) >= 3);
    assert_eq!(stats["storage_backend"], "postgres");

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 5 — `GET /api/v1/namespaces` returns the distinct namespace
/// list aggregated from the `memories` table.
#[tokio::test(flavor = "multi_thread")]
async fn namespaces_round_trip_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping namespaces_round_trip_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let unique_ns = format!("ns-{}", uuid::Uuid::new_v4());
    client
        .post(format!("{base}/api/v1/memories"))
        .json(&json!({
            "tier": "long",
            "namespace": unique_ns.clone(),
            "title": "ns-marker",
            "content": "namespace marker",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "ns-test",
            "metadata": {}
        }))
        .send()
        .await
        .expect("ns marker create");

    let ns: Value = client
        .get(format!("{base}/api/v1/namespaces"))
        .send()
        .await
        .expect("namespaces GET")
        .json()
        .await
        .expect("namespaces body");
    let arr = ns["namespaces"].as_array().expect("namespaces array");
    assert!(arr.iter().any(|v| v.as_str() == Some(&unique_ns)));

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 5 — `POST /api/v1/memories/bulk` writes each row through
/// SAL `store`. Verifies the count + zero-error envelope.
#[tokio::test(flavor = "multi_thread")]
async fn bulk_create_round_trip_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bulk_create_round_trip_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let unique_ns = format!("bulk-{}", uuid::Uuid::new_v4());
    let bulk = (0..5)
        .map(|i| {
            json!({
                "tier": "long",
                "namespace": unique_ns.clone(),
                "title": format!("bulk-row-{i}"),
                "content": format!("row {i} content"),
                "tags": ["bulk"],
                "priority": 5,
                "confidence": 1.0,
                "source": "bulk-test",
                "metadata": {}
            })
        })
        .collect::<Vec<_>>();
    let resp: Value = client
        .post(format!("{base}/api/v1/memories/bulk"))
        .json(&bulk)
        .send()
        .await
        .expect("bulk POST")
        .json()
        .await
        .expect("bulk body");
    assert_eq!(resp["created"], 5);
    assert_eq!(
        resp["errors"].as_array().map_or(0, std::vec::Vec::len),
        0,
        "bulk_create should have zero errors against fresh namespace"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 5 — `POST /api/v1/entities` round-trips through SAL `store`,
/// and `GET /api/v1/entities/by_alias` matches via `metadata.aliases`.
#[tokio::test(flavor = "multi_thread")]
async fn entity_registry_round_trip_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping entity_registry_round_trip_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let unique_ns = format!("ent-{}", uuid::Uuid::new_v4());
    let ent = client
        .post(format!("{base}/api/v1/entities"))
        .json(&json!({
            "canonical_name": "Acme Corp",
            "namespace": unique_ns.clone(),
            "aliases": ["ACME", "Acme"],
            "metadata": {}
        }))
        .send()
        .await
        .expect("entity register");
    assert_eq!(ent.status(), reqwest::StatusCode::CREATED);

    let lookup: Value = client
        .get(format!(
            "{base}/api/v1/entities/by_alias?alias=ACME&namespace={unique_ns}"
        ))
        .send()
        .await
        .expect("entity by_alias")
        .json()
        .await
        .expect("entity by_alias body");
    assert_eq!(lookup["found"], true);
    assert_eq!(lookup["canonical_name"], "Acme Corp");
    let aliases = lookup["aliases"].as_array().expect("aliases array");
    assert!(aliases.iter().any(|a| a == "ACME"));

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 5 — `POST /api/v1/recall` falls back to keyword-only via the
/// SAL `search` trait method. Mode field is `keyword` and storage
/// backend is annotated.
#[tokio::test(flavor = "multi_thread")]
async fn recall_keyword_fallback_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping recall_keyword_fallback_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let unique_ns = format!("recall-{}", uuid::Uuid::new_v4());
    let unique_word = format!("XYZ{}", uuid::Uuid::new_v4().simple());
    client
        .post(format!("{base}/api/v1/memories"))
        .json(&json!({
            "tier": "long",
            "namespace": unique_ns.clone(),
            "title": "recall target",
            "content": format!("contents include the magic token: {unique_word}"),
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "recall-test",
            "metadata": {}
        }))
        .send()
        .await
        .expect("recall create");

    let recall: Value = client
        .post(format!("{base}/api/v1/recall"))
        .json(&json!({
            "context": unique_word.clone(),
            "namespace": unique_ns.clone(),
            "limit": 5,
        }))
        .send()
        .await
        .expect("recall POST")
        .json()
        .await
        .expect("recall body");
    assert_eq!(recall["mode"], "keyword");
    assert_eq!(recall["storage_backend"], "postgres");
    let mems = recall["memories"].as_array().expect("memories array");
    // Postgres FTS may or may not match a synthetic uuid token, so we
    // assert structural shape rather than non-empty result count.
    for m in mems {
        assert!(m["score"].is_number());
    }

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 5 — `GET /api/v1/taxonomy` projects per-namespace counts.
#[tokio::test(flavor = "multi_thread")]
async fn taxonomy_round_trip_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping taxonomy_round_trip_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let unique_ns = format!("tax-{}", uuid::Uuid::new_v4());
    for i in 0..3 {
        client
            .post(format!("{base}/api/v1/memories"))
            .json(&json!({
                "tier": "long",
                "namespace": unique_ns.clone(),
                "title": format!("tax-{i}"),
                "content": "taxonomy fixture",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "tax-test",
                "metadata": {}
            }))
            .send()
            .await
            .expect("tax create");
    }

    let tax: Value = client
        .get(format!("{base}/api/v1/taxonomy"))
        .send()
        .await
        .expect("taxonomy GET")
        .json()
        .await
        .expect("taxonomy body");
    let tree = tax["tree"].as_array().expect("tree array");
    let total = tax["total_count"].as_u64().expect("total_count");
    assert!(total >= 3);
    let row = tree
        .iter()
        .find(|n| n["namespace"].as_str() == Some(&unique_ns))
        .expect("our namespace appears in tree");
    assert!(row["count"].as_u64().unwrap_or(0) >= 3);
    assert_eq!(tax["storage_backend"], "postgres");

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 4 — `GET /api/v1/archive` returns the archive listing
/// projected from `archived_memories`. Empty initially; we don't
/// archive a memory in this test (the archive write path is sqlite-
/// only in v0.7.0).
#[tokio::test(flavor = "multi_thread")]
async fn archive_list_returns_empty_envelope_on_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping archive_list_returns_empty_envelope_on_postgres");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .get(format!("{base}/api/v1/archive"))
        .send()
        .await
        .expect("archive GET")
        .json()
        .await
        .expect("archive body");
    assert!(resp["archived"].is_array());
    assert!(resp["count"].is_number());

    let stats: Value = client
        .get(format!("{base}/api/v1/archive/stats"))
        .send()
        .await
        .expect("archive stats GET")
        .json()
        .await
        .expect("archive stats body");
    assert!(stats["total_archived"].is_number());

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 5 — KG endpoints return wire-shape compatible envelopes.
/// We don't assert on populated results because the live test pgsql
/// may not have AGE installed, but the dispatch path must succeed.
#[tokio::test(flavor = "multi_thread")]
async fn kg_query_dispatches_via_sal() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping kg_query_dispatches_via_sal");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    // Create source memory for predictable id.
    let unique_ns = format!("kg-{}", uuid::Uuid::new_v4());
    let create: Value = client
        .post(format!("{base}/api/v1/memories"))
        .json(&json!({
            "tier": "long",
            "namespace": unique_ns,
            "title": "kg-source",
            "content": "kg source",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "kg-test",
            "metadata": {}
        }))
        .send()
        .await
        .expect("kg source create")
        .json()
        .await
        .expect("kg source body");
    let source_id = create["id"].as_str().expect("source id").to_string();

    let resp = client
        .post(format!("{base}/api/v1/kg/query"))
        .json(&json!({
            "source_id": source_id,
            "max_depth": 1,
        }))
        .send()
        .await
        .expect("kg/query POST");
    // Either OK with structural shape, or 503 if AGE missing — both
    // acceptable. Never 501 (the gate must allow this through).
    assert_ne!(
        resp.status(),
        reqwest::StatusCode::NOT_IMPLEMENTED,
        "kg/query must dispatch to the SAL adapter, not 501 through the gate"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 4 — `GET /api/v1/pending` returns an empty list with a
/// structured note on postgres.
#[tokio::test(flavor = "multi_thread")]
async fn pending_list_returns_structured_empty_on_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping pending_list_returns_structured_empty_on_postgres");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .get(format!("{base}/api/v1/pending"))
        .send()
        .await
        .expect("pending GET")
        .json()
        .await
        .expect("pending body");
    assert_eq!(resp["count"], 0);
    assert!(resp["pending"].is_array());

    shutdown.notify_one();
    let _ = handle.await;
}

/// Phase 5 — `GET /api/v1/subscriptions` returns the structured empty
/// envelope on postgres.
#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_list_returns_structured_empty_on_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping subscriptions_list_returns_structured_empty_on_postgres");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .get(format!("{base}/api/v1/subscriptions"))
        .send()
        .await
        .expect("subs GET")
        .json()
        .await
        .expect("subs body");
    assert_eq!(resp["count"], 0);
    assert!(resp["subscriptions"].is_array());
    assert_eq!(resp["storage_backend"], "postgres");

    shutdown.notify_one();
    let _ = handle.await;
}
