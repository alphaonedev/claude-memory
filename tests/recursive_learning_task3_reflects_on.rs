// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! Issue #655 Task 3 — `reflects_on` relation in the link taxonomy.
//!
//! v0.7.0 add-on mission, recursive learning, Task 3/8. Pins the
//! `reflects_on` relation that a future `memory_reflect` MCP tool
//! (Task 4/8) will write from a reflection memory (the row carrying
//! `reflection_depth > 0`, see Task 1/8) back to each source memory
//! the reflection covers.
//!
//! Directionality contract (matches `derived_from`, see
//! `src/validate.rs::VALID_RELATIONS`): the **reflection memory** is
//! the link's `source_id`; the **source it reflects on** is the
//! `target_id`. The arrow points FROM the derived/newer row TO the
//! thing it points back to.
//!
//! Invariants pinned here:
//!   - `validate_relation("reflects_on")` returns `Ok` via the
//!     `VALID_RELATIONS` fast-path branch.
//!   - The existing closed-set discipline still holds for structurally
//!     malformed labels (uppercase, whitespace, control chars).
//!   - `MemoryLink { relation: "reflects_on", … }` round-trips through
//!     serde JSON without losing the relation label.
//!   - SQLite `db::create_link` + `db::get_links` round-trip a
//!     `reflects_on` edge.
//!   - SQLite `db::find_paths` walks `reflects_on` edges naturally
//!     (the BFS does not filter by relation label, so the new edge
//!     surfaces in chain queries alongside the other relations).
//!   - Postgres `MemoryStore::link` + `MemoryStore::list_links`
//!     round-trip a `reflects_on` edge (gated on `feature =
//!     "sal-postgres"` + `AI_MEMORY_TEST_POSTGRES_URL`).
//!
//! No schema migration is required: `memory_links.relation` is
//! `TEXT NOT NULL DEFAULT 'related_to'` on both SQLite and Postgres
//! with no `CHECK (relation IN (...))` clause, so adding a new label
//! to the taxonomy is a pure validator / documentation change.
//! Accordingly this test file does NOT bump `SCHEMA_VERSION` /
//! `CURRENT_SCHEMA_VERSION` / `MAX_SUPPORTED_SCHEMA`.

use ai_memory::db;
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, MemoryLink, Tier};
use chrono::Utc;

mod common;
#[cfg(feature = "sal-postgres")]
use common::postgres_url;

/// Fixture builder — returns a fully-populated `Memory` so individual
/// tests don't repeat the 16-field literal. `reflection_depth` is
/// surfaced so callers writing reflection-memory rows can pin the
/// provenance signal.
fn make_memory(namespace: &str, title: &str, reflection_depth: i32) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("recursive-learning task3 fixture: {title}"),
        tags: vec!["test".to_string(), "task3".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent-task3"}),
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

// ─────────────────────────────────────────────────────────────────────
// Validator — the canonical surface.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn validate_relation_accepts_reflects_on() {
    // Canonical fast-path branch: `reflects_on` is now a documented
    // member of `VALID_RELATIONS`. A future tightening of
    // `validate_relation` (closing the lenient `[a-z0-9_]+` branch)
    // must still admit this label — that's what the canonical list is
    // for.
    assert!(
        ai_memory::validate::validate_relation("reflects_on").is_ok(),
        "validate_relation must accept the canonical `reflects_on` label"
    );
}

#[test]
fn validate_relation_still_rejects_malformed_labels() {
    // Adding `reflects_on` to the canonical set must not loosen the
    // existing rejection rules. The closed-set boundary still rejects
    // uppercase, whitespace, hyphens, slashes, and empty input — these
    // are the surviving negative tests from `src/validate.rs` plus a
    // label that is structurally distinct from `reflects_on` to prove
    // we didn't accidentally open the gate.
    assert!(ai_memory::validate::validate_relation("").is_err());
    assert!(ai_memory::validate::validate_relation("REFLECTS_ON").is_err());
    assert!(ai_memory::validate::validate_relation("reflects on").is_err());
    assert!(ai_memory::validate::validate_relation("reflects/on").is_err());
    assert!(ai_memory::validate::validate_relation("reflects-on").is_err());
}

#[test]
fn validate_link_accepts_reflects_on_between_distinct_ids() {
    // Self-link rejection still fires; cross-id link accepts.
    assert!(ai_memory::validate::validate_link("aaa", "bbb", "reflects_on").is_ok());
    assert!(ai_memory::validate::validate_link("same", "same", "reflects_on").is_err());
}

// ─────────────────────────────────────────────────────────────────────
// Wire shape — serde JSON round-trip with the new label.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn memorylink_serde_roundtrip_preserves_reflects_on() {
    // Federation-wire shape — a `reflects_on` MemoryLink must survive a
    // serialize/deserialize cycle exactly the way `related_to` does.
    // Pins the contract that the relation field is opaque-string at the
    // wire boundary; no enum / discriminant translation lurks under
    // serde.
    let link = MemoryLink {
        source_id: "reflection-row-id".to_string(),
        target_id: "source-being-reflected-on".to_string(),
        relation: ai_memory::models::MemoryLinkRelation::ReflectsOn,
        created_at: Utc::now().to_rfc3339(),
        signature: None,
        observed_by: None,
        valid_from: None,
        valid_until: None,
        attest_level: None,
    };
    let wire = serde_json::to_string(&link).expect("serialize MemoryLink");
    let back: MemoryLink = serde_json::from_str(&wire).expect("deserialize MemoryLink");
    assert_eq!(
        back.relation,
        ai_memory::models::MemoryLinkRelation::ReflectsOn
    );
    assert_eq!(back.source_id, "reflection-row-id");
    assert_eq!(back.target_id, "source-being-reflected-on");
}

// ─────────────────────────────────────────────────────────────────────
// SQLite — create_link + get_links round-trip the new relation.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn sqlite_create_link_and_get_links_roundtrip_reflects_on() {
    let conn = db::open(std::path::Path::new(":memory:")).expect("open sqlite");

    // Two memories — the **source** of the link is the reflection
    // memory (reflection_depth > 0), the **target** is the original
    // source it reflects on (depth 0). Matches the
    // `derived_from`-style convention documented in
    // `VALID_RELATIONS`.
    let source = make_memory("task3-ns", "original-observation", 0);
    let reflection = make_memory("task3-ns", "reflection-on-observation", 1);
    let source_id = db::insert(&conn, &source).expect("insert source");
    let reflection_id = db::insert(&conn, &reflection).expect("insert reflection");

    // Reflection → source. Arrow direction: derived row first.
    db::create_link(&conn, &reflection_id, &source_id, "reflects_on")
        .expect("create_link reflects_on must succeed");

    let links = db::get_links(&conn, &reflection_id).expect("get_links reflection");
    assert!(
        links.iter().any(
            |l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn
                && l.source_id == reflection_id
                && l.target_id == source_id
        ),
        "round-trip must surface a `reflects_on` edge from reflection to source; got {links:?}"
    );

    // Also visible from the source memory's perspective (get_links is
    // symmetric — returns rows where the id is on either side).
    let inbound = db::get_links(&conn, &source_id).expect("get_links source");
    assert!(
        inbound.iter().any(
            |l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn
                && l.target_id == source_id
        ),
        "source memory's get_links must include the inbound `reflects_on` edge; got {inbound:?}"
    );
}

#[test]
fn sqlite_find_paths_walks_reflects_on_edges() {
    // `find_paths` walks `memory_links` without filtering by relation
    // label (it's relation-agnostic by design, see
    // `db::find_paths::sql` — the recursive CTE projects every edge
    // regardless of `relation`). Adding `reflects_on` to the
    // taxonomy must therefore surface naturally in chain walks.
    // Operators tracing reflection provenance — "what did this
    // reflection reflect on, and what did THOSE reflect on?" — must
    // see the chain.
    //
    // Build a tiny chain:  reflection_b →reflects_on→ reflection_a
    //                      reflection_a →reflects_on→ original
    // and assert `find_paths(reflection_b, original)` enumerates it.
    let conn = db::open(std::path::Path::new(":memory:")).expect("open sqlite");

    let original = make_memory("task3-paths", "original", 0);
    let reflection_a = make_memory("task3-paths", "reflection-a", 1);
    let reflection_b = make_memory("task3-paths", "reflection-b", 2);
    let original_id = db::insert(&conn, &original).expect("insert original");
    let a_id = db::insert(&conn, &reflection_a).expect("insert reflection_a");
    let b_id = db::insert(&conn, &reflection_b).expect("insert reflection_b");

    db::create_link(&conn, &a_id, &original_id, "reflects_on").expect("a → original");
    db::create_link(&conn, &b_id, &a_id, "reflects_on").expect("b → a");

    let paths = db::find_paths(&conn, &b_id, &original_id, Some(4), Some(10), false)
        .expect("find_paths must succeed");
    assert!(
        !paths.is_empty(),
        "find_paths must enumerate at least one `reflects_on` chain from b to original; got {paths:?}"
    );
    // The shortest path is the 3-id chain [b, a, original].
    let canonical = vec![b_id.clone(), a_id.clone(), original_id.clone()];
    assert!(
        paths.iter().any(|p| p == &canonical),
        "find_paths must surface the canonical [b, a, original] chain; got {paths:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Postgres parity — gated on the sal-postgres feature + live test DB.
// Mirrors the gating pattern in
// `tests/recursive_learning_task1_reflection_depth.rs`.
// ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "sal-postgres")]
#[tokio::test]
async fn postgres_link_listlinks_roundtrips_reflects_on() {
    use ai_memory::store::CallerContext;
    use ai_memory::store::MemoryStore;
    use ai_memory::store::postgres::PostgresStore;

    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    let store = PostgresStore::connect(&url).await.expect("connect");
    let ctx = CallerContext::for_agent("test-agent-task3");

    // Unique namespace so re-runs against the same DB don't trip the
    // (title, namespace) unique index from prior fixture rows.
    let suffix = uuid::Uuid::new_v4();
    let ns = format!("task3-reflects-on-pg-{suffix}");

    let source = make_memory(&ns, "original-observation", 0);
    let reflection = make_memory(&ns, "reflection-on-observation", 1);
    let source_id = store.store(&ctx, &source).await.expect("store source");
    let reflection_id = store
        .store(&ctx, &reflection)
        .await
        .expect("store reflection");

    let link = MemoryLink {
        source_id: reflection_id.clone(),
        target_id: source_id.clone(),
        relation: ai_memory::models::MemoryLinkRelation::ReflectsOn,
        created_at: Utc::now().to_rfc3339(),
        signature: None,
        observed_by: None,
        valid_from: None,
        valid_until: None,
        attest_level: None,
    };
    store
        .link(&ctx, &link)
        .await
        .expect("MemoryStore::link must accept `reflects_on`");

    // `list_links(Some(ns))` returns every link whose source memory
    // sits in this namespace — that's the affinity the SAL contract
    // documents. Our reflection_id is in `ns`, so the edge lands.
    let links = store
        .list_links(Some(&ns))
        .await
        .expect("list_links must succeed");
    assert!(
        links.iter().any(
            |l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn
                && l.source_id == reflection_id
                && l.target_id == source_id
        ),
        "Postgres list_links must round-trip the `reflects_on` edge; got {links:?}"
    );

    // Cleanup so re-runs stay deterministic. Deleting the memories
    // cascades to `memory_links` via the FK ON DELETE CASCADE.
    let _ = store.delete(&ctx, &reflection_id).await;
    let _ = store.delete(&ctx, &source_id).await;
}
