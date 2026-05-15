// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 v0.7.1-fold (#687/#688, schema v33) regression suite for the
//! SQL-side `CHECK (relation IN (...))` constraint on `memory_links`.
//!
//! The constraint exists in two layers of defence:
//!
//! 1. `crate::validate::validate_relation` rejects unknown relations
//!    before they reach SQL — every CLI / MCP / HTTP write path runs
//!    through this validator. The Rust gate is well-tested elsewhere.
//!
//! 2. The SQL-side `CHECK` clause (this test) catches direct-SQL
//!    writes that bypass the Rust validator. The pre-v33 surface used
//!    `BEFORE INSERT / UPDATE` triggers with `RAISE(ABORT, ...)`; v33
//!    promotes those to a real column-level CHECK clause baked into
//!    the table definition so the constraint is visible in `.schema`
//!    output and operator-inspectable.
//!
//! These tests open a fresh `db::open(":memory:")` connection (which
//! runs the full migration ladder to v33), then write directly via
//! `rusqlite::Connection::execute` — bypassing the validator entirely
//! — and assert the storage layer refuses bad relation values. Every
//! accepted relation (per `crate::validate::VALID_RELATIONS`) is
//! pinned to make any future taxonomy drift loud.

use ai_memory::db;
use ai_memory::models::{Memory, Tier};
use rusqlite::Connection;
use std::path::Path;

/// Open an in-memory DB walked through the full migration ladder to v33.
fn open_test_db() -> Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

/// Insert two memories so a link can reference them. Returns `(src_id, tgt_id)`.
fn seed_two_memories(conn: &Connection) -> (String, String) {
    let now = chrono::Utc::now().to_rfc3339();
    let src = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: "check-constraint-test".into(),
        title: "source row".into(),
        content: "lorem".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let tgt = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: "check-constraint-test".into(),
        title: "target row".into(),
        content: "ipsum".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let src_id = db::insert(conn, &src).expect("insert source");
    let tgt_id = db::insert(conn, &tgt).expect("insert target");
    (src_id, tgt_id)
}

/// Direct-SQL link insert that bypasses `validate_relation`. Returns the
/// raw `rusqlite::Error` so callers can match on the constraint shape.
fn raw_insert_link(
    conn: &Connection,
    src: &str,
    tgt: &str,
    relation: &str,
) -> Result<usize, rusqlite::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![src, tgt, relation, now],
    )
}

#[test]
fn sqlite_check_constraint_refuses_invalid_relation_on_insert() {
    // Direct-SQL write with `relation = 'invalid_relation'` must fail
    // with a CHECK constraint violation. The shape proves the SQL-side
    // gate is active, not the Rust validator (which we bypass by
    // poking `memory_links` directly).
    let conn = open_test_db();
    let (src, tgt) = seed_two_memories(&conn);

    let err = raw_insert_link(&conn, &src, &tgt, "invalid_relation")
        .expect_err("CHECK constraint must refuse 'invalid_relation'");

    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("check") || msg.contains("constraint"),
        "expected CHECK/constraint failure, got: {err}",
    );
}

#[test]
fn sqlite_check_constraint_refuses_empty_relation() {
    // Empty string is not in the closed taxonomy. The DEFAULT 'related_to'
    // is only applied when the column is OMITTED from the INSERT — an
    // explicit empty string still goes through the CHECK.
    let conn = open_test_db();
    let (src, tgt) = seed_two_memories(&conn);

    let err = raw_insert_link(&conn, &src, &tgt, "")
        .expect_err("CHECK constraint must refuse empty relation");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("check") || msg.contains("constraint"),
        "expected CHECK/constraint failure, got: {err}",
    );
}

#[test]
fn sqlite_check_constraint_refuses_obvious_sql_injection_shape() {
    // A literal that's "almost" valid but isn't on the closed list.
    // Belt + suspenders against a future drift that accidentally widens
    // the predicate (e.g. LIKE 'related%').
    let conn = open_test_db();
    let (src, tgt) = seed_two_memories(&conn);

    let err = raw_insert_link(&conn, &src, &tgt, "related_to_evil")
        .expect_err("CHECK constraint must refuse 'related_to_evil'");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("check") || msg.contains("constraint"),
        "expected CHECK/constraint failure, got: {err}",
    );
}

#[test]
fn sqlite_check_constraint_accepts_every_canonical_relation() {
    // Every value in `crate::validate::VALID_RELATIONS` must be accepted
    // by the SQL-side CHECK. The seven inserts each use a unique
    // (source, target, relation) PK so we don't conflict across the
    // closed taxonomy. Failure on any one of these pins a regression
    // that the CHECK clause and the validator have drifted out of sync.
    let conn = open_test_db();
    let (src, tgt) = seed_two_memories(&conn);

    for rel in [
        "related_to",
        "supersedes",
        "contradicts",
        "derived_from",
        "reflects_on",
    ] {
        raw_insert_link(&conn, &src, &tgt, rel)
            .unwrap_or_else(|e| panic!("canonical relation `{rel}` was rejected: {e}"));
    }
}

#[test]
fn sqlite_check_constraint_refuses_invalid_relation_on_update() {
    // The trigger-era predecessor (v23) covered both INSERT and UPDATE
    // via separate triggers. The column-level CHECK clause is enforced
    // on every write — INSERT and UPDATE alike. Verify the UPDATE arm
    // refuses a swap to an out-of-taxonomy relation.
    let conn = open_test_db();
    let (src, tgt) = seed_two_memories(&conn);

    raw_insert_link(&conn, &src, &tgt, "related_to").expect("canonical insert");

    let err = conn
        .execute(
            "UPDATE memory_links SET relation = 'invalid_relation' WHERE source_id = ?1 AND target_id = ?2",
            rusqlite::params![&src, &tgt],
        )
        .expect_err("CHECK must refuse UPDATE to 'invalid_relation'");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("check") || msg.contains("constraint"),
        "expected CHECK/constraint failure on UPDATE, got: {err}",
    );
}

#[test]
fn sqlite_check_clause_visible_in_table_schema() {
    // The whole motivation behind promoting the v23 triggers to a
    // declarative CHECK clause is operator-visibility: `.schema
    // memory_links` should show the predicate. We probe sqlite_master
    // for the CREATE TABLE statement and assert it contains the closed
    // taxonomy literals. A future operator running `sqlite3 ai-memory.db
    // '.schema memory_links'` will see the CHECK in the output.
    let conn = open_test_db();

    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='memory_links'",
            [],
            |r| r.get(0),
        )
        .expect("memory_links DDL row present");

    let lower = ddl.to_ascii_lowercase();
    assert!(
        lower.contains("check"),
        "CREATE TABLE memory_links DDL should contain a CHECK clause; got:\n{ddl}",
    );
    for rel in [
        "related_to",
        "supersedes",
        "contradicts",
        "derived_from",
        "reflects_on",
    ] {
        assert!(
            ddl.contains(rel),
            "CHECK clause should mention canonical relation `{rel}`; got:\n{ddl}",
        );
    }
}
