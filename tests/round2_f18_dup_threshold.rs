// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F18 — `check_duplicate` exact-match short-circuit.
//!
//! Round-2 evidence: storing memory `M1` with content `C` and then
//! calling `check_duplicate` with the same content `C` returned
//! similarity ~0.92 instead of 1.0. Root cause: the embedding pipeline
//! prefixes stored content with `search_document:` and queries with
//! `search_query:` (Nomic convention), and the asymmetry caps cosine
//! similarity below 1.0 even on byte-identical inputs.
//!
//! Fix: a SHA-256 short-circuit on the canonical query text in
//! [`db::check_duplicate_with_text`]. Identical content scores
//! `similarity = 1.0` regardless of embedding model behaviour. Near-
//! but-not-exact content falls through to embedding-based cosine.

use ai_memory::db;
use ai_memory::models;
use chrono::Utc;
use tempfile::TempDir;

fn open_db() -> (rusqlite::Connection, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("ai-memory-f18.db");
    let conn = db::open(&path).expect("db::open");
    (conn, tmp)
}

/// Insert a memory and stamp it with a deterministic embedding so the
/// fall-through (non-exact) path of `check_duplicate_with_text` has
/// something to score against. The exact embedding values don't matter
/// for the exact-match assertion — the SHA-256 short-circuit fires
/// first.
fn seed_with_embedding(
    conn: &rusqlite::Connection,
    title: &str,
    content: &str,
    namespace: &str,
    embedding: &[f32],
) -> String {
    let now = Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let mem = models::Memory {
        id: id.clone(),
        tier: models::Tier::Long,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: content.to_string(),
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
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let stored_id = db::insert(conn, &mem).expect("db::insert");
    db::set_embedding(conn, &stored_id, embedding).expect("db::set_embedding");
    stored_id
}

#[test]
fn check_duplicate_with_text_exact_match_returns_similarity_1_0() {
    let (conn, _tmp) = open_db();

    // Stored embedding deliberately mismatched against the "query"
    // embedding to simulate the prefix-asymmetry that caps cosine at
    // ~0.92 even on identical content. The SHA-256 short-circuit must
    // bypass this and return similarity=1.0.
    let stored_emb = [0.92_f32, 0.30, 0.25];
    let query_emb = [1.0_f32, 0.0, 0.0];

    let title = "shopping list";
    let content = "buy bread, milk, eggs, and a thoughtfully-named dog toy";
    let stored_id = seed_with_embedding(&conn, title, content, "ns", &stored_emb);

    // The MCP layer forms the query text as `format!("{title} {content}")`;
    // we mirror that exactly so the SHA-256 hash matches.
    let query_text = format!("{title} {content}");
    let r = db::check_duplicate_with_text(&conn, &query_emb, &query_text, Some("ns"), 0.85)
        .expect("check_duplicate_with_text");

    let nearest = r
        .nearest
        .expect("byte-identical content must surface a nearest match");
    assert_eq!(nearest.id, stored_id);
    assert!(
        (nearest.similarity - 1.0).abs() < 1e-6,
        "byte-identical content must score similarity=1.0 (got {})",
        nearest.similarity
    );
    assert!(
        r.is_duplicate,
        "byte-identical content must be flagged is_duplicate=true"
    );
}

#[test]
fn check_duplicate_with_text_near_miss_falls_through_to_embedding() {
    let (conn, _tmp) = open_db();

    // Two memories whose embeddings sit at known cosine angles relative
    // to the query so the fall-through path has a deterministic answer:
    //   stored "shopping list" [0.95, 0.31, 0.0] — cos ≈ 0.95 vs query [1,0,0]
    //   query  text differs by one word; SHA-256 will NOT match, so we
    //   fall through to embedding cosine.
    let stored_id = seed_with_embedding(
        &conn,
        "shopping list",
        "buy bread, milk, eggs, and a thoughtfully-named dog toy",
        "ns",
        &[0.95, 0.31, 0.0],
    );

    // Differs by a single word ("dog" → "cat") so the SHA-256 hashes
    // diverge but the fall-through embedding path still scores the
    // pre-set 0.95 cosine.
    let near_miss_text =
        "shopping list buy bread, milk, eggs, and a thoughtfully-named cat toy".to_string();
    let query_emb = [1.0_f32, 0.0, 0.0];

    let r = db::check_duplicate_with_text(&conn, &query_emb, &near_miss_text, Some("ns"), 0.85)
        .expect("check_duplicate_with_text");

    let nearest = r
        .nearest
        .expect("near-miss must still return a nearest neighbour");
    assert_eq!(nearest.id, stored_id);
    assert!(
        nearest.similarity > 0.85,
        "near-miss embedding cosine must clear the 0.85 floor (got {})",
        nearest.similarity
    );
    assert!(
        nearest.similarity < 1.0,
        "near-miss must NOT saturate at 1.0 — that path is reserved for hash matches (got {})",
        nearest.similarity
    );
}

#[test]
fn check_duplicate_with_text_empty_db_returns_no_match() {
    // No memories stored — neither the hash short-circuit nor the
    // embedding fall-through should fabricate a match.
    let (conn, _tmp) = open_db();
    let query_emb = [1.0_f32, 0.0, 0.0];
    let r = db::check_duplicate_with_text(&conn, &query_emb, "anything goes", None, 0.85)
        .expect("check_duplicate_with_text");
    assert!(!r.is_duplicate);
    assert!(r.nearest.is_none());
}

#[test]
fn check_duplicate_with_text_namespace_filter_isolates_exact_match() {
    // An identical-content row in a different namespace must NOT
    // satisfy the hash short-circuit when the caller scopes the query
    // to a namespace. Exact-match short-circuit honours the same gates
    // as the embedding-similarity path.
    let (conn, _tmp) = open_db();
    let title = "shared title";
    let content = "shared content";
    let _wrong_ns = seed_with_embedding(&conn, title, content, "other", &[1.0, 0.0, 0.0]);

    let query_emb = [1.0_f32, 0.0, 0.0];
    let query_text = format!("{title} {content}");
    let r = db::check_duplicate_with_text(&conn, &query_emb, &query_text, Some("ns"), 0.85)
        .expect("check_duplicate_with_text");
    assert!(
        !r.is_duplicate,
        "namespace filter must scope the exact-match short-circuit"
    );
    assert!(r.nearest.is_none());
}
