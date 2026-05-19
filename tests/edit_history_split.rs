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
fn edit_source_enum_parse_roundtrip_through_wire_strings() {
    // AC pin: the wire strings ("human" / "llm" / "hook") roundtrip
    // through `EditSource::as_str` ↔ `EditSource::from_str`. The MCP
    // `memory_update --edit-source X` flow depends on this.
    for src in [EditSource::Human, EditSource::Llm, EditSource::Hook] {
        let s = src.as_str();
        let back = EditSource::from_str(s).expect("known wire string");
        assert_eq!(back, src);
    }
    assert!(
        EditSource::from_str("garbage").is_none(),
        "unknown value ⇒ None so the caller can fall back to default"
    );
    // Default for back-compat is Human (the v0.6.x in-place behaviour).
    assert_eq!(EditSource::default(), EditSource::Human);
}

#[test]
fn edit_source_appends_and_archives_predicate_only_true_for_llm_and_hook() {
    // AC pin: the substrate uses `appends_and_archives()` as the
    // routing gate. Human is the in-place path; both Llm and Hook
    // route through the append-and-archive supersede path.
    assert!(!EditSource::Human.appends_and_archives());
    assert!(EditSource::Llm.appends_and_archives());
    assert!(EditSource::Hook.appends_and_archives());
}

#[test]
fn llm_supersede_preserves_tier_namespace_and_tags_from_old_row() {
    // AC pin: when a supersede patch omits tier / namespace / tags,
    // the NEW row inherits them from the OLD row. The patch only
    // replaces what the caller asked for.
    let conn = open_test_db();
    let mut mem = make_memory("inherit", "v1");
    mem.tier = Tier::Long;
    mem.namespace = "inherited-ns".to_string();
    mem.tags = vec!["t-a".to_string(), "t-b".to_string()];
    let id = db::insert(&conn, &mem).expect("insert");
    let result = db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,       // title
        Some("v2"), // content
        None,       // tier
        None,       // namespace
        None,       // tags
        None,       // priority
        None,       // confidence
        None,       // expires_at
        None,       // metadata
        None,       // source_uri
        None,       // expected_version
        EditSource::Llm,
    )
    .expect("llm");
    let new_mem = db::get(&conn, &result.new_id)
        .expect("get")
        .expect("present");
    assert_eq!(new_mem.tier, Tier::Long, "tier inherited");
    assert_eq!(new_mem.namespace, "inherited-ns", "namespace inherited");
    assert_eq!(new_mem.tags, vec!["t-a".to_string(), "t-b".to_string()]);
}

#[test]
fn llm_supersede_fails_clean_when_old_id_does_not_exist() {
    // AC pin: a supersede call against a non-existent id fails with
    // a "memory not found" Err — does NOT silently archive a phantom
    // row or panic.
    let conn = open_test_db();
    let err = db::update_with_archive_on_supersede(
        &conn,
        "11111111-2222-3333-4444-555555555555",
        None,
        Some("body"),
        None,
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
    .expect_err("missing id must Err");
    let msg = err.to_string();
    assert!(
        msg.contains("not found") || msg.contains("memory not found"),
        "error message names missing-id: {msg}"
    );
    // No archive row was created.
    assert_eq!(count_archived(&conn), 0);
}

#[test]
fn supersede_result_carries_distinct_archived_and_new_ids() {
    // AC pin: `SupersedeResult` reports the OLD id and the NEW id
    // separately. The downstream MCP response in src/mcp/tools/update.rs
    // surfaces both as `superseded_id` + `new_id`.
    let conn = open_test_db();
    let mem = make_memory("two-ids", "body");
    let id = db::insert(&conn, &mem).expect("insert");
    let result = db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,
        Some("patched"),
        None,
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
    .expect("llm");
    assert_eq!(result.archived_id, id);
    assert_ne!(result.new_id, id, "NEW id distinct from OLD");
    // Round-trip both ids through Display + Debug so the substrate's
    // logging surfaces (audit, tracing) stay readable.
    assert!(!format!("{result:?}").is_empty());
}

#[test]
fn human_edit_path_does_not_stamp_edit_source_in_metadata() {
    // AC pin: the metadata.edit_source stamp lives ONLY on rows minted
    // through the append-and-archive supersede path. A normal Human
    // in-place mutation leaves the metadata untouched so legacy
    // dashboards keying off metadata shape don't see a phantom field.
    let conn = open_test_db();
    let mem = make_memory("no-stamp", "body");
    let id = db::insert(&conn, &mem).expect("insert");
    db::update(
        &conn,
        &id,
        None,
        Some("body-2"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("human");
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert!(
        stored.metadata.get("edit_source").is_none(),
        "human in-place mutation must NOT stamp edit_source in metadata"
    );
    assert!(
        stored.metadata.get("superseded_id").is_none(),
        "human in-place mutation must NOT stamp superseded_id in metadata"
    );
}

#[test]
fn llm_supersede_starts_new_row_at_version_one() {
    // AC pin: the NEW row minted by a supersede starts at version=1
    // (a fresh row, not a continuation of the OLD row's version chain).
    // The OLD row's pre-supersede version is preserved in archived_memories.
    let conn = open_test_db();
    let mem = make_memory("version-restart", "body");
    let id = db::insert(&conn, &mem).expect("insert");
    // Bump the OLD row to version=3 via two in-place updates first.
    db::update(
        &conn,
        &id,
        None,
        Some("body-2"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("bump-1");
    db::update(
        &conn,
        &id,
        None,
        Some("body-3"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("bump-2");
    let result = db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,
        Some("body-4-llm"),
        None,
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
    .expect("supersede");
    let new_mem = db::get(&conn, &result.new_id)
        .expect("get")
        .expect("present");
    assert_eq!(
        new_mem.version, 1,
        "supersede mints a fresh row at version=1 regardless of OLD row's pre-supersede version"
    );
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
