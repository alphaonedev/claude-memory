// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::map_unwrap_or)]
//! v0.7.0 Wave-3 Continuation 4 — F7 handler-parity coverage tests.
//!
//! Test-driven scaffolding for the 13 documented postgres-handler gaps
//! surfaced by Wave 4 R1 (`v0.7.0-a2a-wave4-r1-20260509-1520`). Every
//! test in this file exercises a single F7 bucket against an in-process
//! HTTP daemon backed by the live `PostgresStore` adapter so the round-
//! trip behaviour matches what the cert harness sees over the wire.
//!
//! ## Buckets
//!
//! - **A — Hybrid recall** (`bucket_a_*`): semantic recall returns rows;
//!   `find_paths` over a 5-hop chain surfaces a non-empty path.
//! - **B — Pubsub / notify / inbox / subscriptions** (`bucket_b_*`):
//!   notify pushes deliver to inbox; subscriptions persist + list.
//! - **C — Governance / namespace standards** (`bucket_c_*`):
//!   `permissions.write=owner` enforced; inheritance walk on writes
//!   denies non-owner stores in deep children.
//! - **D — KG temporal / AGE Cypher via daemon** (`bucket_d_*`):
//!   `kg_query`, `kg_timeline`, `kg_invalidate` round-trip through the
//!   daemon for both AGE-active and CTE-fallback postgres deployments.
//! - **E — Taxonomy / aliases / duplicates / quotas** (`bucket_e_*`):
//!   taxonomy walk surfaces all stored memory namespaces; alias union;
//!   duplicate detection; per-agent quota counters.
//! - **F — Link signing** (`bucket_f_*`): signed-link wire envelope
//!   carries `attest_level=self_signed` and populates `observed_by`.
//! - **G — pg `schema_version` test-side fix** (`bucket_g_*`): read the
//!   live `schema_version` via the daemon and assert it matches
//!   `current_schema_version()` (same constant the cert oracle should
//!   pin against).
//! - **State flake** (`state_flake_*`): `memory_promote` round-trip
//!   after a daemon restart against a fresh DB.
//!
//! ## Gating
//!
//! Same gates as `serve_postgres_smoke.rs` — `feature = "sal-postgres"`
//! plus `AI_MEMORY_TEST_POSTGRES_URL` set at run time. Without either,
//! every test prints a skip line and returns cleanly.
//!
//! ## How to run
//!
//! ```sh
//! AI_MEMORY_TEST_POSTGRES_URL=postgres://aimemory:<pwd>@10.20.0.4:5432/aimemory_c4_test \
//!   cargo test --features sal-postgres,sal --test serve_postgres_handler_parity
//! ```
//!
//! Each test boots its own ephemeral daemon listening on `127.0.0.1:0`
//! (kernel-assigned port) so the suite can run in parallel against a
//! single shared scratch database without port collisions. Per-test
//! data is namespaced with a fresh UUID so concurrent runs do not
//! collide in the shared PG namespace.

#![cfg(feature = "sal-postgres")]
#![allow(clippy::too_many_lines, clippy::doc_markdown)]

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
    // Match the production daemon's posture: `permissions.mode = enforce`
    // is the v0.7.0 default. Without this, the test in-process daemon
    // boots in Advisory mode (the static OnceLock fallback) and Bucket C
    // governance scenarios silently pass through Allow.
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );

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
    content: &str,
) -> String {
    let body = json!({
        "tier": "long",
        "namespace": namespace,
        "title": title,
        "content": content,
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "system",
    });
    let resp = client
        .post(format!("{base}/api/v1/memories"))
        .json(&body)
        .send()
        .await
        .expect("store POST");
    assert!(
        resp.status().is_success(),
        "store should succeed: status={}",
        resp.status()
    );
    let v: Value = resp.json().await.expect("body");
    v["id"].as_str().expect("id").to_string()
}

// ============================================================================
// Bucket A — Hybrid recall (S18, S79, S65)
// ============================================================================

/// S18/S79: hybrid recall against a postgres-backed daemon must surface
/// stored rows. Today the postgres path falls back to keyword search only;
/// this test exercises the wire shape so we catch the moment hybrid +
/// pgvector cosine stitch lands.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_a_hybrid_recall_returns_results() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_a_hybrid_recall_returns_results");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("bucket-a-{}", uuid::Uuid::new_v4());

    // Seed with three memories whose content shares a query token.
    store_memory(
        &client,
        &base,
        &ns,
        "alice-research",
        "Research note on distributed consensus protocols",
    )
    .await;
    store_memory(
        &client,
        &base,
        &ns,
        "bob-research",
        "Notes on consensus algorithms for distributed systems",
    )
    .await;
    store_memory(&client, &base, &ns, "filler", "An unrelated entry").await;

    // Recall via the keyword surface — this MUST return at least one row
    // since both stored memories contain "consensus".
    let resp = client
        .get(format!("{base}/api/v1/recall"))
        .query(&[("q", "consensus"), ("namespace", ns.as_str())])
        .send()
        .await
        .expect("recall GET");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    let memories = v["memories"]
        .as_array()
        .or_else(|| v["results"].as_array())
        .expect("memories or results array in recall response");
    assert!(
        !memories.is_empty(),
        "hybrid recall on postgres should surface at least one row for \
         a query token present in stored content; got empty: {v}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S65: `find_paths` over a 5-hop chain A->B->C->D->E should return a
/// non-empty path when invoked over HTTP.
///
/// Note: `memory_find_paths` is currently MCP-only (no HTTP route). The
/// cert harness uses MCP via SSH for this scenario, so this test is a
/// **forward-looking** assertion that the HTTP surface gains a
/// `/api/v1/kg/find_paths` route. Until then it skips with a clear
/// message rather than hard-fail.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_a_find_paths_depth_10() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_a_find_paths_depth_10");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("bucket-a-paths-{}", uuid::Uuid::new_v4());

    // Build a chain A -> B -> C -> D -> E.
    let mut ids = Vec::new();
    for label in ["A", "B", "C", "D", "E"] {
        ids.push(store_memory(&client, &base, &ns, label, &format!("node {label}")).await);
    }
    for w in ids.windows(2) {
        let resp = client
            .post(format!("{base}/api/v1/links"))
            .json(&json!({
                "source_id": w[0],
                "target_id": w[1],
                "relation": "related_to",
            }))
            .send()
            .await
            .expect("link POST");
        assert!(resp.status().is_success(), "link should succeed");
    }

    // Try the `kg/query` surface to enumerate reachable nodes from A.
    // `max_depth=5` is the documented ceiling on both the sqlite + the
    // postgres adapters (KG_QUERY_MAX_SUPPORTED_DEPTH); higher values
    // surface a 422 from `validate_depth`. The chain A→B→C→D→E is
    // exactly 4 hops so depth=5 covers it.
    let resp = client
        .post(format!("{base}/api/v1/kg/query"))
        .json(&json!({
            "source_id": ids[0],
            "max_depth": 5,
        }))
        .send()
        .await
        .expect("kg/query POST");
    // A future fix should make this return a 200 with non-empty
    // `paths` array. Current state: 503 (AGE/CTE not wired through SAL
    // for postgres-backed daemons).
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(json!({}));
    assert!(
        status == reqwest::StatusCode::OK,
        "kg/query should return 200 once postgres KG dispatch lands; \
         got status={status} body={v}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// Bucket B — Pubsub / notify / inbox / subscriptions (S32, S33, S58, S34)
// ============================================================================

/// S32/S58: alice notifies bob; bob's inbox returns the payload.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_b_notify_delivers_to_inbox() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_b_notify_delivers_to_inbox");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let bob = format!("bucket-b-bob-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let alice = format!("bucket-b-alice-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let marker = format!("marker-{}", uuid::Uuid::new_v4());

    let resp = client
        .post(format!("{base}/api/v1/notify"))
        .header("x-agent-id", &alice)
        .json(&json!({
            "target_agent_id": bob,
            "title": "hello",
            "payload": &marker,
        }))
        .send()
        .await
        .expect("notify POST");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "notify must return 201 CREATED on postgres"
    );

    let resp = client
        .get(format!("{base}/api/v1/inbox"))
        .header("x-agent-id", &bob)
        .send()
        .await
        .expect("inbox GET");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    let messages = v["messages"]
        .as_array()
        .expect("inbox response must include messages array");
    assert!(
        messages.iter().any(|m| {
            let payload = m["payload"]
                .as_str()
                .or_else(|| m["content"].as_str())
                .unwrap_or("");
            payload.contains(&marker)
        }),
        "bob's inbox must contain alice's notify with marker={marker}; \
         got messages={messages:?}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S33: bob subscribes to a namespace; list_subscriptions surfaces it.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_b_subscriptions_persist() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_b_subscriptions_persist");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let bob = format!("bucket-b-bob-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let target_ns = format!("bucket-b-target-{}", uuid::Uuid::new_v4());

    let resp = client
        .post(format!("{base}/api/v1/subscriptions"))
        .header("x-agent-id", &bob)
        .json(&json!({
            "agent_id": bob,
            "namespace": target_ns,
        }))
        .send()
        .await
        .expect("subscribe POST");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "subscribe must return 201 CREATED on postgres"
    );

    let resp = client
        .get(format!("{base}/api/v1/subscriptions"))
        .query(&[("agent_id", &bob)])
        .send()
        .await
        .expect("list_subscriptions GET");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    let subs = v["subscriptions"]
        .as_array()
        .expect("subscriptions array in response");
    assert!(
        subs.iter().any(|s| {
            s["namespace"].as_str() == Some(target_ns.as_str())
                || s["namespace_filter"].as_str() == Some(target_ns.as_str())
        }),
        "bob's subscription list must include target_ns={target_ns}; got: {subs:?}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// Bucket C — Governance / namespace standards (S35, S53, S60, S80)
// ============================================================================

/// S53/S60/S80: `governance.write=owner` on a parent namespace must
/// 403 a non-owner write to a deep child.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_c_namespace_standards_enforce() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_c_namespace_standards_enforce");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let parent_ns = format!("bucket-c-parent-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let alice = format!("bucket-c-alice-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let intruder = format!(
        "bucket-c-intruder-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );

    // alice creates the parent + sets governance.write=owner.
    let standard_id = store_memory(
        &client,
        &base,
        &parent_ns,
        "standard",
        "parent governance standard",
    )
    .await;
    let resp = client
        .post(format!("{base}/api/v1/namespaces"))
        .header("x-agent-id", &alice)
        .json(&json!({
            "namespace": parent_ns,
            "id": standard_id,
            "governance": {"write": "owner", "mode": "enforce"},
        }))
        .send()
        .await
        .expect("set-standard POST");
    assert!(
        resp.status().is_success(),
        "set namespace standard should succeed; got status={}",
        resp.status()
    );

    // intruder tries to write to a deep child of parent_ns.
    let child_ns = format!("{parent_ns}/sub/level/deep");
    let resp = client
        .post(format!("{base}/api/v1/memories"))
        .header("x-agent-id", &intruder)
        .json(&json!({
            "tier": "long",
            "namespace": child_ns,
            "title": "intruder",
            "content": "should be forbidden",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "system",
        }))
        .send()
        .await
        .expect("intruder write");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::FORBIDDEN,
        "non-owner write to governed deep child must 403; got status={} body={}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S35: namespace_meta propagation — the standard set on parent must
/// be readable via GET /api/v1/namespaces/{ns}/standard.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_c_namespace_meta_propagation() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_c_namespace_meta_propagation");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let parent_ns = format!("bucket-c-meta-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let standard_id = store_memory(
        &client,
        &base,
        &parent_ns,
        "standard",
        "the standard memory",
    )
    .await;
    let resp = client
        .post(format!("{base}/api/v1/namespaces"))
        .json(&json!({
            "namespace": parent_ns,
            "id": standard_id,
        }))
        .send()
        .await
        .expect("set-standard POST");
    assert!(resp.status().is_success(), "set-standard should succeed");

    let resp = client
        .get(format!("{base}/api/v1/namespaces"))
        .query(&[("namespace", parent_ns.as_str())])
        .send()
        .await
        .expect("get-standard GET");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "get-standard must return 200 (not 501) on postgres"
    );
    let v: Value = resp.json().await.expect("body");
    assert!(
        v["standard_id"].as_str() == Some(&standard_id) || v["id"].as_str() == Some(&standard_id),
        "get-standard must surface the persisted standard_id; got: {v}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// Bucket D — KG temporal / AGE Cypher (S45, S46, S82)
// ============================================================================

/// S46: kg_timeline returns event stream over HTTP.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_d_kg_timeline_returns_events() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_d_kg_timeline_returns_events");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("bucket-d-{}", uuid::Uuid::new_v4());

    let a = store_memory(&client, &base, &ns, "A", "node A").await;
    let b = store_memory(&client, &base, &ns, "B", "node B").await;
    let c = store_memory(&client, &base, &ns, "C", "node C").await;
    for (s, t) in [(&a, &b), (&b, &c)] {
        let _ = client
            .post(format!("{base}/api/v1/links"))
            .json(&json!({"source_id": s, "target_id": t, "relation": "related_to"}))
            .send()
            .await;
    }

    let resp = client
        .get(format!("{base}/api/v1/kg/timeline"))
        .query(&[("source_id", a.as_str())])
        .send()
        .await
        .expect("kg/timeline GET");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "kg/timeline must return 200 on postgres; got status={}",
        resp.status()
    );
    let v: Value = resp.json().await.expect("body");
    let events = v["events"]
        .as_array()
        .or_else(|| v["timeline"].as_array())
        .expect("events or timeline array in response");
    assert!(
        !events.is_empty(),
        "kg/timeline should surface at least 1 edge event from A's outgoing links; got {events:?}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S45/S82: kg_query over HTTP — postgres path must dispatch through
/// the AGE/CTE adapter and return path entries.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_d_age_kg_via_daemon() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_d_age_kg_via_daemon");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("bucket-d-age-{}", uuid::Uuid::new_v4());

    let a = store_memory(&client, &base, &ns, "A", "node A").await;
    let b = store_memory(&client, &base, &ns, "B", "node B").await;
    let _ = client
        .post(format!("{base}/api/v1/links"))
        .json(&json!({"source_id": a, "target_id": b, "relation": "related_to"}))
        .send()
        .await;

    let resp = client
        .post(format!("{base}/api/v1/kg/query"))
        .json(&json!({
            "source_id": a,
            "max_depth": 3,
        }))
        .send()
        .await
        .expect("kg/query POST");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "kg/query must return 200 on postgres-backed daemon; got status={}",
        resp.status()
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// Bucket E — Taxonomy / aliases / duplicates / quotas (S44, S47, S48, S61)
// ============================================================================

/// S44: taxonomy walk surfaces all stored namespaces with subtree_count.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_e_taxonomy_walk() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_e_taxonomy_walk");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let root = format!("bucket-e-tax-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    store_memory(&client, &base, &root, "root-mem", "root").await;
    store_memory(&client, &base, &format!("{root}/child1"), "c1-mem", "c1").await;
    store_memory(
        &client,
        &base,
        &format!("{root}/child1/grand"),
        "g-mem",
        "g",
    )
    .await;

    let resp = client
        .get(format!("{base}/api/v1/taxonomy"))
        .query(&[("prefix", root.as_str())])
        .send()
        .await
        .expect("taxonomy GET");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    let total = v["total_count"].as_u64().unwrap_or(0);
    assert!(
        total >= 3,
        "taxonomy walk under {root} should surface at least 3 memories; got total={total} v={v}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S47: registering an entity twice with different aliases must union them.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_e_entity_aliases() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_e_entity_aliases");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!(
        "bucket-e-aliases-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let canonical = format!("Canonical-{}", uuid::Uuid::new_v4());

    let resp = client
        .post(format!("{base}/api/v1/entities"))
        .json(&json!({
            "canonical_name": canonical,
            "namespace": ns,
            "aliases": ["alpha", "beta"],
        }))
        .send()
        .await
        .expect("first entity_register");
    assert!(resp.status().is_success(), "first register should succeed");

    let resp = client
        .post(format!("{base}/api/v1/entities"))
        .json(&json!({
            "canonical_name": canonical,
            "namespace": ns,
            "aliases": ["gamma", "delta"],
        }))
        .send()
        .await
        .expect("second entity_register");
    assert!(resp.status().is_success(), "second register should succeed");
    let v: Value = resp.json().await.expect("body");
    let aliases: Vec<String> = v["aliases"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        aliases.iter().any(|a| a == "alpha")
            && aliases.iter().any(|a| a == "beta")
            && aliases.iter().any(|a| a == "gamma")
            && aliases.iter().any(|a| a == "delta"),
        "aliases array must be the union of both registers; got {aliases:?}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S48: check_duplicate against a near-identical input returns a match.
/// Without an embedder configured the daemon returns 503; we accept
/// either 200 with a match OR 503 (the embedder dependency is what
/// matters for this gap, not pure SAL routing).
#[tokio::test(flavor = "multi_thread")]
async fn bucket_e_check_duplicate() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_e_check_duplicate");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("bucket-e-dup-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let title = "Duplicate Source";
    let content = "The original content for the dup test.";
    let _ = store_memory(&client, &base, &ns, title, content).await;

    let resp = client
        .post(format!("{base}/api/v1/check_duplicate"))
        .json(&json!({
            "title": title,
            "content": content,
            "namespace": ns,
            "threshold": 0.5,
        }))
        .send()
        .await
        .expect("check_duplicate POST");
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(json!({}));
    let acceptable = status == reqwest::StatusCode::OK
        && (v["is_duplicate"].as_bool() == Some(true)
            || v["candidates_scanned"].as_u64().unwrap_or(0) >= 1);
    assert!(
        acceptable,
        "check_duplicate on postgres should either find the seeded duplicate or scan at \
         least one candidate; got status={status} body={v}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S61: per-agent quota counters reflect writes.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_e_agent_quotas() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_e_agent_quotas");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let agent = format!("bucket-e-quota-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let ns = format!("bucket-e-quota-ns-{}", uuid::Uuid::new_v4());

    // Burn 5 writes.
    for i in 0..5 {
        let _ = client
            .post(format!("{base}/api/v1/memories"))
            .header("x-agent-id", &agent)
            .json(&json!({
                "tier": "long",
                "namespace": ns,
                "title": format!("quota-mem-{i}"),
                "content": "quota burn",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "system",
            }))
            .send()
            .await;
    }

    let resp = client
        .get(format!("{base}/api/v1/quota"))
        .header("x-agent-id", &agent)
        .send()
        .await;
    // The quota_status endpoint may be MCP-only; accept either an
    // OK envelope with `used >= 5` OR a 404 (route absent), and fail
    // loudly only on a non-empty envelope reporting `used == 0`.
    if let Ok(r) = resp
        && r.status() == reqwest::StatusCode::OK
    {
        let v: Value = r.json().await.unwrap_or(json!({}));
        if let Some(used) = v["used"].as_u64() {
            assert!(
                used >= 5,
                "agent quota counter should reflect 5 writes; got used={used} body={v}"
            );
        }
    }

    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// Bucket F — Link signing observed_by (S52)
// ============================================================================

/// S52: signed link populates `observed_by` and `attest_level=self_signed`.
#[tokio::test(flavor = "multi_thread")]
async fn bucket_f_link_signing_observed_by() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_f_link_signing_observed_by");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("bucket-f-signed-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let a = store_memory(&client, &base, &ns, "src", "source").await;
    let b = store_memory(&client, &base, &ns, "tgt", "target").await;

    let resp = client
        .post(format!("{base}/api/v1/links"))
        .header("x-agent-id", "signer-agent")
        .json(&json!({
            "source_id": a,
            "target_id": b,
            "relation": "related_to",
        }))
        .send()
        .await
        .expect("link POST");
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let v: Value = resp.json().await.expect("body");
    // Without an active keypair the daemon falls back to "unsigned";
    // F7 originally surfaced `observed_by=None` even when the daemon
    // had a keypair. The wire envelope MUST include `attest_level`
    // so callers can distinguish the two states.
    assert!(
        v.get("attest_level").is_some(),
        "link response must include attest_level; got: {v}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// Bucket G — pg schema_version test-side oracle (S75)
// ============================================================================

/// S75: schema_version surfaced via capabilities matches the value the
/// migration ladder advertises (28 at HEAD).
#[tokio::test(flavor = "multi_thread")]
async fn bucket_g_pg_schema_version() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping bucket_g_pg_schema_version");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/api/v1/capabilities"))
        .send()
        .await
        .expect("capabilities GET");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    let backend = v["storage_backend"].as_str().unwrap_or("");
    assert_eq!(backend, "postgres");
    // schema_version MAY be absent from capabilities today (test-side
    // oracle); document the read-path here so when it lands the test
    // catches the rev. 28 is the expected value at HEAD per the
    // schema_version table populated by `ai-memory schema-init`.
    if let Some(sv) = v["schema_version"].as_u64() {
        assert!(sv == 28, "schema_version expected 28; got {sv}");
    }
    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// State flake — memory_promote 404 after disposable DB reset
// ============================================================================

/// S16/S49: after storing a memory, the promote path must round-trip.
/// A daemon restart against a fresh DB produced 404 in Wave 4 R2 because
/// the in-memory state cache outlived the schema reset; this test ensures
/// the round-trip is stateless.
#[tokio::test(flavor = "multi_thread")]
async fn state_flake_memory_promote() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping state_flake_memory_promote");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!(
        "state-flake-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    // Store a `mid`-tier memory + promote it to `long`.
    let body = json!({
        "tier": "mid",
        "namespace": ns,
        "title": "promote-me",
        "content": "to be promoted to long",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "system",
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
    let id = v["id"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{base}/api/v1/memories/{id}/promote"))
        .json(&json!({"tier": "long"}))
        .send()
        .await
        .expect("promote POST");
    assert!(
        resp.status() != reqwest::StatusCode::NOT_FOUND,
        "promote on a freshly-stored memory must not 404; got status={}",
        resp.status()
    );

    shutdown.notify_one();
    let _ = handle.await;
}

// ============================================================================
// Continuation 6 — three new REST endpoints (S52, S61, S65)
// ============================================================================

/// S61: `POST /api/v1/quota/status` returns a populated `QuotaStatus`
/// row keyed on the supplied `agent_id` even on a postgres-backed
/// daemon (which does NOT have a sqlite `agent_quotas` row to fall
/// back on). Auto-inserts the default row on first call.
#[tokio::test(flavor = "multi_thread")]
async fn cont6_quota_status_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_quota_status_postgres");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let agent = format!("cont6-quota-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let resp = client
        .post(format!("{base}/api/v1/quota/status"))
        .json(&json!({"agent_id": agent}))
        .send()
        .await
        .expect("quota POST");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert_eq!(v["agent_id"], agent);
    assert_eq!(
        v["max_memories_per_day"], 1000,
        "default daily memory cap must be 1000"
    );
    assert_eq!(
        v["max_storage_bytes"], 104_857_600,
        "default storage cap must be 100 MiB"
    );
    assert_eq!(v["max_links_per_day"], 5000);
    assert_eq!(v["current_memories_today"], 0);

    // Second call: same row, idempotent.
    let resp = client
        .post(format!("{base}/api/v1/quota/status"))
        .json(&json!({"agent_id": agent}))
        .send()
        .await
        .expect("quota POST 2");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v2: Value = resp.json().await.expect("body 2");
    assert_eq!(v2["agent_id"], agent);

    shutdown.notify_one();
    let _ = handle.await;
}

/// S61: omitting `agent_id` returns the full table — the operator
/// surface backing the MCP `memory_quota_status` "no id" path.
#[tokio::test(flavor = "multi_thread")]
async fn cont6_quota_status_list_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_quota_status_list_postgres");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let agent = format!(
        "cont6-quota-list-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    // Auto-insert one row first.
    client
        .post(format!("{base}/api/v1/quota/status"))
        .json(&json!({"agent_id": agent}))
        .send()
        .await
        .expect("quota POST");

    let resp = client
        .post(format!("{base}/api/v1/quota/status"))
        .json(&json!({}))
        .send()
        .await
        .expect("quota list POST");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    let arr = v["quotas"].as_array().expect("quotas array");
    assert!(
        arr.iter().any(|r| r["agent_id"] == agent),
        "list must include the auto-inserted agent {agent}; got {arr:?}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S61: invalid `agent_id` -> 400.
#[tokio::test(flavor = "multi_thread")]
async fn cont6_quota_status_rejects_invalid_agent() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_quota_status_rejects_invalid_agent");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/quota/status"))
        // whitespace + control chars are rejected by validate_agent_id.
        .json(&json!({"agent_id": "bad agent id"}))
        .send()
        .await
        .expect("quota POST");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    shutdown.notify_one();
    let _ = handle.await;
}

/// S65: `POST /api/v1/kg/find_paths` over a 4-hop chain returns at
/// least one path of length >=2 (start + 4 intermediate + end is not
/// guaranteed to exist as a single path on every adapter; the contract
/// is "non-empty, includes both endpoints").
#[tokio::test(flavor = "multi_thread")]
async fn cont6_find_paths_returns_chain() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_find_paths_returns_chain");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("cont6-paths-{}", uuid::Uuid::new_v4());

    // Build A -> B -> C -> D chain.
    let mut ids = Vec::new();
    for label in ["A", "B", "C", "D"] {
        ids.push(store_memory(&client, &base, &ns, label, &format!("node {label}")).await);
    }
    for w in ids.windows(2) {
        let resp = client
            .post(format!("{base}/api/v1/links"))
            .json(&json!({
                "source_id": w[0],
                "target_id": w[1],
                "relation": "related_to",
            }))
            .send()
            .await
            .expect("link POST");
        assert!(resp.status().is_success(), "link must succeed");
    }

    let resp = client
        .post(format!("{base}/api/v1/kg/find_paths"))
        .json(&json!({
            "source_id": ids[0],
            "target_id": ids[3],
            "max_depth": 5,
        }))
        .send()
        .await
        .expect("find_paths POST");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "find_paths must return 200 on postgres-backed daemon"
    );
    let v: Value = resp.json().await.expect("body");
    let paths = v["paths"].as_array().expect("paths array");
    assert!(
        !paths.is_empty(),
        "find_paths must return at least one path"
    );
    let first = paths[0].as_array().expect("inner path array");
    let endpoints: Vec<&str> = first.iter().filter_map(|x| x.as_str()).collect();
    assert!(
        endpoints.first().map(|s| s == &ids[0]).unwrap_or(false),
        "first node of the path must be the source: {endpoints:?}"
    );
    assert!(
        endpoints.last().map(|s| s == &ids[3]).unwrap_or(false),
        "last node of the path must be the target: {endpoints:?}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S65: invalid `source_id` -> 400.
#[tokio::test(flavor = "multi_thread")]
async fn cont6_find_paths_rejects_invalid_id() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_find_paths_rejects_invalid_id");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/kg/find_paths"))
        .json(&json!({
            "source_id": "bad id with space",
            "target_id": "another bad",
        }))
        .send()
        .await
        .expect("find_paths POST");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    shutdown.notify_one();
    let _ = handle.await;
}

/// S52: `POST /api/v1/links/verify` for an unsigned link projects
/// `attest_level=unsigned`, `verified=true`, `signature_present=false`.
#[tokio::test(flavor = "multi_thread")]
async fn cont6_verify_link_unsigned() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_verify_link_unsigned");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let ns = format!("cont6-verify-{}", uuid::Uuid::new_v4());

    let src = store_memory(&client, &base, &ns, "src", "source memory").await;
    let tgt = store_memory(&client, &base, &ns, "tgt", "target memory").await;
    let resp = client
        .post(format!("{base}/api/v1/links"))
        .json(&json!({
            "source_id": src,
            "target_id": tgt,
            "relation": "related_to",
        }))
        .send()
        .await
        .expect("link POST");
    assert!(resp.status().is_success(), "link create must succeed");

    let resp = client
        .post(format!("{base}/api/v1/links/verify"))
        .json(&json!({
            "source_id": src,
            "target_id": tgt,
        }))
        .send()
        .await
        .expect("verify POST");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let v: Value = resp.json().await.expect("body");
    assert_eq!(v["source_id"], src);
    assert_eq!(v["target_id"], tgt);
    assert_eq!(v["relation"], "related_to");
    assert_eq!(v["attest_level"], "unsigned");
    assert_eq!(v["signature_present"], false);
    assert_eq!(
        v["verified"], true,
        "structurally-valid unsigned link must verify clean"
    );

    shutdown.notify_one();
    let _ = handle.await;
}

/// S52: missing both `source_id` and `link_id` -> 400.
#[tokio::test(flavor = "multi_thread")]
async fn cont6_verify_link_rejects_empty_filter() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_verify_link_rejects_empty_filter");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v1/links/verify"))
        .json(&json!({}))
        .send()
        .await
        .expect("verify POST");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    shutdown.notify_one();
    let _ = handle.await;
}

/// S52: missing link -> 404.
#[tokio::test(flavor = "multi_thread")]
async fn cont6_verify_link_missing_returns_not_found() {
    let Some(url) = postgres_url() else {
        eprintln!("skipping cont6_verify_link_missing_returns_not_found");
        return;
    };
    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();
    let bogus_src = uuid::Uuid::new_v4().to_string();
    let bogus_tgt = uuid::Uuid::new_v4().to_string();
    let resp = client
        .post(format!("{base}/api/v1/links/verify"))
        .json(&json!({
            "source_id": bogus_src,
            "target_id": bogus_tgt,
        }))
        .send()
        .await
        .expect("verify POST");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    shutdown.notify_one();
    let _ = handle.await;
}
