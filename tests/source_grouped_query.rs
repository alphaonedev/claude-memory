// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 Provenance Gap 6 (issue #889) — source-grouped reciprocal
//! query regression pin. Five memories share the same `source_uri`;
//! a single call to `memory_search --source-uri X` (MCP) /
//! `?source_uri=X` (HTTP) / `db::list_by_source_uri` (substrate)
//! returns all five.
//!
//! Depends on Gap 2 (#885) — first-class `source_uri` column +
//! `idx_memories_source_uri` partial index — landing first.

use ai_memory::db;
use ai_memory::models::{Memory, Tier};
use std::path::Path;

fn open_test_db() -> rusqlite::Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

fn make_memory(title: &str, uri: Option<&str>) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        content: format!("body of {title}"),
        namespace: "source-grouped-test".to_string(),
        tier: Tier::Mid,
        created_at: now.clone(),
        updated_at: now,
        source_uri: uri.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn list_by_source_uri_returns_all_five_matches() {
    let conn = open_test_db();
    let shared = "doc:contract-2026-05-18";
    for i in 0..5 {
        let mem = make_memory(&format!("clause-{i}"), Some(shared));
        db::insert(&conn, &mem).expect("insert clause");
    }
    // Add two distractor rows whose source_uri does not match — must
    // be excluded from the result set.
    for i in 0..2 {
        let mem = make_memory(&format!("distractor-{i}"), Some("doc:other"));
        db::insert(&conn, &mem).expect("insert distractor");
    }
    let no_uri = make_memory("no-uri", None);
    db::insert(&conn, &no_uri).expect("insert no-uri");

    let hits = db::list_by_source_uri(&conn, shared, None, None).expect("list");
    assert_eq!(
        hits.len(),
        5,
        "exactly five memories share source_uri={shared}"
    );
    for h in &hits {
        assert_eq!(h.source_uri.as_deref(), Some(shared));
        assert!(h.title.starts_with("clause-"));
    }
}

#[test]
fn list_by_source_uri_respects_namespace_filter() {
    let conn = open_test_db();
    let uri = "uri:https://example.com/doc";
    // Two rows in ns A, three in ns B, all sharing the URI.
    for i in 0..2 {
        let mut mem = make_memory(&format!("a-{i}"), Some(uri));
        mem.namespace = "alpha".to_string();
        db::insert(&conn, &mem).expect("insert alpha");
    }
    for i in 0..3 {
        let mut mem = make_memory(&format!("b-{i}"), Some(uri));
        mem.namespace = "beta".to_string();
        db::insert(&conn, &mem).expect("insert beta");
    }
    let alpha = db::list_by_source_uri(&conn, uri, Some("alpha"), None).expect("alpha");
    assert_eq!(alpha.len(), 2);
    let beta = db::list_by_source_uri(&conn, uri, Some("beta"), None).expect("beta");
    assert_eq!(beta.len(), 3);
    let all = db::list_by_source_uri(&conn, uri, None, None).expect("all");
    assert_eq!(all.len(), 5);
}

#[test]
fn search_with_source_uri_narrows_fts_results_to_matching_uri() {
    let conn = open_test_db();
    let uri = "doc:abc";
    // Three rows match the URI; two share the word "alpha" in their
    // content but with a DIFFERENT URI. The search call composes FTS
    // ("alpha") with the URI gate ("doc:abc") so only the matching
    // intersection comes back.
    for i in 0..3 {
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            title: format!("matched-{i}"),
            content: "alpha tokens here".to_string(),
            namespace: "search-gate".to_string(),
            tier: Tier::Mid,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_uri: Some(uri.to_string()),
            ..Default::default()
        };
        db::insert(&conn, &mem).expect("insert matched");
    }
    for i in 0..2 {
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            title: format!("other-uri-{i}"),
            content: "alpha tokens here".to_string(),
            namespace: "search-gate".to_string(),
            tier: Tier::Mid,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_uri: Some("doc:other".to_string()),
            ..Default::default()
        };
        db::insert(&conn, &mem).expect("insert other-uri");
    }
    let hits = db::search_with_source_uri(
        &conn,
        "alpha",
        Some("search-gate"),
        None,
        50,
        None,
        None,
        None,
        None,
        None,
        None,
        false,
        Some(uri),
    )
    .expect("search");
    assert_eq!(hits.len(), 3, "URI gate excludes the doc:other rows");
    for h in &hits {
        assert_eq!(h.source_uri.as_deref(), Some(uri));
    }
}

#[test]
fn empty_uri_returns_zero_rows_not_all_rows() {
    // Defensive: passing a URI that no memory carries must return an
    // empty set, NOT silently fall back to "list everything".
    let conn = open_test_db();
    for i in 0..3 {
        let mem = make_memory(&format!("filler-{i}"), Some("doc:populated"));
        db::insert(&conn, &mem).expect("insert");
    }
    let hits = db::list_by_source_uri(&conn, "doc:does-not-exist", None, None).expect("list");
    assert!(hits.is_empty(), "unknown URI must return zero rows");
}
