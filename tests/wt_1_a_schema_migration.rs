// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-A — schema v36 atomisation foundation regression tests.
//!
//! Pins the migration contract for the first hard prereq of WT-1-B
//! through WT-1-G. Asserts:
//!
//! 1. Fresh-DB migrate lands `memories.atomised_into` + `memories.atom_of`
//!    columns with the right types and the supporting partial indexes.
//! 2. The migration ladder is idempotent — running migrate twice on the
//!    same DB is a no-op (`schema_version` stays at 36).
//! 3. Pre-existing rows seeded at v35 are preserved verbatim through
//!    the v36 upgrade, with the new columns landing as NULL.
//! 4. The new `MemoryLinkRelation::DerivesFrom` variant round-trips
//!    through `Display` and `FromStr`.
//! 5. The extended `memory_links.relation` CHECK constraint accepts
//!    `'derives_from'` and still rejects bogus values.
//! 6. The `/api/v1/capabilities` endpoint reports `db_schema_version
//!    == 36` (extends the s75 test pattern in
//!    `tests/s75_capabilities_db_schema_version.rs`).

#![cfg(feature = "sal")]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::handlers::{ApiKeyState, AppState, Db, StorageBackend};
use ai_memory::models::link::MemoryLinkRelation;
use ai_memory::store::MemoryStore;
use ai_memory::store::sqlite::SqliteStore;
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::{Mutex, Notify, RwLock};

// -------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------

/// Probe whether `column` exists on `table`. Mirrors the inline helper
/// in `src/storage/migrations.rs::tests::column_exists` so we can
/// validate post-migrate schema shape without reaching into the crate-
/// private helper.
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql).expect("PRAGMA prepare");
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("PRAGMA query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect col names");
    names.iter().any(|n| n == column)
}

fn index_exists(conn: &Connection, index_name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
        rusqlite::params![index_name],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|c| c > 0)
}

/// Open a fresh sqlite DB through `db::open` (which applies the full
/// migration ladder). Returns the `Connection` plus the holding
/// `NamedTempFile` so the file outlives the connection.
fn fresh_db_via_open() -> (Connection, tempfile::NamedTempFile) {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let conn = ai_memory::db::open(tmp.path()).expect("db::open applies migrations");
    (conn, tmp)
}

/// Free-port helper — same shape as s75_capabilities_db_schema_version.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

// -------------------------------------------------------------------
// Migration shape & idempotency
// -------------------------------------------------------------------

#[test]
fn test_migration_v36_applies_cleanly() {
    // Fresh DB through the full migrate ladder; the two new columns
    // must be present with the expected types, and the partial
    // indexes must exist.
    let (conn, _tmp) = fresh_db_via_open();

    assert!(
        column_exists(&conn, "memories", "atomised_into"),
        "v36: memories.atomised_into must exist after migrate"
    );
    assert!(
        column_exists(&conn, "memories", "atom_of"),
        "v36: memories.atom_of must exist after migrate"
    );

    // Type sanity: PRAGMA returns the declared type. atomised_into is
    // INTEGER, atom_of is TEXT (matches the existing `memories.id`
    // column type).
    let atomised_into_type: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('memories') WHERE name='atomised_into'",
            [],
            |r| r.get(0),
        )
        .expect("query atomised_into type");
    assert_eq!(
        atomised_into_type.to_uppercase(),
        "INTEGER",
        "v36: atomised_into must be INTEGER; got {atomised_into_type}"
    );

    let atom_of_type: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('memories') WHERE name='atom_of'",
            [],
            |r| r.get(0),
        )
        .expect("query atom_of type");
    assert_eq!(
        atom_of_type.to_uppercase(),
        "TEXT",
        "v36: atom_of must be TEXT (matches memories.id); got {atom_of_type}"
    );

    assert!(
        index_exists(&conn, "idx_memories_atom_of"),
        "v36: idx_memories_atom_of must exist"
    );
    assert!(
        index_exists(&conn, "idx_memories_atomised_into"),
        "v36: idx_memories_atomised_into must exist"
    );

    // Schema version stamp matches the binary's CURRENT_SCHEMA_VERSION.
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .expect("read schema_version");
    // The migration ladder reaches v39 in current HEAD (Form 5 confidence
    // calibration); it passes through v36 (WT-1-A atomisation foundation)
    // on its way there, so the v36 column + index probes above still
    // exercise WT-1-A's migration step. The final stamped version tracks
    // `CURRENT_SCHEMA_VERSION` in src/storage/migrations.rs:
    //   v37 (QW-2 persona substrate) → v38 (Form 4 source-uri provenance)
    //   → v39 (Form 5 confidence calibration shadow table).
    // When CURRENT_SCHEMA_VERSION bumps, update this assertion in lockstep.
    assert_eq!(
        v, 39,
        "v36→v39: schema_version must be stamped at CURRENT_SCHEMA_VERSION \
         (migration ladder passes through v36 on its way to v39)"
    );
}

#[test]
fn test_migration_v36_idempotent() {
    // Re-opening the same DB file runs migrate() a second time. The
    // fast-path early-return at the top of migrate() should short-
    // circuit because version >= CURRENT_SCHEMA_VERSION.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let conn1 = ai_memory::db::open(tmp.path()).expect("first open");
    let v1: i64 = conn1
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .expect("read v1");
    drop(conn1);

    // Re-open — migrate runs again, must be a no-op.
    let conn2 = ai_memory::db::open(tmp.path()).expect("second open");
    let v2: i64 = conn2
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .expect("read v2");

    // The migration ladder reaches v39 (Form 5 confidence calibration);
    // pass-through v36 still exercises WT-1-A's atomisation migration.
    // Tracks `CURRENT_SCHEMA_VERSION` in src/storage/migrations.rs.
    assert_eq!(v1, 39);
    assert_eq!(v1, v2, "v39: migrate is not idempotent — version drifted");

    // Columns + indexes still present after replay.
    assert!(column_exists(&conn2, "memories", "atomised_into"));
    assert!(column_exists(&conn2, "memories", "atom_of"));
    assert!(index_exists(&conn2, "idx_memories_atom_of"));
    assert!(index_exists(&conn2, "idx_memories_atomised_into"));
}

#[test]
fn test_migration_v36_preserves_existing_data() {
    // Seed 100 memories on a fresh DB (already at v36), then verify
    // every row survives a second `db::open` call (which runs migrate
    // again) with NULL atomised_into + atom_of. We can't easily
    // synthesise a "real" v35 DB because the SCHEMA constant already
    // ships at v36 — but the contract we care about is "legacy rows
    // (those without atomised_into/atom_of explicitly set) preserve
    // their identity through the migration". Inserting 100 rows
    // WITHOUT touching the new columns reproduces that exact
    // scenario on a fresh DB; the columns default to NULL and the
    // row count stays stable across replay.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let conn = ai_memory::db::open(tmp.path()).expect("first open");

    for i in 0..100 {
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES (?1, 'mid', 'wt-1-a', ?2, 'body', '2026-05-14T00:00:00Z', '2026-05-14T00:00:00Z')",
            rusqlite::params![format!("m-{i:03}"), format!("title-{i:03}")],
        )
        .expect("seed row");
    }
    drop(conn);

    // Re-open: migrate is a no-op but the file path is real, so the
    // 100 rows must round-trip. Also assert atomised_into + atom_of
    // are NULL on every row.
    let conn = ai_memory::db::open(tmp.path()).expect("re-open after seed");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 100, "v36: row preservation broken");

    let with_atomised_into: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE atomised_into IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count atomised_into");
    assert_eq!(
        with_atomised_into, 0,
        "v36: legacy-seeded rows must have NULL atomised_into"
    );

    let with_atom_of: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE atom_of IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count atom_of");
    assert_eq!(
        with_atom_of, 0,
        "v36: legacy-seeded rows must have NULL atom_of"
    );
}

// -------------------------------------------------------------------
// MemoryLinkRelation::DerivesFrom — round-trip
// -------------------------------------------------------------------

#[test]
fn test_relation_derives_from_serialises() {
    // Display impl: the variant prints as the canonical snake_case
    // wire string.
    assert_eq!(
        format!("{}", MemoryLinkRelation::DerivesFrom),
        "derives_from"
    );
    assert_eq!(MemoryLinkRelation::DerivesFrom.as_str(), "derives_from");

    // FromStr impl: the canonical wire string parses back to the variant.
    let parsed = MemoryLinkRelation::from_str("derives_from").expect("parse derives_from");
    assert_eq!(parsed, MemoryLinkRelation::DerivesFrom);

    // serde round-trip through JSON to pin the wire format.
    let json = serde_json::to_string(&MemoryLinkRelation::DerivesFrom).expect("serialize");
    assert_eq!(json, r#""derives_from""#);
    let de: MemoryLinkRelation = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(de, MemoryLinkRelation::DerivesFrom);

    // Make sure `derives_from` and `derived_from` are NOT aliases.
    // The two relations are semantically distinct (atomisation
    // provenance vs. consolidation provenance) and a slip between
    // them would silently mis-tag wiring.
    assert_ne!(
        MemoryLinkRelation::from_str("derives_from"),
        MemoryLinkRelation::from_str("derived_from"),
        "v36: derives_from must not alias derived_from"
    );
}

// -------------------------------------------------------------------
// CHECK constraint extension
// -------------------------------------------------------------------

#[test]
fn test_relation_derives_from_check_constraint() {
    // The v36 migration extends the closed-taxonomy CHECK on
    // `memory_links.relation` to admit `derives_from` (atomisation
    // provenance). Bogus values must still fail.
    let (conn, _tmp) = fresh_db_via_open();

    // Seed two memories so FKs are satisfiable.
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
         VALUES ('parent', 'mid', 'wt-1-a', 'parent-title', 'body', '2026-05-14T00:00:00Z', '2026-05-14T00:00:00Z')",
        [],
    )
    .expect("seed parent");
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
         VALUES ('atom-1', 'mid', 'wt-1-a', 'atom-title', 'body', '2026-05-14T00:00:00Z', '2026-05-14T00:00:00Z')",
        [],
    )
    .expect("seed atom");

    // Accepted: `derives_from`.
    conn.execute(
        "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
         VALUES ('atom-1', 'parent', 'derives_from', '2026-05-14T00:00:00Z')",
        [],
    )
    .expect("v36: CHECK must accept 'derives_from'");

    // The pre-v36 closed set must still be accepted.
    for rel in [
        "related_to",
        "supersedes",
        "contradicts",
        "derived_from",
        "reflects_on",
    ] {
        // Use a new (source, target, relation) PK tuple for each
        // relation so the PK constraint doesn't trip first. We
        // re-use the same row pair; the `relation` column is part
        // of the PK so each insert is unique.
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
             VALUES ('atom-1', 'parent', ?1, '2026-05-14T00:00:00Z')",
            rusqlite::params![rel],
        )
        .unwrap_or_else(|e| panic!("v36: CHECK must still accept '{rel}'; got error: {e:?}"));
    }

    // Rejected: an off-taxonomy value still fails the CHECK.
    let err = conn.execute(
        "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
         VALUES ('atom-1', 'parent', 'bogus', '2026-05-14T00:00:00Z')",
        [],
    );
    assert!(
        err.is_err(),
        "v36: CHECK must still reject off-taxonomy 'bogus' relation"
    );
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.to_lowercase().contains("check"),
        "v36: bogus rejection must mention CHECK; got: {msg}"
    );
}

// -------------------------------------------------------------------
// Capabilities endpoint — extends s75 pattern.
// -------------------------------------------------------------------

fn build_sqlite_app_state() -> (AppState, tempfile::NamedTempFile) {
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
async fn test_capabilities_db_schema_version_reports_36() {
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
    assert!(ready, "WT-1-A: in-process HTTP daemon never bound");

    let client = reqwest::Client::new();
    let caps: Value = client
        .get(format!("http://{addr}/api/v1/capabilities"))
        .send()
        .await
        .expect("capabilities GET")
        .json()
        .await
        .expect("capabilities body");

    let v = caps
        .get("db_schema_version")
        .and_then(Value::as_i64)
        .expect("WT-1-A: db_schema_version must be a JSON integer");

    assert_eq!(
        v, 42,
        "WT-1-A+QW-2+Form 4+Form 5+Cluster-C+Cluster-G+PERF-8: \
         capabilities.db_schema_version must be 42 after the \
         atomisation-foundation bump (35→36), persona-as-artifact \
         bump (36→37), Form 4 source-uri provenance bump (37→38), \
         Form 5 confidence calibration bump (38→39), Cluster-C \
         signed-events DLQ bump (39→40), Cluster-G shadow-retention \
         denormalised source column bump (40→41), and polish PERF-8 \
         auto_persona mentioned_entity_id bump (41→42). Drift here \
         means the migrate ladder skipped one of those steps or the \
         SAL `schema_version()` lookup is reading the wrong source."
    );

    shutdown.notify_one();
    let _ = handle.await;
}
