// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//! v0.7.0.1 G4 — postgres `link_signed` / `store_link` must project the
//! source + target memories as nodes and the link as an edge into the
//! AGE `memory_graph` projection within the same transaction as the
//! `INSERT INTO memory_links` write.
//!
//! Reproducer for the G4 finding surfaced by R1 against live Plan B
//! cert droplets (`runs/v0.7.0-cpu-r1-20260510-022031/scenario-65.log`):
//!
//!   * scenario writes 5 memories + 4 links (chain A→B→C→D→E) through
//!     `POST /api/v1/memories` + `POST /api/v1/links`
//!   * `SELECT count(*) FROM memory_links` returns 22 (all writes
//!     landed)
//!   * `SELECT * FROM cypher('memory_graph', $$MATCH (n) RETURN
//!     count(n)$$) AS (c agtype)` returns **0**
//!   * `find_paths(A, E, max_depth=7)` returns `paths_found=0` because
//!     the AGE projection is empty
//!
//! ## What this test asserts
//!
//! 1. Boot an in-process HTTP daemon backed by [`PostgresStore`]
//!    against a database with Apache AGE installed (so the SAL
//!    dispatcher's `kg_backend = Age` branch lights up rather than the
//!    CTE fallback).
//! 2. Seed a 5-node chain A→B→C→D→E by POSTing memories then linking
//!    each adjacent pair through the public REST surface — same wire
//!    shape S65 uses.
//! 3. Issue a Cypher node-count over the `memory_graph` projection
//!    directly (so the test fails clearly on the projection layer
//!    rather than masking the gap behind the `find_paths` traversal).
//! 4. Issue `POST /api/v1/kg/find_paths {source: A, target: E,
//!    max_depth: 7}` and assert the daemon returns at least one path.
//!
//! ## Pre-fix behaviour (the failure shape from S65)
//!
//! `link_internal` only writes to the `memory_links` SQL table — it
//! does not run the Cypher MERGE that registers the node/edge in the
//! AGE graph. With an empty projection, `find_paths_cypher` returns
//! zero rows even when the SQL link table holds the chain.
//!
//! ## Gating
//!
//! Same as `g2_postgres_find_paths_age_param_binding.rs` —
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
use sqlx::Row;
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
        "content": format!("g4 reproducer node {title}"),
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

/// sqlx wrapper that decodes/encodes a raw `agtype` value through the
/// Postgres wire protocol. Mirrors the production-side `Agtype` defined
/// in `src/store/postgres.rs::Agtype` (kept private because it's only
/// load-bearing on the AGE Cypher path).
struct Agtype(String);

impl sqlx::Type<sqlx::Postgres> for Agtype {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_name("agtype")
    }
    fn compatible(_ty: &sqlx::postgres::PgTypeInfo) -> bool {
        true
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for Agtype {
    fn decode(value: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        // AGE encodes agtype on the wire as a 1-byte version prefix
        // (0x01) followed by the JSON text payload. Strip the version
        // byte before exposing the inner JSON to the test side.
        let bytes = value.as_bytes()?;
        let payload = if bytes.first() == Some(&1) {
            &bytes[1..]
        } else {
            bytes
        };
        Ok(Agtype(String::from_utf8(payload.to_vec())?))
    }
}

async fn count_age_nodes(url: &str) -> i64 {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("pool");
    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::query("LOAD 'age'")
        .execute(&mut *conn)
        .await
        .expect("load age");
    sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&mut *conn)
        .await
        .expect("set search_path");
    let row = sqlx::query(
        "SELECT c FROM cypher('memory_graph', $$ MATCH (n) RETURN count(n) $$) AS (c agtype)",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("count nodes");
    let raw: Agtype = row.try_get("c").expect("read count");
    // AGE counts come back as bare integers (`12`) — no quoting, no
    // suffix — because the value goes through the agtype JSON encoder
    // before the wire-protocol prefix lands.
    raw.0.trim().parse::<i64>().expect("parse count")
}

/// G4 reproducer — write 5 memories + 4 links through the public REST
/// surface; the AGE projection must contain at least 5 nodes (the
/// memories) and `find_paths` must surface the chain.
#[tokio::test(flavor = "multi_thread")]
async fn g4_postgres_link_projects_memories_into_age_graph() {
    let Some(url) = age_url() else {
        eprintln!(
            "skipping g4_postgres_link_projects_memories_into_age_graph: \
             AI_MEMORY_TEST_AGE_URL / AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    // Probe the connection's `kg_backend`; the CTE fallback doesn't
    // exhibit G4 because there's no AGE projection to keep in sync
    // with the SQL `memory_links` table.
    let store = PostgresStore::connect(&url)
        .await
        .expect("connect postgres adapter");
    if !matches!(store.kg_backend(), ai_memory::store::KgBackend::Age) {
        eprintln!(
            "skipping g4_postgres_link_projects_memories_into_age_graph: \
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
    let ns = format!("g4-link-projection-{suffix}");

    // Snapshot the AGE node count before the test seeds. The shared
    // scratch DB may already hold projections from previous runs, so
    // we measure deltas rather than absolutes.
    let nodes_before = count_age_nodes(&url).await;

    // Build chain A→B→C→D→E in `memories` + `memory_links` through the
    // REST surface. After the fix, every link write also projects both
    // endpoints + the edge into the `memory_graph` AGE projection.
    let mut ids = Vec::new();
    for label in ["A", "B", "C", "D", "E"] {
        ids.push(store_memory(&client, &base, &ns, &format!("g4-{suffix}-{label}")).await);
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

    // Assert the AGE projection grew by at least 5 nodes (one per
    // memory). Pre-fix this delta is zero — the SQL `memory_links`
    // INSERT lands but no Cypher MERGE runs.
    let nodes_after = count_age_nodes(&url).await;
    assert!(
        nodes_after >= nodes_before + 5,
        "G4: AGE memory_graph must hold at least 5 new nodes after \
         seeding a 5-node chain through POST /api/v1/links; \
         nodes_before={nodes_before} nodes_after={nodes_after}. \
         Pre-fix the link write only touched the `memory_links` SQL \
         table — the Cypher MERGE never ran."
    );

    // S65 wire: depth=7 (max ceiling) covers a 4-hop chain.
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
        .expect("find_paths POST");

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(json!({}));

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "G4: find_paths must return 200 from a postgres-AGE daemon \
         after the link write self-projects into AGE; got status={status} body={body}."
    );

    let paths = body["paths"].as_array().expect("paths must be an array");
    assert!(
        !paths.is_empty(),
        "G4: find_paths must surface at least one path through the \
         A→B→C→D→E chain after the link write self-projects into AGE; \
         got empty: {body}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}
