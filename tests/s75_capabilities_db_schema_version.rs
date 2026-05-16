// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// v0.7.0 SHIP CAMPAIGN — this test routes through the SAL `MemoryStore`
// trait + `SqliteStore` adapter (both in `ai_memory::store::*`, which
// is gated behind the `sal` feature). Under the default feature set
// the module is configured out, so the unconditional `use` lines below
// would fail to resolve and the workspace `cargo test` (default-feature)
// build would break. Gate the entire file behind `sal` to match the
// shape of the code under test. Surfaced by the fold-a2a1.7 polish
// closeout per operator directive: no v0.8.0 deferral.
#![cfg(feature = "sal")]
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
        deferred_audit_queue: Arc::new(None),
    };
    (state, tmp)
}

#[tokio::test(flavor = "multi_thread")]
async fn s75_capabilities_surfaces_runtime_db_schema_version() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
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
    //    CURRENT_SCHEMA_VERSION (32 at v0.7.0 after substrate-rules
    //    (issue #691) took v30 for the `governance_rules` table, L1-1
    //    took v31 for the `memories.memory_kind` column, and L1-5 took
    //    v32 for the `skills` + `skill_resources` tables) on `open`,
    //    so the live read MUST land at that value — proving the field
    //    comes from the SAL trait lookup rather than a hard-coded
    //    constant.
    assert!(
        v >= 1,
        "S75: db_schema_version must be a positive integer for a \
         freshly-opened SqliteStore (migrations were applied at open \
         time); got {v}"
    );
    assert_eq!(
        v, 42,
        "S75: db_schema_version must match `CURRENT_SCHEMA_VERSION` \
         (42 at v0.7.0 polish-readiness — v0.7.0 grand-slam delta over \
         the prior 37: Form 4 (#757, citations/source-uri/atom-span) \
         bumped 37 to 38, Form 5 (#758, confidence-calibration + \
         shadow-mode) bumped 38 to 39, Cluster C signed-events DLQ \
         (issue #767 / SEC-3) bumped 39 to 40, Cluster G \
         shadow-retention denormalised source column + compound \
         index (issue #767 / PERF-4) bumped 40 to 41, and polish \
         PERF-8 (issue #781, auto_persona mentioned_entity_id + \
         partial index replacing the content LIKE scan) bumped 41 \
         to 42. Pre-grand-slam baseline 37 history retained in the \
         pre-cluster-G version of this test. A drift here means \
         either the binary's migrate ladder skipped a step or the new \
         SAL `schema_version()` lookup is reading from the wrong \
         source."
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
