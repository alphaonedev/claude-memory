// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::too_many_lines)]
//! v0.7.0.1 G4 follow-up — confirm both the **unsigned** trait method
//! `MemoryStore::link` and the live HTTP `POST /api/v1/links` path
//! project link writes into the AGE `memory_graph` projection on
//! postgres-backed daemons.
//!
//! ## Why this test exists
//!
//! The first G4 reproducer (`tests/g4_postgres_link_projects_into_age_graph.rs`)
//! exercised only the signed trait method (`link_signed`), reaching
//! `link_internal` through the keypair-aware path. R1 v4 against the
//! Plan B cert droplets surfaced the symptom again at HEAD `4c96c3eb`
//! — 22 rows in `memory_links`, 0 nodes/edges in `memory_graph`,
//! `find_paths` returns empty.
//!
//! The hypothesis from the round-2 bug report:
//!
//!   > the G4 commit 9f5eb1f added 355 lines but ONLY in `link_signed`.
//!   > The HTTP `POST /api/v1/links` handler likely calls a DIFFERENT
//!   > method (maybe `store_link`, `create_link`, or routes through
//!   > `link()` without the `signed` variant). Verify which trait
//!   > method the handler actually calls and ensure ALL link-creation
//!   > paths project to AGE.
//!
//! On inspection both `MemoryStore::link` and `MemoryStore::link_signed`
//! ALREADY route through the postgres adapter's `link_internal`
//! helper (which carries the G4 projection). This test pins that
//! contract so a future refactor that splits the helpers cannot
//! regress only one of the two paths silently.
//!
//! ## What this test asserts
//!
//! 1. The unsigned `MemoryStore::link` trait method (no keypair) on a
//!    postgres-AGE backend lands the `memory_links` SQL row AND
//!    projects both endpoints + the edge into the `memory_graph`
//!    AGE projection inside the same transaction.
//! 2. The live HTTP `POST /api/v1/links` path with no `active_keypair`
//!    in `AppState` (so the handler's signed-or-unsigned dispatch
//!    lands on the unsigned branch) still projects to AGE.
//!
//! Mirrors the gating in `tests/g4_postgres_link_projects_into_age_graph.rs`
//! — feature `sal-postgres` plus `AI_MEMORY_TEST_AGE_URL` (or
//! `AI_MEMORY_TEST_POSTGRES_URL` fallback) must point at a database
//! with the `age` extension, otherwise the test prints a skip line
//! and returns cleanly.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::models::{Memory, MemoryLink, Tier};
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::{CallerContext, MemoryStore};
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

/// Mirror the agtype decoder from the original G4 reproducer so we
/// can read AGE's count(n) result over the SQL pool without dragging
/// in the production-side helper as a public type.
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
    raw.0.trim().parse::<i64>().expect("parse count")
}

async fn count_age_edges_with_relation(url: &str, relation: &str) -> i64 {
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
    let cypher = format!("MATCH ()-[r:{relation}]->() RETURN count(r)");
    let sql = format!("SELECT c FROM cypher('memory_graph', $$ {cypher} $$) AS (c agtype)");
    let row = sqlx::query(&sql)
        .fetch_one(&mut *conn)
        .await
        .expect("count edges");
    let raw: Agtype = row.try_get("c").expect("read count");
    raw.0.trim().parse::<i64>().expect("parse count")
}

fn fresh_memory(namespace: &str, title: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("g4 unsigned-link reproducer {title}"),
        tags: Vec::new(),
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
    }
}

/// Direct-trait reproducer — calls `MemoryStore::link` (unsigned) and
/// asserts the AGE projection grew.
#[tokio::test(flavor = "multi_thread")]
async fn g4_unsigned_link_trait_projects_into_age() {
    let Some(url) = age_url() else {
        eprintln!(
            "skipping g4_unsigned_link_trait_projects_into_age: \
             AI_MEMORY_TEST_AGE_URL / AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let store = PostgresStore::connect(&url)
        .await
        .expect("connect postgres adapter");
    if !matches!(store.kg_backend(), ai_memory::store::KgBackend::Age) {
        eprintln!(
            "skipping g4_unsigned_link_trait_projects_into_age: \
             kg_backend != Age (no AGE extension on this fixture)"
        );
        return;
    }

    let suffix = uuid::Uuid::new_v4();
    let ns = format!("g4-unsigned-trait-{suffix}");

    // Seed two memories so the link write satisfies the FK pre-flight
    // in `link_internal`.
    let ctx = CallerContext::for_agent("ai:test");
    let mem_a = fresh_memory(&ns, &format!("g4-unsigned-{suffix}-A"));
    let mem_b = fresh_memory(&ns, &format!("g4-unsigned-{suffix}-B"));
    store.store(&ctx, &mem_a).await.expect("store memory A");
    store.store(&ctx, &mem_b).await.expect("store memory B");

    let nodes_before = count_age_nodes(&url).await;

    let link = MemoryLink {
        source_id: mem_a.id.clone(),
        target_id: mem_b.id.clone(),
        relation: "related_to".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        valid_from: None,
        valid_until: None,
        observed_by: None,
        signature: None,
    };
    // **Unsigned** trait method — exercise the path the user's
    // hypothesis flagged ("the HTTP path may call link() rather than
    // link_signed()"). G4 must reach BOTH paths.
    store
        .link(&ctx, &link)
        .await
        .expect("trait link (unsigned) succeeds");

    let nodes_after = count_age_nodes(&url).await;
    assert!(
        nodes_after >= nodes_before + 2,
        "G4 unsigned-link trait: AGE memory_graph must hold at least 2 \
         new nodes after `MemoryStore::link` writes a single edge \
         between two memories; nodes_before={nodes_before} \
         nodes_after={nodes_after}. Pre-fix the unsigned path only \
         touched the SQL `memory_links` table — the Cypher MERGE never ran."
    );

    let edges_after = count_age_edges_with_relation(&url, "related_to").await;
    assert!(
        edges_after >= 1,
        "G4 unsigned-link trait: AGE memory_graph must hold at least 1 \
         related_to edge after the unsigned link write; got {edges_after}."
    );
}

/// HTTP-path reproducer — boots an in-process daemon with no
/// `active_keypair` so `create_link` lands on the unsigned branch,
/// and asserts the AGE projection grew.
#[tokio::test(flavor = "multi_thread")]
async fn g4_unsigned_http_link_projects_into_age() {
    let Some(url) = age_url() else {
        eprintln!(
            "skipping g4_unsigned_http_link_projects_into_age: \
             AI_MEMORY_TEST_AGE_URL / AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let store_probe = PostgresStore::connect(&url)
        .await
        .expect("connect postgres adapter");
    if !matches!(store_probe.kg_backend(), ai_memory::store::KgBackend::Age) {
        eprintln!(
            "skipping g4_unsigned_http_link_projects_into_age: \
             kg_backend != Age (no AGE extension on this fixture)"
        );
        return;
    }
    drop(store_probe);

    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).expect("scratch sqlite");
    let path = std::path::PathBuf::from(":memory:");
    let db: Db = Arc::new(Mutex::new((conn, path, ResolvedTtl::default(), true)));
    let store: Arc<dyn MemoryStore> = Arc::new(
        PostgresStore::connect(&url)
            .await
            .expect("connect postgres adapter"),
    );
    // CRITICAL: `active_keypair = None` so the handler's signed
    // branch falls through to the unsigned attest_level path. This
    // is the same configuration a daemon launched without
    // `--keypair` would have, so the test exercises the
    // production-equivalent unsigned path.
    let app_state = AppState {
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
    };

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState { key: None };
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
    assert!(ready, "in-process HTTP daemon never bound");

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let suffix = uuid::Uuid::new_v4();
    let ns = format!("g4-unsigned-http-{suffix}");

    // Seed two memories via HTTP — same wire as S65.
    let mut ids = Vec::new();
    for label in ["A", "B"] {
        let resp = client
            .post(format!("{base}/api/v1/memories"))
            .json(&json!({
                "tier": "long",
                "namespace": ns.clone(),
                "title": format!("g4-unsigned-http-{suffix}-{label}"),
                "content": format!("unsigned-http reproducer {label}"),
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "system"
            }))
            .send()
            .await
            .expect("memory POST");
        assert!(resp.status().is_success());
        let v: Value = resp.json().await.expect("memory body");
        ids.push(v["id"].as_str().expect("id").to_string());
    }

    let nodes_before = count_age_nodes(&url).await;

    let resp = client
        .post(format!("{base}/api/v1/links"))
        .json(&json!({
            "source_id": ids[0],
            "target_id": ids[1],
            "relation": "related_to",
        }))
        .send()
        .await
        .expect("link POST");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "G4 unsigned-http: POST /api/v1/links must succeed; got status={status} body={body}"
    );
    // Without an `active_keypair`, the wire response should report
    // `attest_level = "unsigned"` — proving the handler did NOT take
    // the signed branch.
    assert_eq!(
        body["attest_level"].as_str(),
        Some("unsigned"),
        "G4 unsigned-http: with no active_keypair the handler must \
         return attest_level=unsigned; got body={body}"
    );

    let nodes_after = count_age_nodes(&url).await;
    assert!(
        nodes_after >= nodes_before + 2,
        "G4 unsigned-http: AGE memory_graph must hold at least 2 new \
         nodes after POST /api/v1/links on a daemon with no active \
         keypair; nodes_before={nodes_before} nodes_after={nodes_after}."
    );

    shutdown.notify_one();
    let _ = handle.await;
}
