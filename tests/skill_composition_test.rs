// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-7 (issue #672) — reflection-skill composition declaration.
//!
//! End-to-end pins:
//!
//! - Parsing the `composes_with_reflections` frontmatter populates the
//!   structured Vec AND mirrors the declaration into `metadata`.
//! - Registering a composing skill via the public skills table layout
//!   carries the metadata mirror, so `memory_skill_compositional_context`
//!   reads the declaration back from the row.
//! - Calling `memory_skill_compositional_context(skill_id)` returns the
//!   decompressed body and reflections from the declared namespaces.
//! - Reflections at `depth < min_depth` are filtered out.
//! - Reflections at `depth > max_reflection_depth` (governance ceiling)
//!   are bounded by design — substrate refuses expansion beyond it.
//! - Ranking is recency + `recall_count` (`recall_count` = `access_count`,
//!   the documented "every recall mutates `access_count`" proxy).
//! - Backwards-compat regression pin: old SKILL.md (no
//!   `composes_with_reflections`) parses fine; the response degrades to
//!   body-only.
//!
//! These tests don't depend on the MCP JSON-RPC envelope — they exercise
//! the handler directly, mirroring the pattern in `tests/skill_test.rs`.

use std::path::PathBuf;

use ai_memory::db;
use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::parsing::skill_md;
use chrono::Utc;
use serde_json::json;
use sha2::Digest as _;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_test_db() -> (rusqlite::Connection, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("l2-7-skill-composition.db");
    let conn = db::open(&db_path).expect("open db");
    // Leak the tempdir for the lifetime of the test by retaining its
    // path inside the returned tuple — the caller's binding extends
    // through end-of-test scope so the file isn't deleted under us.
    std::mem::forget(dir);
    (conn, db_path)
}

/// Insert a SKILL row directly with the metadata blob the L2-7 parser
/// produces. Mirrors `register_core` shape without pulling in the
/// keypair/signed-events scaffolding (those layers are tested in
/// `tests/skill_test.rs`). Returns the inserted skill id.
fn insert_skill_with_metadata(
    conn: &rusqlite::Connection,
    namespace: &str,
    name: &str,
    description: &str,
    body: &str,
    metadata: &serde_json::Value,
) -> String {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();

    let body_blob = zstd::encode_all(body.as_bytes(), 3).expect("compress");
    let canonical = serde_json::to_vec(&json!({
        "namespace": namespace,
        "name": name,
        "description": description,
        "license": null,
        "compatibility": null,
        "allowed_tools": [],
    }))
    .unwrap();
    let mut hasher = sha2::Sha256::new();
    hasher.update(&canonical);
    hasher.update(body.as_bytes());
    let digest: Vec<u8> = hasher.finalize().to_vec();

    let id = uuid::Uuid::new_v4().to_string();
    let metadata_json = serde_json::to_string(metadata).unwrap();
    conn.execute(
        "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![id, namespace, name, description, metadata_json, body_blob, digest, now],
    )
    .expect("insert skill");
    id
}

/// Make + insert a reflection memory at a given depth in a namespace,
/// stamped with the given `access_count`. The substrate normally
/// increments `access_count` on every recall; the test seeds it
/// directly to model the "hot reflection" scoring path.
fn seed_reflection(
    conn: &rusqlite::Connection,
    namespace: &str,
    title: &str,
    content: &str,
    depth: i32,
    access_count: i64,
    created_offset_secs: i64,
) -> String {
    let created = Utc::now() - chrono::Duration::seconds(created_offset_secs);
    let m = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec!["reflection".to_string()],
        priority: 5,
        confidence: 0.9,
        source: "test".to_string(),
        access_count: 0, // set explicitly below; db::insert seeds 0.
        created_at: created.to_rfc3339(),
        updated_at: created.to_rfc3339(),
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": "test-l2-7"}),
        reflection_depth: depth,
        memory_kind: MemoryKind::Reflection,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let id = db::insert(conn, &m).expect("insert reflection");

    // Patch access_count + created_at to model the historical state we want.
    // `created_at` matters for the recency term; `access_count` is the
    // recall_count proxy.
    conn.execute(
        "UPDATE memories SET access_count = ?1, created_at = ?2 WHERE id = ?3",
        rusqlite::params![access_count, created.to_rfc3339(), id],
    )
    .expect("update reflection metadata");
    id
}

/// The handler signature is private to `crate::mcp::tools` — exercise it
/// via the public MCP dispatch so the test rides the same code path the
/// production JSON-RPC server uses.
fn call_handler(
    conn: &rusqlite::Connection,
    skill_id: &str,
    budget_tokens: Option<u64>,
) -> serde_json::Value {
    let mut params = json!({"skill_id": skill_id});
    if let Some(b) = budget_tokens {
        params["budget_tokens"] = json!(b);
    }
    ai_memory::mcp::skill_compositional_context_for_tests(conn, &params)
        .expect("compositional_context handler returns Ok")
}

// ---------------------------------------------------------------------------
// Pin 1: parser populates Vec + mirrors into metadata
// ---------------------------------------------------------------------------

#[test]
fn parser_populates_vec_and_mirrors_metadata() {
    let doc = "---\n\
        namespace: skills\n\
        name: composer\n\
        description: A composing skill.\n\
        composes_with_reflections:\n  \
          - namespace: foo/observations\n    \
            min_depth: 1\n  \
          - namespace: foo/decisions\n    \
            min_depth: 2\n\
        ---\n\nDo a thing.\n";

    let m = skill_md::parse(doc).expect("parse");
    assert_eq!(m.composes_with_reflections.len(), 2);
    assert_eq!(m.composes_with_reflections[0].namespace, "foo/observations");
    assert_eq!(m.composes_with_reflections[0].min_depth, 1);
    assert_eq!(m.composes_with_reflections[1].namespace, "foo/decisions");
    assert_eq!(m.composes_with_reflections[1].min_depth, 2);

    // The metadata mirror is the back-compat surface — pre-L2-7 readers
    // see the declaration as opaque-but-present data here.
    let mirrored = m
        .metadata
        .get("composes_with_reflections")
        .expect("L2-7 mirror");
    assert!(mirrored.is_array());
}

// ---------------------------------------------------------------------------
// Pin 2: backwards-compat regression — old SKILL.md parses + composes
// returns body-only.
// ---------------------------------------------------------------------------

#[test]
fn legacy_skill_md_returns_body_only() {
    let (conn, _path) = open_test_db();

    // SKILL.md with NO composes_with_reflections — pre-L2-7 shape.
    let body = "# Legacy\n\nLegacy skill body.\n";
    let skill_id = insert_skill_with_metadata(
        &conn,
        "legacy-ns",
        "legacy-skill",
        "A pre-L2-7 skill.",
        body,
        &json!({}),
    );

    let resp = call_handler(&conn, &skill_id, None);

    assert_eq!(resp["skill_id"].as_str().unwrap(), skill_id);
    assert_eq!(resp["body"].as_str().unwrap(), body);
    assert_eq!(
        resp["reflections"].as_array().unwrap().len(),
        0,
        "legacy skill returns body-only"
    );
    assert_eq!(
        resp["compositional_namespaces"].as_array().unwrap().len(),
        0,
        "no declared namespaces"
    );
}

// ---------------------------------------------------------------------------
// Pin 3: acceptance — 5 reflections, ranked by recency + recall_count.
// ---------------------------------------------------------------------------

#[test]
fn five_reflections_ranked_by_recency_and_recall_count() {
    let (conn, _path) = open_test_db();

    let body = "# Composer\n\nUse the declared namespaces.\n";
    // Mirror the declaration as the L2-7 parser would: a single entry
    // for `foo/observations` with `min_depth: 1`. (The mirror is what
    // the handler reads back at runtime.)
    let metadata = json!({
        "composes_with_reflections": [
            {"namespace": "foo/observations", "min_depth": 1},
        ],
    });
    let skill_id = insert_skill_with_metadata(
        &conn,
        "skills",
        "composer",
        "A composing skill.",
        body,
        &metadata,
    );

    // 5 reflections at depth=1 in foo/observations. Mix recency and
    // access_count so we can verify both terms contribute.
    //
    // Layout:
    //   recent-hot     : created 60s ago,  access_count = 50 (saturated)
    //   recent-cold    : created 60s ago,  access_count = 0
    //   old-hot        : created 30d ago,  access_count = 50
    //   medium-medium  : created 7d ago,   access_count = 10
    //   ancient-cold   : created 300d ago, access_count = 0
    //
    // Expected score order (highest first):
    //   recent-hot   (recency ~ 1.0  + recall 1.0  = 2.0)
    //   recent-cold  (recency ~ 1.0  + recall 0.0  = 1.0)
    //   medium-medium(recency ~ 0.98 + recall 0.2  = 1.18)
    //   old-hot      (recency ~ 0.92 + recall 1.0  = 1.92)
    //   ancient-cold (recency ~ 0.18 + recall 0.0  = 0.18)
    //
    // Reordered descending by score:
    //   recent-hot, old-hot, medium-medium, recent-cold, ancient-cold
    let id_recent_hot = seed_reflection(
        &conn,
        "foo/observations",
        "recent-hot",
        "Body of recent hot reflection.",
        1,
        50,
        60,
    );
    let id_recent_cold = seed_reflection(
        &conn,
        "foo/observations",
        "recent-cold",
        "Body of recent cold reflection.",
        1,
        0,
        60,
    );
    let id_old_hot = seed_reflection(
        &conn,
        "foo/observations",
        "old-hot",
        "Body of old hot reflection.",
        1,
        50,
        30 * 24 * 3600,
    );
    let id_medium = seed_reflection(
        &conn,
        "foo/observations",
        "medium-medium",
        "Body of medium reflection.",
        1,
        10,
        7 * 24 * 3600,
    );
    let id_ancient = seed_reflection(
        &conn,
        "foo/observations",
        "ancient-cold",
        "Body of ancient cold reflection.",
        1,
        0,
        300 * 24 * 3600,
    );

    let resp = call_handler(&conn, &skill_id, Some(32_000));
    let refs = resp["reflections"].as_array().unwrap();

    // All 5 should appear (budget is enormous relative to short content).
    assert_eq!(refs.len(), 5, "all 5 reflections in the budget");

    // Pull ids in returned order and verify descending score.
    let ids: Vec<&str> = refs
        .iter()
        .map(|r| r["id"].as_str().expect("id present"))
        .collect();

    let scores: Vec<f64> = refs
        .iter()
        .map(|r| r["score"].as_f64().expect("score present"))
        .collect();
    for w in scores.windows(2) {
        assert!(
            w[0] >= w[1],
            "scores must be monotonically non-increasing: {scores:?}"
        );
    }

    // The hottest, most-recent reflection ranks first.
    assert_eq!(
        ids[0], id_recent_hot,
        "recent-hot must rank #1 (recency ~1 + recall 1 ≈ 2.0)"
    );
    // The ancient cold reflection ranks last.
    assert_eq!(ids[4], id_ancient, "ancient-cold must rank last");
    // old-hot beats recent-cold because the recall-saturation bonus
    // dominates the small recency penalty over 30 days.
    let pos_old_hot = ids.iter().position(|&i| i == id_old_hot).unwrap();
    let pos_recent_cold = ids.iter().position(|&i| i == id_recent_cold).unwrap();
    assert!(
        pos_old_hot < pos_recent_cold,
        "old-hot must rank above recent-cold (recall saturation dominates)"
    );

    // Compositional namespaces echo the floor + the resolved ceiling.
    let ns_info = resp["compositional_namespaces"].as_array().unwrap();
    assert_eq!(ns_info.len(), 1);
    assert_eq!(ns_info[0]["namespace"].as_str(), Some("foo/observations"));
    assert_eq!(ns_info[0]["min_depth"].as_u64(), Some(1));
    // Default ceiling (no governance override) = 3.
    assert_eq!(ns_info[0]["max_reflection_depth"].as_u64(), Some(3));

    // Pin the medium-medium id is mentioned so a future refactor can't
    // silently drop it from the response.
    assert!(ids.contains(&id_medium.as_str()));
}

// ---------------------------------------------------------------------------
// Pin 4: depth < min_depth is filtered.
// ---------------------------------------------------------------------------

#[test]
fn min_depth_filters_out_shallower_reflections() {
    let (conn, _path) = open_test_db();

    let metadata = json!({
        "composes_with_reflections": [
            {"namespace": "foo/decisions", "min_depth": 2},
        ],
    });
    let skill_id = insert_skill_with_metadata(
        &conn,
        "skills",
        "deep-only",
        "A skill that wants depth>=2.",
        "Body.",
        &metadata,
    );

    // depth=1 — must be filtered out by min_depth=2.
    seed_reflection(
        &conn,
        "foo/decisions",
        "shallow",
        "depth 1 reflection",
        1,
        0,
        60,
    );
    // depth=2 — admits.
    let id_d2 = seed_reflection(
        &conn,
        "foo/decisions",
        "deep",
        "depth 2 reflection",
        2,
        0,
        60,
    );

    let resp = call_handler(&conn, &skill_id, None);
    let refs = resp["reflections"].as_array().unwrap();

    assert_eq!(
        refs.len(),
        1,
        "depth=1 reflection must be filtered by min_depth=2"
    );
    assert_eq!(refs[0]["id"].as_str().unwrap(), id_d2);
}

// ---------------------------------------------------------------------------
// Pin 5: depth > max_reflection_depth is bounded by design.
// ---------------------------------------------------------------------------

#[test]
fn max_reflection_depth_ceiling_is_authoritative() {
    let (conn, _path) = open_test_db();

    let metadata = json!({
        "composes_with_reflections": [
            {"namespace": "foo/observations", "min_depth": 1},
        ],
    });
    let skill_id = insert_skill_with_metadata(
        &conn,
        "skills",
        "bounded",
        "A skill that respects governance.",
        "Body.",
        &metadata,
    );

    // depth=3 — at the compiled default ceiling, admits.
    let id_admits = seed_reflection(
        &conn,
        "foo/observations",
        "at-ceiling",
        "depth 3 reflection",
        3,
        0,
        60,
    );
    // depth=4 — above the ceiling; the substrate refuses expansion
    // beyond max_reflection_depth per declared namespace.
    seed_reflection(
        &conn,
        "foo/observations",
        "above-ceiling",
        "depth 4 reflection",
        4,
        0,
        60,
    );

    let resp = call_handler(&conn, &skill_id, None);
    let refs = resp["reflections"].as_array().unwrap();
    assert_eq!(refs.len(), 1, "ceiling filters out depth=4 reflection");
    assert_eq!(refs[0]["id"].as_str().unwrap(), id_admits);
}

// ---------------------------------------------------------------------------
// Pin 6: budget_tokens cap drops the lowest-ranked overflowing entries.
// ---------------------------------------------------------------------------

#[test]
fn budget_tokens_caps_response() {
    let (conn, _path) = open_test_db();

    let metadata = json!({
        "composes_with_reflections": [
            {"namespace": "budget-ns", "min_depth": 1},
        ],
    });
    let skill_id = insert_skill_with_metadata(
        &conn,
        "skills",
        "budget",
        "A skill testing the budget.",
        "Body.",
        &metadata,
    );

    // Two reflections; both at depth=1. The first is the "winner"
    // (high access_count, recent), the second is the "loser" with the
    // same recency but no recalls. A 1-token budget admits zero rows;
    // we ask for a budget large enough for ONE 50-token reflection but
    // not both.
    seed_reflection(
        &conn,
        "budget-ns",
        "winner",
        &"Winning content. ".repeat(20),
        1,
        50,
        60,
    );
    seed_reflection(
        &conn,
        "budget-ns",
        "loser",
        &"Losing content. ".repeat(20),
        1,
        0,
        60,
    );

    // Budget that admits the first reflection (~60 tokens) but not both.
    let resp = call_handler(&conn, &skill_id, Some(70));
    let refs = resp["reflections"].as_array().unwrap();
    assert!(
        !refs.is_empty(),
        "budget must admit at least one reflection when the top one fits"
    );
    // The "winner" (higher score) must be the survivor.
    assert_eq!(refs[0]["title"].as_str(), Some("winner"));
    // At least one was dropped.
    assert!(resp["memories_dropped"].as_u64().unwrap() >= 1);
}

// ---------------------------------------------------------------------------
// Pin 7: unknown skill_id returns a clean error.
// ---------------------------------------------------------------------------

#[test]
fn unknown_skill_id_errors_cleanly() {
    let (conn, _path) = open_test_db();
    let r = ai_memory::mcp::skill_compositional_context_for_tests(
        &conn,
        &json!({"skill_id": "00000000-0000-0000-0000-000000000000"}),
    );
    let err = r.expect_err("unknown skill must error");
    assert!(err.contains("skill not found"), "got: {err}");
}
