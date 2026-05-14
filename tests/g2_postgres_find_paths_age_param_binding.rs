// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::elidable_lifetime_names)]
//! v0.7.0.1 G2 — postgres `find_paths_cypher` must use a SQL parameter
//! binding for AGE's `cypher()` third argument (the params jsonb), not
//! an inlined literal.
//!
//! Reproducer for HALT finding G2 from
//! `runs/v0.7.0-a2a-cont6-cert-r1b-20260509-2148/findings/HALT.md`.
//!
//! ## What this test asserts
//!
//! 1. Boot an in-process HTTP daemon backed by [`PostgresStore`] against
//!    a database with Apache AGE 1.5.0 enabled (so the SAL dispatcher's
//!    `kg_backend = Age` branch lights up rather than falling back to
//!    the recursive-CTE path).
//! 2. Bootstrap the `memory_graph` AGE projection.
//! 3. Seed a 5-node chain A→B→C→D→E by POSTing memories then projecting
//!    nodes + edges directly into the AGE graph (the `link_internal`
//!    path doesn't mirror into AGE; the projection is normally seeded
//!    by the J1 graph-prep scripts — this test owns its corpus).
//! 4. POST `{source_id: A, target_id: E, max_depth: 7}` to
//!    `/api/v1/kg/find_paths` and assert a 200 with at least one
//!    enumerated path.
//!
//! ## Pre-fix behaviour (the failure shape from S65)
//!
//! AGE 1.5.0 raises:
//!
//! ```text
//! error returned from database: third argument of cypher function must
//! be a parameter
//! ```
//!
//! …because the postgres adapter inlines the params dict as a literal
//! `'{"start_id":"…"}'::agtype` rather than binding it through sqlx as
//! a `$N` placeholder. The handler converts that error to a 503
//! "storage backend unavailable" so the cert harness sees the wire
//! shape `503` + JSON error.
//!
//! ## Gating
//!
//! Same as `serve_postgres_smoke.rs` plus `AI_MEMORY_TEST_AGE_URL` must
//! be set to a database with the `age` extension installed (vanilla
//! pgvector deployments hit the CTE branch and bypass this test).

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
        "content": format!("g2 reproducer node {title}"),
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

/// sqlx wrapper that binds a raw agtype value through the Postgres
/// wire protocol. Mirrors the production-side `Agtype` defined in
/// `src/store/postgres.rs::Agtype` (kept private because it's only
/// load-bearing on the AGE Cypher path). The fixture path in this
/// test file owns its own copy because the production-side type is
/// crate-private.
struct Agtype(String);

impl sqlx::Type<sqlx::Postgres> for Agtype {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_name("agtype")
    }
    fn compatible(_ty: &sqlx::postgres::PgTypeInfo) -> bool {
        true
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Postgres> for Agtype {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        // AGE binary format: 1-byte version (0x01) prefix, then the
        // JSON text payload. Mirrors the production-side encoder in
        // `src/store/postgres.rs::Agtype` so the test fixture
        // exercises the same wire shape `find_paths_cypher` does.
        buf.push(1);
        buf.extend_from_slice(self.0.as_bytes());
        Ok(sqlx::encode::IsNull::No)
    }
}

/// Bootstrap `memory_graph` and project a chain `ids[0]→ids[1]→…→ids[n-1]`
/// into AGE. Idempotent — safe to re-run against an already-bootstrapped
/// projection. Mirrors the helper in `age_cte_equivalence.rs` so the
/// AGE half of the test owns its own corpus.
async fn bootstrap_and_project_chain(url: &str, ids: &[String]) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("pool");

    // Bootstrap the projection. AGE returns "already exists" when the
    // graph is present — tolerate that as a success signal.
    {
        let mut conn = pool.acquire().await.expect("acquire");
        sqlx::query("LOAD 'age'")
            .execute(&mut *conn)
            .await
            .expect("load age");
        sqlx::query("SET search_path = ag_catalog, \"$user\", public")
            .execute(&mut *conn)
            .await
            .expect("set search_path");
        if let Err(e) = sqlx::query("SELECT create_graph('memory_graph')")
            .execute(&mut *conn)
            .await
        {
            let msg = e.to_string();
            assert!(
                msg.contains("already exists"),
                "create_graph must succeed or be already-bootstrapped: {msg}"
            );
        }
    }

    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("LOAD 'age'")
        .execute(&mut *tx)
        .await
        .expect("load age tx");
    sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&mut *tx)
        .await
        .expect("set search_path tx");

    for id in ids {
        let cypher = "MERGE (n {id: $id}) RETURN n";
        let sql = format!("SELECT * FROM cypher('memory_graph', $$ {cypher} $$, $1) AS (n agtype)");
        let params = json!({ "id": id }).to_string();
        sqlx::query(&sql)
            .bind(Agtype(params))
            .fetch_all(&mut *tx)
            .await
            .expect("project node");
    }

    for w in ids.windows(2) {
        let cypher = "MATCH (a {id: $src}), (b {id: $dst}) \
             MERGE (a)-[r:related_to {relation: 'related_to'}]->(b) \
             RETURN r";
        let sql = format!("SELECT * FROM cypher('memory_graph', $$ {cypher} $$, $1) AS (r agtype)");
        let params = json!({ "src": w[0], "dst": w[1] }).to_string();
        sqlx::query(&sql)
            .bind(Agtype(params))
            .fetch_all(&mut *tx)
            .await
            .expect("project edge");
    }

    tx.commit().await.expect("commit projection");
}

/// G2 reproducer — `POST /api/v1/kg/find_paths` over a 5-node chain
/// must return a 200 with a non-empty `paths` array on a postgres-AGE
/// daemon at `max_depth=7`.
#[tokio::test(flavor = "multi_thread")]
async fn g2_postgres_find_paths_age_returns_200_with_paths() {
    let Some(url) = age_url() else {
        eprintln!(
            "skipping g2_postgres_find_paths_age_returns_200_with_paths: \
             AI_MEMORY_TEST_AGE_URL / AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    // Probe the connection's `kg_backend`; if AGE isn't installed, the
    // CTE path doesn't exhibit G2 and the test is a no-op.
    let store = PostgresStore::connect(&url)
        .await
        .expect("connect postgres adapter");
    if !matches!(store.kg_backend(), ai_memory::store::KgBackend::Age) {
        eprintln!(
            "skipping g2_postgres_find_paths_age_returns_200_with_paths: \
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
    let ns = format!("g2-find-paths-{suffix}");

    // Build chain A→B→C→D→E in `memories` + `memory_links`.
    let mut ids = Vec::new();
    for label in ["A", "B", "C", "D", "E"] {
        ids.push(store_memory(&client, &base, &ns, &format!("g2-{suffix}-{label}")).await);
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

    // Mirror the same chain into the AGE projection. The `link_internal`
    // path on the postgres adapter does NOT replay into AGE; the J1
    // graph-prep scripts seed the projection in production. The cert
    // harness gets to a populated projection through that scripts path
    // — for the unit-level test we own the corpus.
    bootstrap_and_project_chain(&url, &ids).await;

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
        "G2: find_paths must return 200 from a postgres-AGE daemon for a \
         valid 4-hop chain; got status={status} body={body}. Pre-fix this \
         was a 503 with `cypher find_paths: third argument of cypher \
         function must be a parameter`."
    );

    let paths = body["paths"].as_array().expect("paths must be an array");
    assert!(
        !paths.is_empty(),
        "G2: find_paths must surface at least one path through the \
         A→B→C→D→E chain; got empty: {body}"
    );

    // First path should be the canonical 5-node chain.
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
