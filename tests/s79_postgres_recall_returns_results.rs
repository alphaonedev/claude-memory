// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::too_many_lines)]
//! v0.7.0.1 S79 — postgres-backed recall must surface non-empty
//! results for namespace-scoped queries that lexically match the
//! seeded corpus.
//!
//! Reproducer for the S79 finding surfaced by R1 against live Plan B
//! cert droplets (`runs/v0.7.0-cpu-r1-20260510-022031/scenario-79.log`):
//!
//!   * scenario seeds 50 memories across 5 namespaces (10 per cluster:
//!     animals.dog, animals.cat, lang.python, lang.rust, db.sql)
//!   * issues 10 recall queries; mean Jaccard@5 vs the lexical
//!     reference settles at 0.13 (floor 0.20)
//!   * 3/10 queries return EMPTY: `cat sleeping`, `rust ownership`,
//!     `compile time safety`
//!
//! Because the cert harness's reference set is computed lexically
//! (token Jaccard against memory content), the daemon's contract is
//! "return at least one row from the namespace whenever the
//! namespace-scoped FTS index matches at least one row". The
//! postgres adapter's `recall_hybrid` path issues
//! `plainto_tsquery('english', $1)` which is AND-joined: a query
//! like `dog field` requires **both** lemmas in the same row. Sqlite
//! parity is `OR`-joined (`sanitize_fts_query(_, true)` produces
//! `"dog" OR "field"` for the FTS5 MATCH expression). The mismatch
//! surfaces as a strictly tighter recall on the postgres side.
//!
//! ## What this test asserts
//!
//! 1. Boot an in-process HTTP daemon backed by [`PostgresStore`].
//! 2. Seed 5 memories under a fresh namespace, with content that
//!    each contains AT LEAST one of the query's tokens but not all
//!    of them. Sqlite's OR-joined FTS5 returns these rows; postgres
//!    `plainto_tsquery` returns empty (pre-fix).
//! 3. Issue `POST /api/v1/recall` with a multi-token query against
//!    that namespace and assert at least one result lands.
//!
//! ## Gating
//!
//! Same as `g1_postgres_quota_increment_on_store.rs` —
//! `feature = "sal-postgres"` plus `AI_MEMORY_TEST_POSTGRES_URL`
//! must be set. Without either, the test prints a skip line and
//! returns cleanly.

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
        "store should succeed: status={} body={:?}",
        resp.status(),
        resp.text().await.ok()
    );
    let v: Value = resp.json().await.expect("body");
    v["id"].as_str().expect("id").to_string()
}

/// S79 reproducer — multi-token recall queries against a namespace
/// where each row matches AT LEAST one token (but not all of them)
/// must surface results. Sqlite's OR-joined FTS5 contract returns
/// these rows; the postgres adapter's `plainto_tsquery` (AND-joined)
/// pre-fix returns empty.
#[tokio::test(flavor = "multi_thread")]
async fn s79_postgres_recall_or_style_returns_namespace_results() {
    let Some(url) = postgres_url() else {
        eprintln!(
            "skipping s79_postgres_recall_or_style_returns_namespace_results: \
             AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let (base, shutdown, handle) = spawn_daemon(&url).await;
    let client = reqwest::Client::new();

    // Per-test namespace so concurrent runs against a shared scratch DB
    // don't reuse memory ids.
    let suffix = uuid::Uuid::new_v4();
    let ns = format!("s79-recall-{suffix}");

    // Seed 5 memories that each contain ONE of the query tokens
    // (`brown`, `dog`, `fast`, `field`) but never both `brown` AND
    // `runs` (the second query). With AND-joined plainto_tsquery the
    // second query falls into the empty bucket; with OR-joined
    // (sqlite parity) at least one row matches.
    store_memory(
        &client,
        &base,
        &ns,
        &format!("s79-{suffix}-brown"),
        "the brown animal grazes nearby",
    )
    .await;
    store_memory(
        &client,
        &base,
        &ns,
        &format!("s79-{suffix}-dog"),
        "a friendly dog wags its tail",
    )
    .await;
    store_memory(
        &client,
        &base,
        &ns,
        &format!("s79-{suffix}-fast"),
        "the runner sprints fast across pavement",
    )
    .await;
    store_memory(
        &client,
        &base,
        &ns,
        &format!("s79-{suffix}-field"),
        "the field stretches to the horizon",
    )
    .await;
    store_memory(
        &client,
        &base,
        &ns,
        &format!("s79-{suffix}-cat"),
        "an orange cat naps on the windowsill",
    )
    .await;

    // Query "brown runs" — `brown` appears in row #1, `runs` in row
    // #3, but no row has both. AND-joined `plainto_tsquery` returns
    // zero rows; OR-joined parity returns at least one (row #1 by
    // exact `brown` match).
    let resp = client
        .post(format!("{base}/api/v1/recall"))
        .json(&json!({
            "query": "brown runs",
            "namespace": ns,
            "limit": 5,
        }))
        .send()
        .await
        .expect("recall POST");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("recall body");
    let memories = body["memories"]
        .as_array()
        .or_else(|| body["results"].as_array())
        .expect("memories array on recall response");

    assert!(
        !memories.is_empty(),
        "S79: recall query 'brown runs' against the namespace must \
         return at least one row that lexically matches `brown` OR \
         `runs` (sqlite parity); got empty: {body}. Pre-fix the \
         postgres adapter's `plainto_tsquery` ANDs every token, so a \
         query whose tokens never co-occur in a single row falls \
         into the empty bucket."
    );

    // Sanity — the corpus has at least one row containing `brown` so
    // any OR-joined matcher MUST surface it. The exact ordering is
    // not part of the contract here (that's covered by S62 + S79's
    // Jaccard floor).
    let titles: Vec<String> = memories
        .iter()
        .filter_map(|m| m["title"].as_str().map(str::to_string))
        .collect();
    assert!(
        titles
            .iter()
            .any(|t| t.ends_with("-brown") || t.ends_with("-fast")),
        "S79: recall must surface the row containing `brown` or the \
         row containing `runs`/`fast`; got titles={titles:?} body={body}"
    );

    // Second probe — "field horizon": both lemmas live on row #4. AND
    // and OR semantics agree. This sanity-asserts the FTS match
    // surface is wired at all (rules out a broader regression).
    let resp = client
        .post(format!("{base}/api/v1/recall"))
        .json(&json!({
            "query": "field horizon",
            "namespace": ns,
            "limit": 5,
        }))
        .send()
        .await
        .expect("recall POST 2");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body2: Value = resp.json().await.expect("recall body 2");
    let memories2 = body2["memories"]
        .as_array()
        .or_else(|| body2["results"].as_array())
        .expect("memories array on recall response 2");
    assert!(
        !memories2.is_empty(),
        "sanity: AND/OR-agreeing query 'field horizon' must return \
         the field row; got empty: {body2}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}
