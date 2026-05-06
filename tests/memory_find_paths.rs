// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 J7 — `memory_find_paths` MCP tool integration tests.
//!
//! J7 ships a new MCP tool `memory_find_paths(source_id, target_id,
//! max_depth?, max_results?)` that returns up to N paths through the
//! KG between two memories using BFS with cycle detection. The
//! implementation dispatches on the resolved `KgBackend`: `SQLite` uses
//! a recursive CTE through `db::find_paths`; Postgres deployments
//! either fall back to the same recursive-CTE shape via
//! `PostgresStore::find_paths_cte` or use Cypher
//! `MATCH p = (s)-[*..N]-(t) RETURN p` via
//! `PostgresStore::find_paths_cypher` when AGE is installed.
//!
//! Scenarios pinned here:
//! 1. Linear 3-hop chain (A→B→C→D) — `find_paths(A, D)` returns the
//!    one canonical path `[A, B, C, D]`.
//! 2. Multiple paths between same endpoints — `find_paths(A, D)`
//!    returns both routes (a diamond graph), shortest-first.
//! 3. Disconnected pair — `find_paths(A, X)` returns an empty list.
//!
//! The walk treats links as undirected so callers don't need to
//! reverse-orient the chain when their KG was modeled with
//! `derived_from` pointing one way and `supersedes` the other.

use ai_memory::db;
use ai_memory::mcp;
use ai_memory::models;
use chrono::Utc;
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn open_db() -> (rusqlite::Connection, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("ai-memory-find-paths.db");
    let conn = db::open(&path).expect("db::open");
    (conn, tmp)
}

fn seed(conn: &rusqlite::Connection, title: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let mem = models::Memory {
        id: id.clone(),
        tier: models::Tier::Long,
        namespace: "j7-test".to_string(),
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

fn call(conn: &rusqlite::Connection, source_id: &str, target_id: &str) -> Value {
    mcp::handle_find_paths(
        conn,
        &json!({
            "source_id": source_id,
            "target_id": target_id,
        }),
    )
    .expect("handle_find_paths Ok")
}

// ---------------------------------------------------------------------------
// Case 1 — linear 3-hop chain
// ---------------------------------------------------------------------------

#[test]
fn find_paths_linear_three_hop_chain_returns_single_path() {
    let (conn, _tmp) = open_db();
    let a = seed(&conn, "a");
    let b = seed(&conn, "b");
    let c = seed(&conn, "c");
    let d = seed(&conn, "d");
    db::create_link(&conn, &a, &b, "related_to").unwrap();
    db::create_link(&conn, &b, &c, "related_to").unwrap();
    db::create_link(&conn, &c, &d, "related_to").unwrap();

    let val = call(&conn, &a, &d);
    let paths = val["paths"].as_array().expect("paths array");
    assert_eq!(val["count"], 1, "single canonical path expected: {val}");
    assert_eq!(paths.len(), 1);

    let chain: Vec<&str> = paths[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(chain, vec![a.as_str(), b.as_str(), c.as_str(), d.as_str()]);
}

// ---------------------------------------------------------------------------
// Case 2 — multiple paths (diamond graph)
// ---------------------------------------------------------------------------

#[test]
fn find_paths_diamond_graph_returns_both_routes_shortest_first() {
    // A --> B --> D
    // A --> C --> D
    // Both routes are 3 nodes long, so both must come back.
    let (conn, _tmp) = open_db();
    let a = seed(&conn, "a");
    let b = seed(&conn, "b");
    let c = seed(&conn, "c");
    let d = seed(&conn, "d");
    db::create_link(&conn, &a, &b, "related_to").unwrap();
    db::create_link(&conn, &b, &d, "related_to").unwrap();
    db::create_link(&conn, &a, &c, "related_to").unwrap();
    db::create_link(&conn, &c, &d, "related_to").unwrap();

    let val = call(&conn, &a, &d);
    let paths = val["paths"].as_array().expect("paths array");
    assert_eq!(val["count"], 2, "two parallel routes expected: {val}");
    assert_eq!(paths.len(), 2);

    // Each path starts at A and ends at D.
    for path in paths {
        let chain: Vec<&str> = path
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(chain.first(), Some(&a.as_str()), "source first: {path}");
        assert_eq!(chain.last(), Some(&d.as_str()), "target last: {path}");
        assert_eq!(chain.len(), 3, "diamond hop = 3 nodes: {path}");
    }

    // The two middle nodes across the two paths must be {B, C}.
    let mids: std::collections::HashSet<&str> = paths
        .iter()
        .map(|p| p.as_array().unwrap()[1].as_str().unwrap())
        .collect();
    let expected: std::collections::HashSet<&str> = [b.as_str(), c.as_str()].into_iter().collect();
    assert_eq!(mids, expected, "diamond mid-hops must be both B and C");
}

// ---------------------------------------------------------------------------
// Case 3 — no path / disconnected pair
// ---------------------------------------------------------------------------

#[test]
fn find_paths_disconnected_pair_returns_empty_paths() {
    let (conn, _tmp) = open_db();
    let a = seed(&conn, "a");
    let b = seed(&conn, "b");
    let c = seed(&conn, "c");
    // x is isolated — no links touch it.
    let x = seed(&conn, "x");

    // Add an unrelated link in the graph so memory_links isn't empty
    // (covers the "edges exist but not on this pair" branch).
    db::create_link(&conn, &a, &b, "related_to").unwrap();
    db::create_link(&conn, &b, &c, "related_to").unwrap();

    let val = call(&conn, &a, &x);
    let paths = val["paths"].as_array().expect("paths array");
    assert_eq!(val["count"], 0, "disconnected pair = no paths: {val}");
    assert!(paths.is_empty(), "disconnected pair = empty list: {val}");
}
