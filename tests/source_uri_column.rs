// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 Provenance Gap 2 (issue #885) — `source_uri` first-class
//! column regression pin. 100 memories seeded with `source_uri`;
//! reciprocal "memories from this document" lookup must use the
//! partial `idx_memories_source_uri` index (`EXPLAIN QUERY PLAN`
//! returns `SEARCH ... USING INDEX`, not a `SCAN`).
//!
//! Also pins the v46 backfill migration: when a legacy row stored
//! the URI under `metadata.source_uri` or as the first
//! `citations[]` entry, the column gets promoted automatically on
//! migrate.

use ai_memory::db;
use ai_memory::models::{Citation, Memory, Tier};
use rusqlite::params;
use std::path::Path;

fn open_test_db() -> rusqlite::Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

fn make_memory_with_uri(title: &str, uri: Option<&str>) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        content: format!("content for {title}"),
        namespace: "source-uri-test".to_string(),
        tier: Tier::Mid,
        created_at: now.clone(),
        updated_at: now,
        source_uri: uri.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn explain_plan_uses_partial_source_uri_index() {
    let conn = open_test_db();
    // Seed 100 rows with source_uri so the planner has incentive
    // to pick the index over a sequential scan.
    for i in 0..100 {
        let mem = make_memory_with_uri(
            &format!("doc-{i}"),
            Some(&format!("uri:https://example.com/{i}")),
        );
        db::insert(&conn, &mem).expect("insert");
    }
    let plan: Vec<String> = conn
        .prepare("EXPLAIN QUERY PLAN SELECT id FROM memories WHERE source_uri = ?1")
        .expect("prepare")
        .query_map(params!["uri:https://example.com/42"], |r| {
            r.get::<_, String>(3)
        })
        .expect("query_map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect");
    let detail = plan.join(" | ");
    assert!(
        detail.contains("USING INDEX idx_memories_source_uri")
            || detail.contains("USING COVERING INDEX idx_memories_source_uri"),
        "EXPLAIN PLAN must hit the partial source_uri index — got: {detail}"
    );
}

#[test]
fn list_by_source_uri_returns_all_matching_rows() {
    let conn = open_test_db();
    let uri = "doc:contract-123";
    // Three rows share the URI, two don't.
    for i in 0..3 {
        let mem = make_memory_with_uri(&format!("clause-{i}"), Some(uri));
        db::insert(&conn, &mem).expect("insert");
    }
    let other_a = make_memory_with_uri("other-a", Some("doc:other"));
    let other_b = make_memory_with_uri("other-b", None);
    db::insert(&conn, &other_a).expect("insert");
    db::insert(&conn, &other_b).expect("insert");

    let hits = db::list_by_source_uri(&conn, uri, None, None).expect("list");
    assert_eq!(hits.len(), 3, "exactly three rows share the URI");
    for h in &hits {
        assert_eq!(h.source_uri.as_deref(), Some(uri));
    }
}

#[test]
fn v46_backfill_promotes_metadata_source_uri_to_column() {
    let conn = open_test_db();
    // Insert a legacy-shaped row directly: source_uri column empty,
    // metadata.source_uri carries the URI. Simulates a pre-Form-4
    // operator write.
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories
            (id, tier, namespace, title, content, tags, priority, confidence, source,
             access_count, created_at, updated_at, metadata, citations)
         VALUES (?1, 'mid', 'legacy', 'legacy-row', 'body', '[]', 5, 1.0, 'api',
                 0, ?2, ?3, ?4, '[]')",
        params![
            "11111111-2222-3333-4444-555555555555",
            now,
            now,
            r#"{"agent_id":"ai:legacy","source_uri":"uri:https://legacy.example.com/doc"}"#,
        ],
    )
    .expect("legacy insert");

    // Re-run the v46 backfill SQL manually (the migration ladder
    // already ran on open() — these UPDATE statements are
    // idempotent so a re-run is safe).
    conn.execute_batch(
        "UPDATE memories
            SET source_uri = json_extract(metadata, '$.source_uri')
          WHERE source_uri IS NULL
            AND json_valid(metadata) = 1
            AND json_extract(metadata, '$.source_uri') IS NOT NULL
            AND length(json_extract(metadata, '$.source_uri')) > 0;",
    )
    .expect("v46 backfill");
    let stored = db::get(&conn, "11111111-2222-3333-4444-555555555555")
        .expect("get")
        .expect("present");
    assert_eq!(
        stored.source_uri.as_deref(),
        Some("uri:https://legacy.example.com/doc"),
        "v46 backfill must lift metadata.source_uri into the column"
    );
}

#[test]
fn v46_backfill_lifts_first_citation_uri() {
    let conn = open_test_db();
    // Insert a row with empty source_uri but a populated citations[] —
    // the v46 backfill's second UPDATE promotes citations[0].uri.
    let now = chrono::Utc::now().to_rfc3339();
    let citations = serde_json::to_string(&vec![Citation {
        uri: "file:/srv/docs/spec.md".to_string(),
        accessed_at: now.clone(),
        hash: None,
        span: None,
    }])
    .unwrap();
    conn.execute(
        "INSERT INTO memories
            (id, tier, namespace, title, content, tags, priority, confidence, source,
             access_count, created_at, updated_at, metadata, citations)
         VALUES (?1, 'mid', 'legacy', 'cite-row', 'body', '[]', 5, 1.0, 'api',
                 0, ?2, ?3, '{}', ?4)",
        params!["22222222-3333-4444-5555-666666666666", now, now, citations,],
    )
    .expect("legacy insert with citations");
    conn.execute_batch(
        "UPDATE memories
            SET source_uri = json_extract(citations, '$[0].uri')
          WHERE source_uri IS NULL
            AND json_valid(citations) = 1
            AND json_array_length(citations) > 0
            AND json_extract(citations, '$[0].uri') IS NOT NULL
            AND length(json_extract(citations, '$[0].uri')) > 0;",
    )
    .expect("v46 backfill (citations)");
    let stored = db::get(&conn, "22222222-3333-4444-5555-666666666666")
        .expect("get")
        .expect("present");
    assert_eq!(
        stored.source_uri.as_deref(),
        Some("file:/srv/docs/spec.md"),
        "v46 backfill must promote citations[0].uri when source_uri is NULL"
    );
}

#[test]
fn insert_with_source_uri_persists_to_column_not_metadata() {
    // AC pin: `storage::insert` writes the `Memory.source_uri` field
    // into the first-class column, not metadata.source_uri. This is
    // the "new write path" half of #885; the v46 backfill covers the
    // legacy half.
    let conn = open_test_db();
    let mem = make_memory_with_uri("first-class-write", Some("doc:fresh-write"));
    let id = db::insert(&conn, &mem).expect("insert");
    // Verify directly via SQL — the column carries the value.
    let col: Option<String> = conn
        .query_row(
            "SELECT source_uri FROM memories WHERE id = ?1",
            params![&id],
            |r| r.get(0),
        )
        .expect("read column");
    assert_eq!(col.as_deref(), Some("doc:fresh-write"));
    // And via the typed accessor.
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(stored.source_uri.as_deref(), Some("doc:fresh-write"));
}

#[test]
fn insert_without_source_uri_leaves_column_null() {
    // AC pin: `insert` does NOT manufacture a source_uri value when
    // the field is None. The substrate-side "null column" is the
    // signal the v46 backfill needs to know it should attempt to
    // promote from metadata or citations.
    let conn = open_test_db();
    let mem = make_memory_with_uri("no-source", None);
    let id = db::insert(&conn, &mem).expect("insert");
    let col: Option<String> = conn
        .query_row(
            "SELECT source_uri FROM memories WHERE id = ?1",
            params![&id],
            |r| r.get(0),
        )
        .expect("read column");
    assert!(col.is_none(), "no source_uri ⇒ NULL column");
}

#[test]
fn list_by_source_uri_returns_zero_rows_for_unknown_uri() {
    // AC pin: passing a URI no row carries must return empty — NOT
    // silently fall back to "list all memories" (which would be a
    // catastrophic data-exfiltration bug).
    let conn = open_test_db();
    for i in 0..5 {
        let mem = make_memory_with_uri(&format!("present-{i}"), Some("doc:present"));
        db::insert(&conn, &mem).expect("insert");
    }
    let hits = db::list_by_source_uri(&conn, "doc:does-not-exist", None, None).expect("list");
    assert!(hits.is_empty(), "unknown URI ⇒ zero rows, not fallback-all");
}

#[test]
fn list_by_source_uri_respects_limit_param() {
    // AC pin: the `limit` arg is honored; without it the default cap
    // (200) applies. Both branches matter — the MCP search tool clamps
    // and the HTTP query handler passes `limit` through.
    let conn = open_test_db();
    for i in 0..10 {
        let mem = make_memory_with_uri(&format!("lim-{i}"), Some("doc:lim"));
        db::insert(&conn, &mem).expect("insert");
    }
    let three = db::list_by_source_uri(&conn, "doc:lim", None, Some(3)).expect("list");
    assert_eq!(three.len(), 3, "limit=3 caps the response");
    let unlimited = db::list_by_source_uri(&conn, "doc:lim", None, None).expect("list");
    assert_eq!(unlimited.len(), 10, "no limit ⇒ default cap is high enough");
}

#[test]
fn v46_backfill_is_idempotent_under_replay() {
    // AC pin: the v46 backfill SQL is idempotent — re-applying it on
    // a row that already carries a source_uri column value must NOT
    // overwrite it with `metadata.source_uri`. The WHERE clause
    // `source_uri IS NULL` is the load-bearing predicate.
    let conn = open_test_db();
    let mem = make_memory_with_uri("idem", Some("doc:already-set"));
    let id = db::insert(&conn, &mem).expect("insert");
    // Stamp a DIFFERENT URI under metadata.source_uri to prove the
    // backfill does not clobber the existing column value.
    conn.execute(
        "UPDATE memories SET metadata = ?1 WHERE id = ?2",
        params![
            r#"{"agent_id":"ai:test","source_uri":"doc:wrong-value-should-not-be-promoted"}"#,
            &id
        ],
    )
    .expect("stamp metadata");
    conn.execute_batch(
        "UPDATE memories
            SET source_uri = json_extract(metadata, '$.source_uri')
          WHERE source_uri IS NULL
            AND json_valid(metadata) = 1
            AND json_extract(metadata, '$.source_uri') IS NOT NULL
            AND length(json_extract(metadata, '$.source_uri')) > 0;",
    )
    .expect("v46 backfill replay");
    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(
        stored.source_uri.as_deref(),
        Some("doc:already-set"),
        "idempotent backfill must NOT clobber populated column"
    );
}

#[test]
fn migration_ladder_reaches_at_least_v46_on_fresh_db() {
    let conn = open_test_db();
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .expect("read schema_version");
    assert!(
        v >= 46,
        "migrate ladder must reach at least v46 on fresh open; got {v}"
    );
}
