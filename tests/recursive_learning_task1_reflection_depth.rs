// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! v0.7.0 Task 1/8 (recursive learning) — `Memory.reflection_depth` regression suite.
//!
//! Pins:
//!   - [`Memory::default()`] leaves `reflection_depth = 0`.
//!   - Round-trip via JSON serde preserves a non-zero value.
//!   - Deserialising a JSON payload that omits the field defaults it to 0
//!     (the `#[serde(default)]` contract that keeps pre-v0.7.0 wire
//!     payloads readable).
//!   - SQLite `db::insert + db::get` round-trip carries `reflection_depth`.
//!   - SQLite `db::insert` upsert (same title+namespace) takes `MAX` of
//!     the stored and the incoming `reflection_depth` so a higher-depth
//!     reflection doesn't lose its provenance signal.
//!   - Postgres `store + get` round-trips the value (gated on
//!     `feature = "sal-postgres"` + `AI_MEMORY_TEST_POSTGRES_URL`).
//!   - Migration idempotency for SQLite: opening the DB twice never errors
//!     (the v29 ALTER is guarded by a column-existence probe).
//!
//! See `migrations/postgres/0013_v0700_reflection_depth.sql` and the
//! `migrate_v31` body in `src/store/postgres.rs` for the Postgres side.

use ai_memory::db;
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, Tier};
use chrono::Utc;

mod common;
#[cfg(feature = "sal-postgres")]
use common::postgres_url;

/// Test fixture builder. Returns a fully-populated `Memory` with the
/// supplied `reflection_depth` so individual tests don't have to repeat
/// the 16-field literal.
fn make_memory(title: &str, reflection_depth: i32) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: "recursive-learning-task1".to_string(),
        title: title.to_string(),
        content: "test content for reflection_depth round-trip".to_string(),
        tags: vec!["test".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent"}),
        reflection_depth,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

#[test]
fn default_memory_has_reflection_depth_zero() {
    let m = Memory::default();
    assert_eq!(
        m.reflection_depth, 0,
        "Memory::default() must leave reflection_depth at 0"
    );
}

#[test]
fn serde_roundtrip_preserves_reflection_depth() {
    // Forward direction: a Memory with reflection_depth=3 must survive a
    // serialize/deserialize cycle. The depth tracks recursion in the
    // substrate-native reflection tree (v0.7.0 Task 1/8 mission).
    let original = make_memory("serde-roundtrip", 3);
    let json = serde_json::to_string(&original).expect("serialize Memory");
    let parsed: Memory = serde_json::from_str(&json).expect("deserialize Memory");
    assert_eq!(
        parsed.reflection_depth, 3,
        "JSON round-trip must preserve reflection_depth"
    );
}

#[test]
fn deserialize_legacy_json_defaults_reflection_depth_to_zero() {
    // A pre-v0.7.0 wire payload (or any external client that hasn't
    // upgraded) won't carry the field. `#[serde(default)]` must fill it
    // in as 0 — the same value the SQL DEFAULT clause uses on the DB
    // side. Without this, federation peers running an older daemon
    // version would fail to deserialize anything we replicate to them
    // and vice versa.
    let now = Utc::now().to_rfc3339();
    let legacy = serde_json::json!({
        "id": "legacy-id",
        "tier": "mid",
        "namespace": "legacy-ns",
        "title": "legacy-title",
        "content": "legacy",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "access_count": 0,
        "created_at": now,
        "updated_at": now,
        "metadata": {},
        // reflection_depth intentionally omitted.
    });
    let parsed: Memory = serde_json::from_value(legacy).expect("deserialize legacy");
    assert_eq!(
        parsed.reflection_depth, 0,
        "legacy JSON without reflection_depth must default to 0"
    );
}

#[test]
fn sqlite_insert_get_roundtrips_reflection_depth() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let mem = make_memory("sqlite-roundtrip", 7);
    let id = db::insert(&conn, &mem).expect("insert memory");
    let fetched = db::get(&conn, &id)
        .expect("get memory")
        .expect("memory must exist");
    assert_eq!(
        fetched.reflection_depth, 7,
        "SQLite insert+get must round-trip reflection_depth"
    );
}

#[test]
fn sqlite_upsert_takes_max_reflection_depth() {
    // The schema-v29 INSERT ... ON CONFLICT DO UPDATE clause in
    // `db::insert` uses `MAX(memories.reflection_depth, excluded.reflection_depth)`
    // so a higher-depth reflection at the same (title, namespace) wins
    // — federating two peers' views of the same memory shouldn't lose
    // the recursion-depth provenance just because one peer happened to
    // write its row first.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();

    // First write: depth=2 lands.
    let first = make_memory("upsert-collision", 2);
    let id = db::insert(&conn, &first).expect("first insert");
    let after_first = db::get(&conn, &id).unwrap().unwrap();
    assert_eq!(after_first.reflection_depth, 2);

    // Second write at the SAME (title, namespace) with a HIGHER depth.
    // The upsert must take the max.
    let mut second = make_memory("upsert-collision", 5);
    // Force the (title, namespace) collision — keep title equal, drop
    // the second row's id since the conflict is resolved by the unique
    // index.
    second.title.clone_from(&first.title);
    second.namespace.clone_from(&first.namespace);
    db::insert(&conn, &second).expect("second insert (upsert path)");
    let after_second = db::get(&conn, &id).unwrap().unwrap();
    assert_eq!(
        after_second.reflection_depth, 5,
        "upsert must preserve the higher reflection_depth"
    );

    // Now write a LOWER depth — the existing higher value must win.
    let mut third = make_memory("upsert-collision", 1);
    third.title.clone_from(&first.title);
    third.namespace.clone_from(&first.namespace);
    db::insert(&conn, &third).expect("third insert (upsert path)");
    let after_third = db::get(&conn, &id).unwrap().unwrap();
    assert_eq!(
        after_third.reflection_depth, 5,
        "upsert must NOT downgrade reflection_depth"
    );
}

#[test]
fn sqlite_migration_is_idempotent() {
    // Open the same DB file twice — the v29 column-existence probe
    // must make the second open a no-op (the migration ladder short-
    // circuits when `MAX(version) >= CURRENT_SCHEMA_VERSION`, and even
    // a forced replay would skip the ADD COLUMN because the column
    // probe finds it already present).
    let tmp = tempfile::NamedTempFile::new().expect("temp file");
    let path = tmp.path().to_path_buf();

    let conn1 = db::open(&path).expect("first open");
    // Write something so the table is non-empty across the second open.
    let mem = make_memory("migration-idempotent", 4);
    let id = db::insert(&conn1, &mem).expect("first insert");
    drop(conn1);

    let conn2 = db::open(&path).expect("second open must not error");
    let fetched = db::get(&conn2, &id).unwrap().unwrap();
    assert_eq!(
        fetched.reflection_depth, 4,
        "row written under the v29 schema must survive a re-open"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Postgres parity — gated on the sal-postgres feature + live test DB.
// Mirrors the gating pattern in tests/g1_postgres_quota_increment_on_store.rs
// and tests/sal_v07_postgres_findings.rs.
// ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "sal-postgres")]
#[tokio::test]
async fn postgres_store_get_roundtrips_reflection_depth() {
    use ai_memory::store::CallerContext;
    use ai_memory::store::MemoryStore;
    use ai_memory::store::postgres::PostgresStore;

    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let store = PostgresStore::connect(&url).await.expect("connect");
    let ctx = CallerContext::for_agent("test-agent".to_string());
    // Use a unique title so re-runs against the same DB don't collide
    // with prior fixture rows.
    let title = format!("reflection_depth-pg-roundtrip-{}", uuid::Uuid::new_v4());
    let mut mem = make_memory(&title, 11);
    mem.namespace = "recursive-learning-task1-pg".to_string();

    let id = store.store(&ctx, &mem).await.expect("store");
    let fetched = store.get(&ctx, &id).await.expect("get");
    assert_eq!(
        fetched.reflection_depth, 11,
        "Postgres store+get must round-trip reflection_depth"
    );

    // Cleanup so re-runs stay deterministic.
    let _ = store.delete(&ctx, &id).await;
}
