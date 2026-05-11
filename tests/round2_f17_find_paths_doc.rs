// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F17 — `find_paths` surface guarantees.
//!
//! Two contracts are pinned here:
//!
//! 1. `max_depth > FIND_PATHS_MAX_DEPTH` returns an error whose message
//!    names the constant (`FIND_PATHS_MAX_DEPTH`) so an operator can
//!    grep the codebase to find the single tunable knob and the
//!    "contact maintainers" guidance in the doc-comment.
//!
//! 2. `find_paths` is **undirected** by design — registering a forward
//!    edge `A → B` must surface in both `find_paths(A, B)` *and*
//!    `find_paths(B, A)`. This is the symmetric-closure contract spelled
//!    out in the function's doc-comment alongside the directed `kg_query`
//!    counterpart.

use ai_memory::db;
use ai_memory::models;
use chrono::Utc;
use tempfile::TempDir;

fn open_db() -> (rusqlite::Connection, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("ai-memory-f17.db");
    let conn = db::open(&path).expect("db::open");
    (conn, tmp)
}

fn seed(conn: &rusqlite::Connection, title: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let mem = models::Memory {
        id: id.clone(),
        tier: models::Tier::Long,
        namespace: "f17-test".to_string(),
        title: title.to_string(),
        content: "x".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: models::default_metadata(),
    };
    db::insert(conn, &mem).expect("db::insert")
}

#[test]
fn find_paths_max_depth_over_cap_names_constant_in_error() {
    // Asking for one over the cap should surface the FIND_PATHS_MAX_DEPTH
    // identifier in the error so operators can grep the codebase. The
    // function's doc-comment also points at the "contact maintainers
    // after benchmarking" path; we assert that hint is in the message.
    let (conn, _tmp) = open_db();
    let a = seed(&conn, "a");
    let b = seed(&conn, "b");

    let err = db::find_paths(
        &conn,
        &a,
        &b,
        Some(db::FIND_PATHS_MAX_DEPTH + 1),
        None,
        false,
    )
    .expect_err("max_depth above cap must error");
    let msg = err.to_string();
    assert!(
        msg.contains("FIND_PATHS_MAX_DEPTH"),
        "error must name the constant so operators can grep — got: {msg}"
    );
    assert!(
        msg.contains("contact maintainers"),
        "error must point at the maintainer-escalation path — got: {msg}"
    );
    assert!(
        msg.contains(&format!("max_depth={}", db::FIND_PATHS_MAX_DEPTH + 1)),
        "error must echo the offending depth back — got: {msg}"
    );
}

#[test]
fn find_paths_at_cap_succeeds() {
    // Boundary: exactly FIND_PATHS_MAX_DEPTH must still succeed; the
    // off-by-one only kicks in for cap+1.
    let (conn, _tmp) = open_db();
    let a = seed(&conn, "a");
    let b = seed(&conn, "b");
    db::create_link(&conn, &a, &b, "related_to").unwrap();

    let paths = db::find_paths(&conn, &a, &b, Some(db::FIND_PATHS_MAX_DEPTH), None, false)
        .expect("max_depth at cap must succeed");
    assert_eq!(paths.len(), 1);
}

#[test]
fn find_paths_is_undirected_forward_edge_visible_from_either_endpoint() {
    // The doc-comment promises: "find_paths is UNDIRECTED (UNION of
    // forward + reverse edges); kg_query is DIRECTED."  Register a
    // forward edge A → B and assert find_paths sees the connection in
    // both directions.
    let (conn, _tmp) = open_db();
    let a = seed(&conn, "a");
    let b = seed(&conn, "b");
    db::create_link(&conn, &a, &b, "related_to").unwrap();

    let forward = db::find_paths(&conn, &a, &b, Some(2), None, false).unwrap();
    assert_eq!(
        forward.len(),
        1,
        "forward direction must surface the edge: {forward:?}"
    );
    assert_eq!(forward[0], vec![a.clone(), b.clone()]);

    let reverse = db::find_paths(&conn, &b, &a, Some(2), None, false).unwrap();
    assert_eq!(
        reverse.len(),
        1,
        "reverse direction must ALSO surface the edge (undirected): {reverse:?}"
    );
    assert_eq!(reverse[0], vec![b.clone(), a.clone()]);
}
