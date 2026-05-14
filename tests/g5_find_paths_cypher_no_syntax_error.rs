// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_lazy_continuation, clippy::too_many_lines)]
//! v0.7.0.1 G5 — postgres `find_paths_cypher` must compile cleanly
//! against AGE 1.5.0's `convert_cypher_to_subquery` analyzer.
//!
//! Reproducer for the G5 finding surfaced by R1b against the live cert
//! droplets:
//!
//! ```text
//! ERROR ai_memory::handlers: store backend error: backend unavailable:
//!   postgres: cypher find_paths: error returned from database:
//!   syntax error at or near "|"
//! ```
//!
//! The pre-fix Cypher was:
//!
//! ```cypher
//! MATCH p = (a)-[*..N]-(b)
//! WHERE a.id = $start_id AND b.id = $target_id
//! RETURN [n IN nodes(p) | properties(n).id] AS path
//! ORDER BY length(p) ASC
//! LIMIT N
//! ```
//!
//! AGE 1.5.0's parser supports list comprehensions (`[var IN list | expr]`)
//! in isolation, but `convert_cypher_to_subquery` mishandles the shape when
//! the iteration variable is bound from a function call (`nodes(p)`) and
//! the projection touches a property — the analyzer's recovery point at
//! `|` bubbles up as a Postgres-grammar `syntax error at or near "|"`
//! before the planner ever sees the Cypher body. The handler converts
//! that error to a 503 `storage backend unavailable`.
//!
//! ## What this test asserts
//!
//! 1. Boot an in-process HTTP daemon backed by [`PostgresStore`] against
//!    a database with Apache AGE 1.5.0 enabled (so the SAL dispatcher's
//!    `kg_backend = Age` branch lights up).
//! 2. Seed a 5-node chain A→B→C→D→E through `POST /api/v1/memories`
//!    + `POST /api/v1/links` — same wire shape S65 uses; the G4 fix
//!    self-projects each link into the `memory_graph` AGE projection so
//!    the corpus is in place by the time `find_paths` runs.
//! 3. POST `{source_id: A, target_id: E, max_depth: 3}` to
//!    `/api/v1/kg/find_paths` and assert a 200 with a non-empty `paths`
//!    array. Pre-G5 the daemon returned 503 with the syntax-error wire
//!    shape above.
//!
//! ## Gating
//!
//! Same as `g4_postgres_link_projects_into_age_graph.rs` —
//! `feature = "sal-postgres"` plus `AI_MEMORY_TEST_AGE_URL`
//! (or `AI_MEMORY_TEST_POSTGRES_URL` — fallback) must point at a
//! database with the `age` extension. Without either, the test prints
//! a skip line and returns cleanly.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::store::MemoryStore;
use ai_memory::store::postgres::PostgresStore;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock};

fn age_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_AGE_URL")
        .ok()
        .or_else(|| std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok())
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
) -> String {
    let body = json!({
        "tier": "long",
        "namespace": namespace,
        "title": title,
        "content": format!("g5 reproducer node {title}"),
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
        "store should succeed: status={} body={:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let v: Value = resp.json().await.expect("body");
    v["id"].as_str().expect("id").to_string()
}

/// G5 reproducer — `POST /api/v1/kg/find_paths` over a 5-node chain
/// must return a 200 with a non-empty `paths` array on a postgres-AGE
/// daemon. Pre-fix this surfaces as a 503 `syntax error at or near "|"`
/// because the Cypher list-comprehension form fails AGE 1.5.0's analyzer.
#[tokio::test(flavor = "multi_thread")]
async fn g5_find_paths_cypher_compiles_against_age_1_5() {
    let Some(url) = age_url() else {
        eprintln!(
            "skipping g5_find_paths_cypher_compiles_against_age_1_5: \
             AI_MEMORY_TEST_AGE_URL / AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    // Probe the connection's `kg_backend`; the CTE fallback doesn't
    // exhibit G5 because there's no Cypher to compile against.
    let store = PostgresStore::connect(&url)
        .await
        .expect("connect postgres adapter");
    if !matches!(store.kg_backend(), ai_memory::store::KgBackend::Age) {
        eprintln!(
            "skipping g5_find_paths_cypher_compiles_against_age_1_5: \
             kg_backend != Age (no AGE extension on this fixture)"
        );
        return;
    }
    drop(store);

    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    // Per-test namespace so concurrent runs against a shared scratch DB
    // don't reuse memory ids.
    let suffix = uuid::Uuid::new_v4();
    let ns = format!("g5-find-paths-cypher-{suffix}");

    // Build chain A→B→C→D→E in `memories` + `memory_links`. The G4 fix
    // mirrors each link write into the AGE `memory_graph` projection so
    // the corpus is in place by the time `find_paths` runs.
    let mut ids = Vec::new();
    for label in ["A", "B", "C", "D", "E"] {
        ids.push(store_memory(&client, &base, &ns, &format!("g5-{suffix}-{label}")).await);
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
        assert!(
            resp.status().is_success(),
            "link should succeed: status={} body={:?}",
            resp.status(),
            resp.text().await.ok()
        );
    }

    // Wire shape lifted from the live R1b reproducer:
    // `{source_id, target_id, max_depth: 3}` against the 4-hop chain.
    // depth=3 only covers an A→B→C→D path which is intentional — the
    // assertion is "no 503", not "every path is enumerated"; the longer
    // 4-hop A→…→E path stays out so we can prove the depth knob round-
    // trips the handler boundary as well as the cypher analyzer.
    let resp = client
        .post(format!("{base}/api/v1/kg/find_paths"))
        .json(&json!({
            "source_id": ids[0],
            "target_id": ids[3],
            "max_depth": 3,
            "max_results": 16,
        }))
        .send()
        .await
        .expect("find_paths POST");

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(json!({}));

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "G5: find_paths must return 200 from a postgres-AGE daemon — \
         pre-fix this surfaces as 503 with `cypher find_paths: \
         syntax error at or near \"|\"` because the list-comprehension \
         RETURN form fails AGE 1.5.0's `convert_cypher_to_subquery` \
         analyzer; got status={status} body={body}."
    );

    let paths = body["paths"].as_array().expect("paths must be an array");
    assert!(
        !paths.is_empty(),
        "G5: find_paths must surface at least one path through the \
         A→B→C→D chain at max_depth=3 once the cypher compiles; got \
         empty: {body}"
    );

    // First (shortest) path should be the 4-node A→B→C→D chain.
    let first: Vec<&str> = paths[0]
        .as_array()
        .expect("path[0] is an array")
        .iter()
        .map(|v| v.as_str().expect("path[0] entries are strings"))
        .collect();
    assert_eq!(
        first.len(),
        4,
        "shortest path through A→B→C→D must be 4 nodes long: {first:?}"
    );
    assert_eq!(first[0], ids[0]);
    assert_eq!(first[3], ids[3]);

    // Second 200-pass over the full 4-hop chain (A→…→E at max_depth=7)
    // — same wire shape the R1b reproducer captured and the smoke a
    // 4-hop traversal at the published ceiling depth.
    let resp = client
        .post(format!("{base}/api/v1/kg/find_paths"))
        .json(&json!({
            "source_id": ids[0],
            "target_id": ids[ids.len() - 1],
            "max_depth": 7,
            "max_results": 16,
        }))
        .send()
        .await
        .expect("find_paths POST (4-hop)");

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(json!({}));
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "G5: find_paths must return 200 for the 4-hop chain at depth=7; \
         got status={status} body={body}"
    );
    let paths = body["paths"].as_array().expect("paths must be an array");
    assert!(
        !paths.is_empty(),
        "G5: find_paths must surface at least one 5-node path through \
         A→B→C→D→E at max_depth=7; got empty: {body}"
    );
    let first: Vec<&str> = paths[0]
        .as_array()
        .expect("path[0] is an array")
        .iter()
        .map(|v| v.as_str().expect("path[0] entries are strings"))
        .collect();
    assert_eq!(
        first.len(),
        5,
        "shortest path through A→B→C→D→E must be 5 nodes long: {first:?}"
    );
    assert_eq!(first[0], ids[0]);
    assert_eq!(first[4], ids[4]);

    shutdown.notify_one();
    let _ = handle.await;
}
