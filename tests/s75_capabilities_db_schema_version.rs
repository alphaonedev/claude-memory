// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//! v0.7.0.1 S75 — `/api/v1/capabilities` must surface a runtime
//! `db_schema_version` integer (the live `MAX(version)` from the
//! underlying store's `schema_version` table) so operators can tell at
//! a glance whether the deployed daemon's database is on the schema
//! the binary expects.
//!
//! Pre-fix the only schema-related field on the capabilities response
//! was the wire-format discriminator (`schema_version: "3"`) which is
//! the capabilities-document version, not the migration ladder. R1 v4
//! S75 surfaces the gap: a daemon connected to `aimemory_w4_live`
//! but the parity oracle queries `aimemory` and reports `-1`, with no
//! daemon-side runtime way to confirm which database the live
//! deployment is actually on.
//!
//! ## What this test asserts
//!
//! 1. The `db_schema_version` field is present on every capabilities
//!    response (always-emitted, never `skip_serializing_if`).
//! 2. It is a JSON number (not a string — distinct in shape from the
//!    string-typed wire-format discriminator).
//! 3. The value matches the live `MAX(version) FROM schema_version`
//!    on the underlying store. For a freshly-opened SQLite store the
//!    value is the binary's `CURRENT_SCHEMA_VERSION` (28 at v0.7.0),
//!    proving the lookup goes through the SAL adapter rather than a
//!    hard-coded constant.
//!
//! Runs against an in-process `serve` boot using the `SqliteStore`
//! adapter (no postgres fixture required) so it is a pure unit-style
//! test that reproduces against the same code path the live HTTP
//! deployment uses.

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::store::MemoryStore;
use ai_memory::store::sqlite::SqliteStore;
use serde_json::Value;
use tokio::sync::{Mutex, Notify, RwLock};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// Build an `AppState` with a SAL-routed `SqliteStore` opened against
/// a tempfile so the freshly-applied migrations populate the
/// `schema_version` table.
fn build_sqlite_app_state() -> (AppState, tempfile::NamedTempFile) {
    // The `Db` legacy field stays a `:memory:` connection because the
    // legacy direct-rusqlite handlers reach for it at GC / WAL
    // checkpoint time; the test never exercises those paths so a
    // disjoint handle is harmless. The trait-routed `app.store` is
    // the live SqliteStore opened against an on-disk tempfile so its
    // own migration path runs (the in-memory connection runs the
    // same migrations but its `schema_version` table is invisible
    // to `app.store`'s own pool).
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).expect("scratch sqlite");
    let path = std::path::PathBuf::from(":memory:");
    let db: Db = Arc::new(Mutex::new((conn, path, ResolvedTtl::default(), true)));
    let tmp = tempfile::NamedTempFile::new().expect("tempfile for SqliteStore");
    let store: Arc<dyn MemoryStore> =
        Arc::new(SqliteStore::open(tmp.path()).expect("open SqliteStore"));
    let state = AppState {
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
        storage_backend: StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        #[cfg(not(feature = "sal"))]
        _phantom: std::marker::PhantomData,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
    };
    (state, tmp)
}

#[tokio::test(flavor = "multi_thread")]
async fn s75_capabilities_surfaces_runtime_db_schema_version() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState { key: None };
    let (app_state, _tmp) = build_sqlite_app_state();

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

    // Wait for the listener — same shape as serve_postgres_smoke.
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
    assert!(ready, "in-process HTTP daemon never bound");

    let client = reqwest::Client::new();
    let caps: Value = client
        .get(format!("http://{addr}/api/v1/capabilities"))
        .send()
        .await
        .expect("capabilities GET")
        .json()
        .await
        .expect("capabilities body");

    // 1) Always-emitted field. Pre-fix the response carried only the
    //    wire-format `schema_version: "3"` discriminator and no live
    //    DB schema field at all.
    let raw = caps.get("db_schema_version");
    assert!(
        raw.is_some(),
        "S75: capabilities response must include `db_schema_version`; \
         got: {caps:#}"
    );

    // 2) Number, not string. Distinct in shape from the wire-format
    //    discriminator so clients can branch off the type.
    let v = raw
        .and_then(Value::as_i64)
        .expect("db_schema_version must be a JSON integer");

    // 3) Value tracks the live SAL adapter's `schema_version` table.
    //    A freshly-opened SqliteStore runs every migration up to
    //    CURRENT_SCHEMA_VERSION (29 at v0.7.0 after recursive-learning
    //    Task 1/8 added `memories.reflection_depth`) on `open`, so the
    //    live read MUST land at that value — proving the field comes
    //    from the SAL trait lookup rather than a hard-coded constant.
    assert!(
        v >= 1,
        "S75: db_schema_version must be a positive integer for a \
         freshly-opened SqliteStore (migrations were applied at open \
         time); got {v}"
    );
    assert_eq!(
        v, 30,
        "S75: db_schema_version must match `CURRENT_SCHEMA_VERSION` \
         (30 at v0.7.0 — issue #691 bumped from 29 to 30 to add the \
         `governance_rules` table backing the substrate-level \
         agent-action rules engine). A drift here means either the \
         binary's migrate ladder skipped a step or the new SAL \
         `schema_version()` lookup is reading from the wrong source."
    );

    // 4) The wire-format discriminator stays alongside but distinct
    //    — pre-fix bug report flagged the daemon as "reporting 3"
    //    where 28 was expected; the fix preserves the discriminator
    //    (string `"3"`) AND adds the integer migration value.
    assert_eq!(
        caps.get("schema_version").and_then(Value::as_str),
        Some("3"),
        "S75: wire-format `schema_version` discriminator must remain \
         `\"3\"` (the capabilities-document version); \
         got: {caps:#}"
    );

    shutdown.notify_one();
    let _ = handle.await;
}
