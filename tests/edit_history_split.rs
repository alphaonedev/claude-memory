// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 Provenance Gap 5 (issue #888) — split-write-path
//! regression pin.
//!
//! * `EditSource::Human` (default) — in-place mutation, content
//!   overwritten, version bumped, no archive created.
//! * `EditSource::Llm` / `EditSource::Hook` — append-and-archive:
//!   a NEW memory row is minted carrying the patched content; the
//!   OLD row is archived with `archive_reason='superseded'` so
//!   `memory_archive_list` can rewind to read the pre-edit body.
//!
//! Mirrors mem9's pattern: human-typed corrections mutate in place
//! (rewinding is rare); programmatic LLM rewrites preserve the
//! prior content so "what did we believe before?" stays answerable.

use ai_memory::db;
use ai_memory::models::{EditSource, Memory, Tier};
use rusqlite::params;
use std::path::Path;

fn open_test_db() -> rusqlite::Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

fn make_memory(title: &str, body: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        content: body.to_string(),
        namespace: "edit-history-test".to_string(),
        tier: Tier::Mid,
        created_at: now.clone(),
        updated_at: now,
        ..Default::default()
    }
}

fn count_archived(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM archived_memories", [], |r| r.get(0))
        .expect("count archived")
}

#[test]
fn human_edit_path_mutates_in_place_and_bumps_version() {
    let conn = open_test_db();
    let mem = make_memory("title", "v1 body");
    let id = db::insert(&conn, &mem).expect("insert");
    let before = count_archived(&conn);
    let (found, _) = db::update(
        &conn,
        &id,
        None,
        Some("v2 body — human typed"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("human update");
    assert!(found);
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(stored.content, "v2 body — human typed");
    assert_eq!(stored.version, 2, "in-place update bumps version");
    let after = count_archived(&conn);
    assert_eq!(before, after, "human edit must NOT create an archived row");
}

#[test]
fn llm_edit_appends_new_row_and_archives_old() {
    let conn = open_test_db();
    let mem = make_memory("ml-rewrite", "old body — what we believed Tuesday");
    let id = db::insert(&conn, &mem).expect("insert");
    let original_archive_count = count_archived(&conn);
    let result = db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,
        Some("new body — LLM rewrite"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        EditSource::Llm,
    )
    .expect("llm update");
    assert_eq!(result.archived_id, id, "OLD row id flows back");
    assert_ne!(result.new_id, id, "NEW row gets a fresh id");

    // OLD row is gone from `memories`, present in `archived_memories`
    // with reason='superseded'.
    assert!(
        db::get(&conn, &id).expect("get").is_none(),
        "OLD row removed from live table"
    );
    let archived_count = count_archived(&conn);
    assert_eq!(
        archived_count,
        original_archive_count + 1,
        "exactly one new archive row"
    );
    let reason: String = conn
        .query_row(
            "SELECT archive_reason FROM archived_memories WHERE id = ?1",
            params![&id],
            |r| r.get(0),
        )
        .expect("read archive reason");
    assert_eq!(reason, "superseded");
    let archived_body: String = conn
        .query_row(
            "SELECT content FROM archived_memories WHERE id = ?1",
            params![&id],
            |r| r.get(0),
        )
        .expect("read archived body");
    assert_eq!(
        archived_body, "old body — what we believed Tuesday",
        "archived row preserves PRE-EDIT content"
    );

    // NEW row carries the patched content + stamps edit_source +
    // superseded_id in metadata so downstream observers know the
    // lineage.
    let new_mem = db::get(&conn, &result.new_id)
        .expect("get")
        .expect("new row present");
    assert_eq!(new_mem.content, "new body — LLM rewrite");
    assert_eq!(new_mem.version, 1, "NEW row starts fresh at version=1");
    assert_eq!(
        new_mem.metadata.get("edit_source").and_then(|v| v.as_str()),
        Some("llm"),
        "metadata.edit_source stamped on NEW row"
    );
    assert_eq!(
        new_mem
            .metadata
            .get("superseded_id")
            .and_then(|v| v.as_str()),
        Some(id.as_str()),
        "metadata.superseded_id points back to archived OLD row"
    );
}

#[test]
fn hook_edit_uses_same_path_as_llm_and_archives_with_superseded_reason() {
    let conn = open_test_db();
    let mem = make_memory("hook-rewrite", "pre-hook body");
    let id = db::insert(&conn, &mem).expect("insert");
    let result = db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,
        Some("post-hook body"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        EditSource::Hook,
    )
    .expect("hook update");
    let reason: String = conn
        .query_row(
            "SELECT archive_reason FROM archived_memories WHERE id = ?1",
            params![&result.archived_id],
            |r| r.get(0),
        )
        .expect("read archive reason");
    assert_eq!(reason, "superseded");
    let new_mem = db::get(&conn, &result.new_id)
        .expect("get")
        .expect("present");
    assert_eq!(
        new_mem.metadata.get("edit_source").and_then(|v| v.as_str()),
        Some("hook"),
    );
}

#[test]
fn archive_list_returns_old_content_after_llm_supersede() {
    // Rewind contract: after an LLM-driven supersede, calling the
    // archive surface must surface the OLD content (substrate-level
    // equivalent of "show me Tuesday's body").
    let conn = open_test_db();
    let mem = make_memory("rewind-pin", "tuesday's body");
    let id = db::insert(&conn, &mem).expect("insert");
    db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,
        Some("today's body — LLM rewrite"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        EditSource::Llm,
    )
    .expect("llm update");
    // Walk archived_memories directly (the same query backing the
    // `memory_archive_list` MCP tool).
    let archived: Vec<(String, String, String)> = conn
        .prepare("SELECT id, content, archive_reason FROM archived_memories WHERE id = ?1")
        .expect("prepare")
        .query_map(params![&id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .expect("query_map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect");
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].1, "tuesday's body");
    assert_eq!(archived[0].2, "superseded");
}

#[test]
fn append_and_archive_honors_expected_version_gate() {
    // Gap 1 + Gap 5 compose: the optimistic-concurrency gate
    // applies to the append-and-archive path too.
    let conn = open_test_db();
    let mem = make_memory("compose-pin", "v1 body");
    let id = db::insert(&conn, &mem).expect("insert");
    // Bump version once via a Human in-place update so the live
    // row now has version=2.
    db::update(
        &conn,
        &id,
        None,
        Some("v2 body"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("bump");
    let err = db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,
        Some("LLM supersede with stale expected_version"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(1), // STALE — actual stored version is 2.
        EditSource::Llm,
    )
    .expect_err("must refuse with VersionConflict");
    let vc = err
        .downcast_ref::<ai_memory::storage::VersionConflict>()
        .expect("typed VersionConflict on the supersede gate");
    assert_eq!(vc.expected, 1);
    assert_eq!(vc.current, 2);
    // No archive was created — the gate refused before the move.
    assert_eq!(count_archived(&conn), 0);
}
