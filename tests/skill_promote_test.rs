// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-6 (issue #671) — reflection-as-skill promotion regression
//! suite. The closing loop of the recursive-learning substrate.
//!
//! Acceptance contracts pinned here:
//! - Reflection depth=1 with 3 sources → skill with 3 reference resources.
//! - Promoted skill exports via `memory_skill_export` to a valid Agent
//!   Skills folder.
//! - **Round-trip**: promote → export → re-register from folder →
//!   **identical SHA-256 digest**. This is the keystone acceptance.
//! - Reflection depth=0 → refused (no synthesised insight).
//! - Threshold configurable via
//!   `namespace.governance.skill_promotion_min_depth`.
//! - `skills-ref validate <exported>` passes when installed; skipped
//!   gracefully with stderr message when absent (mirrors the L1-5
//!   pattern in `tests/skill_test.rs`).
//!
//! The tests call the MCP handlers directly via the `pub use` re-exports
//! in `src/mcp/mod.rs`. Going through the stdio JSON-RPC layer would add
//! noise without buying coverage — every handler is feature-checked via
//! its own typed signature.

use ai_memory::models::ConfidenceSource;
use std::path::PathBuf;

use ai_memory::db;
use ai_memory::mcp;
use ai_memory::models::{Memory, MemoryKind, Tier};
use serde_json::json;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_test_db() -> (rusqlite::Connection, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("promote.db");
    let conn = db::open(&db_path).expect("open db");
    (conn, db_path, dir)
}

fn insert_observation(conn: &rusqlite::Connection, title: &str, ns: &str, body: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let m = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: body.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "cli".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    db::insert(conn, &m).expect("insert observation")
}

fn make_reflection(
    conn: &rusqlite::Connection,
    sources: &[String],
    ns: &str,
    title: &str,
) -> ai_memory::db::ReflectOutcome {
    let input = db::ReflectInput {
        source_ids: sources.to_vec(),
        title: title.to_string(),
        content: format!("Reflection over {} sources: pattern X.", sources.len()),
        namespace: Some(ns.to_string()),
        tier: Tier::Mid,
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "cli".to_string(),
        agent_id: "test-agent".to_string(),
        metadata: json!({}),
    };
    db::reflect(conn, &input).expect("reflect should succeed")
}

// ---------------------------------------------------------------------------
// Acceptance tests
// ---------------------------------------------------------------------------

#[test]
fn promote_reflection_depth_1_with_3_sources_produces_3_reference_resources() {
    let (conn, _path, _dir) = open_test_db();

    let s1 = insert_observation(&conn, "src-1", "ns-a", "body 1");
    let s2 = insert_observation(&conn, "src-2", "ns-a", "body 2");
    let s3 = insert_observation(&conn, "src-3", "ns-a", "body 3");

    let refl = make_reflection(&conn, &[s1, s2, s3], "ns-a", "refl-3-src");
    assert_eq!(refl.reflection_depth, 1, "depth must be 1");

    let payload = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "pattern-x",
            "skill_description": "Reusable pattern derived from three observations.",
        }),
        None,
    )
    .expect("promote should succeed");
    assert_eq!(payload["promoted"], true);
    assert_eq!(payload["sources_attached"], 3);
    assert_eq!(payload["name"], "pattern-x");
    assert_eq!(payload["namespace"], "ns-a");
    assert_eq!(payload["original_reflection_depth"], 1);

    let skill_id = payload["skill_id"].as_str().expect("skill_id");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM skill_resources WHERE skill_id = ?1",
            [skill_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 3, "expected 3 reference resources");

    let mut stmt = conn
        .prepare(
            "SELECT resource_path FROM skill_resources WHERE skill_id = ?1 ORDER BY resource_path",
        )
        .unwrap();
    let paths: Vec<String> = stmt
        .query_map([skill_id], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(
        paths,
        vec![
            "references/source_0.md".to_string(),
            "references/source_1.md".to_string(),
            "references/source_2.md".to_string(),
        ],
    );
}

#[test]
fn refuses_depth_zero_reflection() {
    let (conn, _path, _dir) = open_test_db();

    // Construct memory_kind='reflection' at depth 0 explicitly to
    // exercise the depth gate (a depth-0 row is the kill-switch case —
    // it carries no synthesised insight even when the kind is reflection).
    let now = chrono::Utc::now().to_rfc3339();
    let m = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: "ns-a".to_string(),
        title: "fake-shallow-reflection".to_string(),
        content: "depth-0 reflection that shouldn't be promotable".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "cli".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Reflection,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    let id = db::insert(&conn, &m).expect("insert");

    let err = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": id,
            "skill_name": "should-not-land",
            "skill_description": "Should be refused.",
        }),
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("skill_promotion_min_depth") || err.contains("synthesised insight"),
        "must refuse with threshold message: {err}",
    );
}

#[test]
fn refuses_non_reflection_kind() {
    let (conn, _path, _dir) = open_test_db();
    let obs_id = insert_observation(&conn, "raw note", "ns-a", "raw content");

    let err = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": obs_id,
            "skill_name": "should-not-land",
            "skill_description": "Should be refused — not a reflection.",
        }),
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("memory_kind") && err.contains("observation"),
        "must surface kind mismatch: {err}",
    );
}

#[test]
fn threshold_configurable_via_namespace_governance() {
    let (conn, _path, _dir) = open_test_db();
    // Operator sets `skill_promotion_min_depth = 2` on 'ns-strict'.
    // A depth-1 reflection in that namespace must be refused.
    let now = chrono::Utc::now().to_rfc3339();
    let std_mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: "ns-strict".to_string(),
        title: "namespace_standard".to_string(),
        content: "Standard for ns-strict.".to_string(),
        tags: vec![],
        priority: 9,
        confidence: 1.0,
        source: "cli".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({
            "governance": {
                "skill_promotion_min_depth": 2,
            }
        }),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    let std_id = db::insert(&conn, &std_mem).expect("insert standard");
    db::set_namespace_standard(&conn, "ns-strict", &std_id, None).expect("set standard");

    let s1 = insert_observation(&conn, "src-strict", "ns-strict", "body");
    let refl = make_reflection(&conn, &[s1], "ns-strict", "refl-strict-1");
    assert_eq!(refl.reflection_depth, 1);

    let err = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "strict-skill",
            "skill_description": "Should be refused under stricter policy.",
        }),
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("skill_promotion_min_depth=2"),
        "must surface configured threshold: {err}",
    );
}

#[test]
fn round_trip_promote_export_reregister_identical_digest() {
    // KEYSTONE ACCEPTANCE — the round-trip digest match.
    let (conn, _path, dir) = open_test_db();

    let s1 = insert_observation(&conn, "src-rt-1", "ns-rt", "body 1");
    let s2 = insert_observation(&conn, "src-rt-2", "ns-rt", "body 2");
    let refl = make_reflection(&conn, &[s1, s2], "ns-rt", "refl-rt");

    let payload = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "roundtrip-promoted",
            "skill_description": "Round-trip from reflection promotion.",
        }),
        None,
    )
    .expect("promote");
    let skill_id = payload["skill_id"].as_str().expect("skill_id").to_string();
    let digest_original = payload["digest"].as_str().expect("digest").to_string();

    let export_dir = dir.path().join("exported");
    let export_payload = mcp::handle_skill_export(
        &conn,
        &json!({
            "skill_id": skill_id,
            "target_folder": export_dir.to_string_lossy(),
        }),
        None,
    )
    .expect("export");
    assert_eq!(export_payload["exported"], true);
    assert_eq!(
        export_payload["digest"].as_str().unwrap(),
        digest_original,
        "exported digest equals promoted digest"
    );
    assert!(export_dir.join("SKILL.md").is_file());
    // skill_export writes resources at `<target>/resources/<resource_path>`,
    // so the references/ subtree lives under resources/references/.
    assert!(
        export_dir
            .join("resources/references/source_0.md")
            .is_file()
    );
    assert!(
        export_dir
            .join("resources/references/source_1.md")
            .is_file()
    );

    // Re-register from the exported folder into a fresh DB so the
    // (namespace, name) collision doesn't trigger a supersession.
    let (conn2, _path2, _dir2) = open_test_db();
    let rereg_payload = mcp::handle_skill_register(
        &conn2,
        &json!({
            "folder_path": export_dir.to_string_lossy(),
        }),
        None,
    )
    .expect("re-register");
    let digest_reregistered = rereg_payload["digest"].as_str().expect("digest");

    assert_eq!(
        digest_reregistered, digest_original,
        "round-trip digest must be identical (promote → export → re-register)"
    );
}

#[test]
fn promote_records_derived_from_provenance_in_metadata() {
    let (conn, _path, _dir) = open_test_db();
    let s1 = insert_observation(&conn, "src-prov", "ns-prov", "body");
    let refl = make_reflection(&conn, &[s1], "ns-prov", "refl-prov");

    let payload = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "prov-skill",
            "skill_description": "Provenance edge test.",
        }),
        None,
    )
    .expect("promote");
    assert_eq!(
        payload["derived_from_reflection_id"].as_str().unwrap(),
        refl.id,
        "skill envelope must carry derived_from_reflection_id"
    );
    assert_eq!(payload["original_reflection_depth"], 1);

    let skill_id = payload["skill_id"].as_str().unwrap();
    let metadata_json: String = conn
        .query_row(
            "SELECT metadata FROM skills WHERE id = ?1",
            [skill_id],
            |r| r.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
    assert_eq!(
        metadata["derived_from_reflection_id"].as_str().unwrap(),
        refl.id,
        "skills.metadata.derived_from_reflection_id must be persisted"
    );
    assert_eq!(metadata["original_reflection_depth"], 1);
}

#[test]
fn parameters_schema_spliced_into_skill_body() {
    let (conn, _path, _dir) = open_test_db();
    let s1 = insert_observation(&conn, "src-params", "ns-params", "body");
    let refl = make_reflection(&conn, &[s1], "ns-params", "refl-params");

    let schema = json!({
        "type": "object",
        "properties": {
            "input_x": {"type": "string"},
        },
    });
    let payload = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "params-skill",
            "skill_description": "Skill with parameters schema.",
            "parameters_schema": schema,
        }),
        None,
    )
    .expect("promote");
    let skill_id = payload["skill_id"].as_str().unwrap();
    let body_blob: Vec<u8> = conn
        .query_row(
            "SELECT body_blob FROM skills WHERE id = ?1",
            [skill_id],
            |r| r.get(0),
        )
        .unwrap();
    let body = String::from_utf8(zstd::decode_all(body_blob.as_slice()).unwrap()).unwrap();
    assert!(
        body.contains("## Parameters"),
        "body must contain Parameters section: {body}"
    );
    assert!(
        body.contains("input_x"),
        "schema fields must be spliced: {body}"
    );
}

#[test]
fn missing_required_params_are_rejected() {
    let (conn, _path, _dir) = open_test_db();
    let err = mcp::handle_skill_promote_from_reflection(&conn, &json!({}), None).unwrap_err();
    assert!(err.contains("reflection_id"), "{err}");

    let err =
        mcp::handle_skill_promote_from_reflection(&conn, &json!({"reflection_id": "x"}), None)
            .unwrap_err();
    assert!(err.contains("skill_name"), "{err}");

    let err = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({"reflection_id": "x", "skill_name": "n"}),
        None,
    )
    .unwrap_err();
    assert!(err.contains("skill_description"), "{err}");
}

#[test]
fn invalid_skill_name_rejected_with_spec_citation() {
    let (conn, _path, _dir) = open_test_db();
    let s1 = insert_observation(&conn, "src-bad", "ns-bad", "body");
    let refl = make_reflection(&conn, &[s1], "ns-bad", "refl-bad-name");
    let err = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "BadName",
            "skill_description": "desc",
        }),
        None,
    )
    .unwrap_err();
    assert!(err.contains("spec §3.1"), "must cite spec: {err}");
}

#[test]
fn unknown_reflection_id_returns_not_found() {
    let (conn, _path, _dir) = open_test_db();
    let err = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": "nonexistent-id",
            "skill_name": "x",
            "skill_description": "desc",
        }),
        None,
    )
    .unwrap_err();
    assert!(err.contains("not found"), "expected not found: {err}");
}

// ---------------------------------------------------------------------------
// External validator — optional, skipped if not installed (L1-5 pattern)
// ---------------------------------------------------------------------------

#[test]
fn skills_ref_validates_promoted_skill_if_installed() {
    let (conn, _path, dir) = open_test_db();
    let s1 = insert_observation(&conn, "src-val", "ns-val", "body");
    let refl = make_reflection(&conn, &[s1], "ns-val", "refl-val");

    let payload = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "validate-me",
            "skill_description": "External validator target.",
        }),
        None,
    )
    .expect("promote");
    let skill_id = payload["skill_id"].as_str().unwrap();

    let export_dir = dir.path().join("validate-exported");
    let _ = mcp::handle_skill_export(
        &conn,
        &json!({
            "skill_id": skill_id,
            "target_folder": export_dir.to_string_lossy(),
        }),
        None,
    )
    .expect("export");

    let which = std::process::Command::new("which")
        .arg("skills-ref")
        .output();
    match which {
        Ok(out) if out.status.success() => {
            let validate = std::process::Command::new("skills-ref")
                .arg("validate")
                .arg(&export_dir)
                .output();
            match validate {
                Ok(v) if v.status.success() => {
                    eprintln!("skills-ref validate passed on promoted-from-reflection skill");
                }
                Ok(v) => {
                    let stderr = String::from_utf8_lossy(&v.stderr);
                    panic!("skills-ref validate failed: {stderr}");
                }
                Err(e) => {
                    eprintln!("skills-ref invocation error: {e}; treating as skip");
                }
            }
        }
        _ => {
            eprintln!(
                "skills-ref not installed; external validation of promoted-from-reflection skill skipped (same pattern as tests/skill_test.rs L1-5)."
            );
        }
    }
}
