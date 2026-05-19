// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 (issue #519) — proactive contradiction detection on
//! `memory_store`.
//!
//! Pins the substrate-level contract per the Initiative #9 v0.7.0-
//! blocker scope statement:
//!
//! 1. `proactive_conflict_check` returns `None` when no embedded
//!    candidate in the namespace passes the 0.95 cosine threshold.
//!
//! 2. `proactive_conflict_check` returns `Some(ProactiveConflict{..})`
//!    when at least one candidate is a near-duplicate (>= 0.95 cosine)
//!    AND its content body differs from the incoming write — the
//!    substrate-layer deterministic contradiction signal.
//!
//! 3. Same-content near-duplicates are NOT classified as conflicts
//!    (they're the upsert happy-path).
//!
//! 4. Self-matches (same memory id) are excluded.
//!
//! 5. Cross-namespace candidates do not trigger the guard.
//!
//! 6. Wire-shape: the new `force: bool` field on `CreateMemory`
//!    defaults to `false` and round-trips through serde.
//!
//! Embeddings are caller-supplied (no embedder required) so the
//! substrate-level contract is exercised under
//! `AI_MEMORY_NO_CONFIG=1` with zero network deps.

use ai_memory::models::{ConfidenceSource, CreateMemory, Memory, MemoryKind, Tier};
use ai_memory::storage as db;
use ai_memory::storage::{PROACTIVE_CONFLICT_SIM_THRESHOLD, proactive_conflict_check};

fn fresh_conn() -> rusqlite::Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn make_mem(title: &str, content: &str, ns: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "api".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: vec![],
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

/// Insert a memory + attach a caller-supplied embedding. Mirrors the
/// pattern handlers/http.rs uses: embed BEFORE the lock, then insert,
/// then `db::set_embedding`.
fn insert_with_embedding(conn: &rusqlite::Connection, mem: &Memory, embedding: &[f32]) -> String {
    let id = db::insert(conn, mem).expect("insert");
    db::set_embedding(conn, &id, embedding).expect("set_embedding");
    id
}

#[test]
fn proactive_conflict_returns_none_on_low_similarity() {
    let conn = fresh_conn();
    // Existing memory A.
    let mem_a = make_mem("alpha", "the moon landing was 1969", "global");
    let emb_a = vec![1.0_f32, 0.0, 0.0, 0.0];
    insert_with_embedding(&conn, &mem_a, &emb_a);

    // Incoming write B with orthogonal embedding => low cosine.
    let mem_b = make_mem("beta", "the speed of light is c", "global");
    let emb_b = vec![0.0_f32, 1.0, 0.0, 0.0];

    let conflict = proactive_conflict_check(&conn, &mem_b, &emb_b).expect("check ok");
    assert!(
        conflict.is_none(),
        "orthogonal embeddings must not trigger the proactive conflict guard"
    );
}

#[test]
fn proactive_conflict_returns_some_on_near_duplicate_with_differing_content() {
    let conn = fresh_conn();
    // Existing memory A about a quoted fact.
    let mem_a = make_mem("project-deadline", "deadline is june 15", "global");
    let emb_a = vec![1.0_f32, 0.0, 0.0];
    insert_with_embedding(&conn, &mem_a, &emb_a);

    // Incoming write A' with IDENTICAL embedding (cosine 1.0) but
    // DIFFERENT content — the substrate-layer contradiction signal.
    let mut mem_a_prime = make_mem("project-deadline-revised", "deadline is june 22", "global");
    // Distinct id so the self-exclusion branch can't short-circuit.
    mem_a_prime.id = uuid::Uuid::new_v4().to_string();
    let emb_a_prime = emb_a.clone();

    let conflict = proactive_conflict_check(&conn, &mem_a_prime, &emb_a_prime)
        .expect("check ok")
        .expect("near-duplicate with differing content must be a conflict");
    assert!(
        conflict.similarity >= PROACTIVE_CONFLICT_SIM_THRESHOLD,
        "similarity must clear the 0.95 threshold; got {}",
        conflict.similarity
    );
    assert_eq!(conflict.existing_title, "project-deadline");
    assert_eq!(conflict.reason, "near_duplicate_with_differing_content");
}

#[test]
fn proactive_conflict_skips_same_content_near_duplicates() {
    // Same-content near-duplicates are NOT contradictions — they are
    // the upsert happy-path that the existing `ON CONFLICT(title,
    // namespace)` SQL already handles.
    let conn = fresh_conn();
    let plaintext = "user prefers dark mode";
    let mem_a = make_mem("user-pref", plaintext, "global");
    let emb_a = vec![0.5_f32, 0.5, 0.0];
    insert_with_embedding(&conn, &mem_a, &emb_a);

    let mut mem_a_dup = make_mem("user-pref-2", plaintext, "global");
    mem_a_dup.id = uuid::Uuid::new_v4().to_string();
    let emb_dup = emb_a.clone();

    let conflict = proactive_conflict_check(&conn, &mem_a_dup, &emb_dup).expect("check ok");
    assert!(
        conflict.is_none(),
        "same-content near-duplicate must NOT trigger the conflict guard"
    );
}

#[test]
fn proactive_conflict_excludes_self_match() {
    // A re-store that reuses the existing memory id (NHI replay path)
    // must not see itself as a conflict.
    let conn = fresh_conn();
    let mem = make_mem("self-replay", "version 1 of the fact", "global");
    let emb = vec![1.0_f32, 0.0];
    let id = insert_with_embedding(&conn, &mem, &emb);

    // Build the "incoming" write that reuses the same id but proposes
    // differing content with an identical embedding.
    let mut replay = make_mem("self-replay", "version 2 of the fact", "global");
    replay.id = id;

    let conflict = proactive_conflict_check(&conn, &replay, &emb).expect("check ok");
    assert!(
        conflict.is_none(),
        "self-match (same memory id) must be excluded from the conflict scan"
    );
}

#[test]
fn proactive_conflict_scoped_to_namespace() {
    // Cross-namespace near-duplicates do not trigger the guard —
    // namespaces are deliberately isolated scopes.
    let conn = fresh_conn();
    let mem_alpha = make_mem("shared-title", "fact body alpha", "ns-alpha");
    let emb = vec![0.0_f32, 1.0];
    insert_with_embedding(&conn, &mem_alpha, &emb);

    let mut mem_beta = make_mem("shared-title", "fact body beta", "ns-beta");
    mem_beta.id = uuid::Uuid::new_v4().to_string();

    let conflict = proactive_conflict_check(&conn, &mem_beta, &emb).expect("check ok");
    assert!(
        conflict.is_none(),
        "cross-namespace near-duplicate must NOT trigger the guard"
    );
}

#[test]
fn proactive_conflict_ignores_candidates_without_embedding() {
    // A row stored without an embedding is invisible to the proactive
    // check (the scan filters on `embedding IS NOT NULL`).
    let conn = fresh_conn();
    let mem_a = make_mem("no-embed", "established fact", "global");
    db::insert(&conn, &mem_a).expect("insert without embedding");

    let mut mem_a_prime = make_mem("no-embed-conflict", "contradicting fact", "global");
    mem_a_prime.id = uuid::Uuid::new_v4().to_string();
    let emb = vec![1.0_f32, 1.0];

    let conflict = proactive_conflict_check(&conn, &mem_a_prime, &emb).expect("check ok");
    assert!(
        conflict.is_none(),
        "embedding-less candidates must not trigger the guard"
    );
}

#[test]
fn proactive_conflict_empty_embedding_short_circuits() {
    // An empty query embedding (degraded mode, no embedder wired)
    // returns None without touching the candidate pool.
    let conn = fresh_conn();
    let mem = make_mem("anything", "anything", "global");
    let emb: Vec<f32> = vec![];
    let conflict = proactive_conflict_check(&conn, &mem, &emb).expect("check ok");
    assert!(
        conflict.is_none(),
        "empty query embedding must short-circuit to None"
    );
}

#[test]
fn create_memory_body_force_defaults_to_false() {
    // Wire-shape: callers that omit `force` see `false` after serde
    // round-trip. The new field is `#[serde(default)]` — pre-#519
    // clients keep working byte-for-byte.
    let raw = serde_json::json!({
        "title": "wire-shape-check",
        "content": "force defaults to false",
        "namespace": "global",
    });
    let body: CreateMemory = serde_json::from_value(raw).expect("parse");
    assert!(!body.force, "force defaults to false");

    let raw_with_force = serde_json::json!({
        "title": "wire-shape-check-2",
        "content": "force=true round-trips",
        "namespace": "global",
        "force": true,
    });
    let body2: CreateMemory = serde_json::from_value(raw_with_force).expect("parse");
    assert!(body2.force, "force=true round-trips");
}
