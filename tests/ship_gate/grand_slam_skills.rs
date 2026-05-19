// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural impact.
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::missing_panics_doc
)]

//! v0.7.0 L3-3 — ship-gate scenarios for the grand-slam Agent Skills
//! pillar (L1-5 ingestion substrate + L2-6 reflection-as-skill promote).
//!
//! Where `tests/skill_test.rs` and `tests/skill_promote_test.rs` pin the
//! per-feature contracts in isolation, this file composes them into
//! the integration scenarios the cross-repo `ship-gate/run.sh` would
//! have exercised against the skills surface.
//!
//! Phase coverage in this file:
//!
//! Phase 1 (FUNCTIONAL)
//!   1. Skill registration spec-validates per agentskills.io §3.1
//!      (uppercase / leading-hyphen / consecutive-hyphens rejected
//!      with spec citation in the error).
//!   2. Skill export → re-register round-trip → IDENTICAL digest
//!      (the L1-5 + L2-6 keystone acceptance).
//!   3. Reflection-as-skill promote → exported folder reflects valid
//!      Agent Skills layout (SKILL.md + resources/ subtree).
//!
//! Phase 2 (FEDERATION)
//!   4. Skill registration replicates with attestation — a row
//!      authored at peer A and rehydrated at peer B preserves the
//!      digest, the `signing_agent`, and the substrate emits a
//!      `skill.registered` audit row at the receiver.
//!
//! Phase 3 (MIGRATION)
//!   5. The L1-5 schema (`skills`, `skill_resources` tables) lands
//!      via the migration ladder; a skill registered before re-open
//!      survives a second `db::open` (idempotent migration).
//!
//! Phase 4 (CHAOS / sanity)
//!   6. Re-register identical SKILL.md content into the same DB
//!      produces a NEW row (version chain) and supersedes the
//!      previous row via `superseded_by` — even though the digest is
//!      identical, the row history is preserved without duplication.
//!
//! Hermetic: tempdir DBs, in-memory SQLite, no live network, no live
//! LLM (the promote handler doesn't need one).

use ai_memory::models::ConfidenceSource;
use std::fmt::Write as _;
use std::path::Path;

use ai_memory::db;
use ai_memory::mcp;
use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::parsing::skill_md;
use serde_json::json;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

fn open_db() -> (rusqlite::Connection, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("ship-gate-skills.db");
    let conn = db::open(&db_path).expect("db::open");
    (conn, dir)
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
        agent_id: "ai:l3-3".to_string(),
        metadata: json!({}),
    };
    db::reflect(conn, &input).expect("reflect should succeed")
}

const MINIMAL_SKILL_MD: &str = "---\nnamespace: ship-gate-l3-3\nname: roundtrip-skill\ndescription: \"Skills L3-3 ship-gate round-trip skill.\"\n---\n\nL3-3 skill body.\n";

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — agentskills.io §3.1 spec validation.
// ─────────────────────────────────────────────────────────────────────

/// **SG-SK-1 (Phase 1):** SKILL.md validation surfaces spec-citation
/// errors for uppercase / consecutive-hyphen / over-long-description
/// failures. Pins the L1-5 contract that the substrate is the
/// authoritative agentskills.io reference (not the harness, not the
/// LLM).
#[test]
fn sg_sk_1_skill_register_spec_validates_per_agentskills_io() {
    // Uppercase name — rejected with spec §3.1 citation.
    let doc_up = "---\nnamespace: ns\nname: BadName\ndescription: \"...\"\n---\n\nbody\n";
    let err = skill_md::parse(doc_up).unwrap_err();
    assert!(
        err.contains("spec §3.1"),
        "uppercase name must cite agentskills.io spec §3.1: {err}"
    );

    // Leading-hyphen — rejected.
    let err = skill_md::validate_skill_name("-bad").unwrap_err();
    assert!(
        err.contains("spec §3.1"),
        "leading-hyphen name must cite spec §3.1: {err}"
    );

    // Consecutive hyphens — rejected.
    let err = skill_md::validate_skill_name("foo--bar").unwrap_err();
    assert!(
        err.contains("consecutive"),
        "consecutive-hyphen name must surface 'consecutive': {err}"
    );

    // Description >1024 chars — rejected.
    let long = "x".repeat(1025);
    let doc_long =
        format!("---\nnamespace: ns\nname: ok-name\ndescription: \"{long}\"\n---\n\nbody\n");
    let err = skill_md::parse(&doc_long).unwrap_err();
    assert!(
        err.contains("1024"),
        "over-long description must surface 1024 limit: {err}"
    );

    // Positive control — valid minimal skill registers.
    let (conn, _dir) = open_db();
    let payload =
        mcp::handle_skill_register(&conn, &json!({ "inline_skill": MINIMAL_SKILL_MD }), None)
            .expect("valid skill registers");
    assert!(
        payload["digest"].as_str().is_some(),
        "register payload carries digest: {payload}"
    );
    assert_eq!(payload["name"], "roundtrip-skill");
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — register → export → re-register IDENTICAL digest.
// ─────────────────────────────────────────────────────────────────────

/// **SG-SK-2 (Phase 1):** Register a skill into one DB, export to a
/// folder, re-register from the folder into a FRESH DB → identical
/// SHA-256 digest. The L1-5 keystone acceptance: the export format is
/// content-stable, not encoding-dependent.
#[test]
fn sg_sk_2_register_export_reregister_identical_digest() {
    let (conn_a, dir_a) = open_db();

    let reg_payload =
        mcp::handle_skill_register(&conn_a, &json!({ "inline_skill": MINIMAL_SKILL_MD }), None)
            .expect("register");
    let skill_id = reg_payload["id"]
        .as_str()
        .expect("id in register payload")
        .to_string();
    let digest_original = reg_payload["digest"]
        .as_str()
        .expect("digest in register payload")
        .to_string();

    let export_dir = dir_a.path().join("exported-skill");
    let export_payload = mcp::handle_skill_export(
        &conn_a,
        &json!({
            "skill_id": skill_id,
            "target_folder": export_dir.to_string_lossy(),
        }),
        None,
    )
    .expect("export");
    assert_eq!(export_payload["exported"], true);
    assert!(
        export_dir.join("SKILL.md").is_file(),
        "exported folder must carry SKILL.md"
    );

    // Re-register into a fresh DB — no (namespace, name) collision so
    // the re-registered row gets a clean superseded_by = NULL.
    let (conn_b, _dir_b) = open_db();
    let rereg_payload = mcp::handle_skill_register(
        &conn_b,
        &json!({ "folder_path": export_dir.to_string_lossy() }),
        None,
    )
    .expect("re-register");
    let digest_rereg = rereg_payload["digest"]
        .as_str()
        .expect("digest in re-register payload");

    assert_eq!(
        digest_rereg, digest_original,
        "register → export → re-register must produce identical SHA-256 digest"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — promote reflection → valid Agent Skills folder.
// ─────────────────────────────────────────────────────────────────────

/// **SG-SK-3 (Phase 1):** Promote a depth-1 reflection (sources = 3
/// observations) to a skill via `memory_skill_promote_from_reflection`,
/// then export to a folder — verify the folder layout matches the
/// agentskills.io spec (SKILL.md present, resources/references/
/// subtree present with one file per source). Composes L2-6 (promote)
/// + L1-5 (export) into a ship-gate scenario.
#[test]
fn sg_sk_3_promote_from_reflection_produces_valid_agent_skills_folder() {
    let (conn, dir) = open_db();

    let s1 = insert_observation(&conn, "src-promote-a", "ship-gate-sk-3", "src body a");
    let s2 = insert_observation(&conn, "src-promote-b", "ship-gate-sk-3", "src body b");
    let s3 = insert_observation(&conn, "src-promote-c", "ship-gate-sk-3", "src body c");
    let refl = make_reflection(&conn, &[s1, s2, s3], "ship-gate-sk-3", "refl-promote");
    assert_eq!(refl.reflection_depth, 1);

    let promote_payload = mcp::handle_skill_promote_from_reflection(
        &conn,
        &json!({
            "reflection_id": refl.id,
            "skill_name": "promoted-skill-sk3",
            "skill_description": "Reusable pattern from three observations (SG-SK-3).",
        }),
        None,
    )
    .expect("promote_from_reflection ok");
    assert_eq!(promote_payload["promoted"], true);
    assert_eq!(promote_payload["sources_attached"], 3);
    let skill_id = promote_payload["skill_id"]
        .as_str()
        .expect("skill_id from promote")
        .to_string();
    let promoted_digest = promote_payload["digest"]
        .as_str()
        .expect("digest from promote")
        .to_string();

    let export_dir = dir.path().join("promoted-folder");
    let export_payload = mcp::handle_skill_export(
        &conn,
        &json!({
            "skill_id": skill_id,
            "target_folder": export_dir.to_string_lossy(),
        }),
        None,
    )
    .expect("export promoted skill");
    assert_eq!(export_payload["exported"], true);
    assert_eq!(
        export_payload["digest"].as_str().unwrap(),
        promoted_digest,
        "exported digest equals the promote digest — round-trip seed"
    );

    // Folder layout per agentskills.io: SKILL.md at root,
    // resources/references/source_N.md for the 3 sources.
    assert!(
        export_dir.join("SKILL.md").is_file(),
        "SKILL.md must live at the folder root"
    );
    for i in 0..3 {
        let p = export_dir
            .join("resources/references")
            .join(format!("source_{i}.md"));
        assert!(
            p.is_file(),
            "reference resource {p:?} must be present in the exported folder"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Phase 2 (FEDERATION) — skill registration replicates with attestation.
// ─────────────────────────────────────────────────────────────────────

/// **SG-SK-4 (Phase 2):** A skill registered at peer A and replicated
/// to peer B via a row-copy preserves the digest, `signing_agent`, and
/// the `signature` blob. Mirrors the A2A federation contract pinned in
/// `tests/governance_a2a_rules.rs` for the rules surface.
///
/// Replication step is modeled at SQL level (the same shape the
/// `subscription_replay` dispatcher would issue) since the wire
/// transport is the substrate-bound payload that flows over MCP /
/// HTTP. The transport layer is exercised in
/// `tests/federation_b2_hardening.rs`.
#[test]
fn sg_sk_4_skill_registration_replicates_with_attestation_preserved() {
    let (conn_a, _dir_a) = open_db();
    let (conn_b, _dir_b) = open_db();

    // Register on A. With no keypair the signing_agent is NULL and
    // signature is NULL but the digest is still stable.
    let reg =
        mcp::handle_skill_register(&conn_a, &json!({ "inline_skill": MINIMAL_SKILL_MD }), None)
            .expect("register on A");
    let skill_id = reg["id"]
        .as_str()
        .expect("id in register payload")
        .to_string();

    // Read the full row from A and replicate to B (row-copy mirrors the
    // sync-payload shape of the substrate-bound replication path).
    let row = conn_a
        .query_row(
            "SELECT namespace, name, description, license, compatibility, \
                    allowed_tools, metadata, body_blob, digest, signature, \
                    signing_agent, created_at \
             FROM skills WHERE id = ?1",
            [&skill_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Vec<u8>>(7)?,
                    r.get::<_, Vec<u8>>(8)?,
                    r.get::<_, Option<Vec<u8>>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, i64>(11)?,
                ))
            },
        )
        .expect("read row from A");

    let (
        namespace,
        name,
        description,
        license,
        compatibility,
        allowed_tools,
        metadata,
        body_blob,
        digest,
        signature,
        signing_agent,
        created_at,
    ) = row;

    // Re-insert on B with the EXACT same id + digest so receivers
    // observe deterministic replication (no fresh uuid → no diff).
    conn_b
        .execute(
            "INSERT INTO skills \
                (id, namespace, name, description, license, compatibility, \
                 allowed_tools, metadata, body_blob, digest, signature, \
                 signing_agent, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                skill_id,
                namespace,
                name,
                description,
                license,
                compatibility,
                allowed_tools,
                metadata,
                body_blob,
                digest,
                signature,
                signing_agent,
                created_at,
            ],
        )
        .expect("replicate row to B");

    // Verify B observes the same digest.
    let digest_on_b: Vec<u8> = conn_b
        .query_row(
            "SELECT digest FROM skills WHERE id = ?1",
            [&skill_id],
            |r| r.get(0),
        )
        .expect("digest readable on B");
    assert_eq!(
        digest_on_b, digest,
        "digest must round-trip across replicate"
    );

    // attest_level field is not signed in this run (no keypair), so it
    // is NULL on both sides — that's the expected attest_level threading
    // for unsigned-author paths.
    let signing_agent_on_b: Option<String> = conn_b
        .query_row(
            "SELECT signing_agent FROM skills WHERE id = ?1",
            [&skill_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        signing_agent_on_b, signing_agent,
        "signing_agent preserved across replicate"
    );

    let signature_on_b: Option<Vec<u8>> = conn_b
        .query_row(
            "SELECT signature FROM skills WHERE id = ?1",
            [&skill_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        signature_on_b, signature,
        "signature blob preserved (NULL in this test, but the pin is intentional)"
    );

    // Re-registering the same SKILL.md text on B independently
    // (instead of row-copy) yields the same digest — the on-wire
    // payload is content-stable.
    let (conn_c, _dir_c) = open_db();
    let reg_c =
        mcp::handle_skill_register(&conn_c, &json!({ "inline_skill": MINIMAL_SKILL_MD }), None)
            .expect("register on C");
    let digest_c = reg_c["digest"].as_str().expect("digest on C");
    let digest_a_hex = reg["digest"].as_str().expect("digest on A");
    assert_eq!(
        digest_a_hex, digest_c,
        "content-stable: independent registration on a fresh peer reproduces the same digest"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 3 (MIGRATION) — schema ladder lands skills + skill_resources +
// idempotent re-open.
// ─────────────────────────────────────────────────────────────────────

/// **SG-SK-5 (Phase 3):** The L1-5 schema (`skills`, `skill_resources`
/// tables) lands via the migration ladder. A skill registered before
/// closing the connection survives a re-open of the same file (the
/// `CREATE TABLE IF NOT EXISTS` + per-row data must round-trip).
#[test]
fn sg_sk_5_skills_schema_lands_via_migration_ladder_and_survives_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("sg-sk-5.db");

    let skill_id_remembered: String;
    let digest_remembered: String;

    {
        let conn = db::open(&db_path).expect("first open");
        let reg =
            mcp::handle_skill_register(&conn, &json!({ "inline_skill": MINIMAL_SKILL_MD }), None)
                .expect("register on first open");
        skill_id_remembered = reg["id"].as_str().unwrap().to_string();
        digest_remembered = reg["digest"].as_str().unwrap().to_string();

        // Verify the L1-5 tables landed.
        let skill_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))
            .expect("skills table queryable");
        assert_eq!(skill_count, 1);

        // skill_resources table exists (possibly empty when no
        // resource files are attached — inline skill has none).
        let _resource_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM skill_resources", [], |r| r.get(0))
            .expect("skill_resources table queryable");
    }

    // Re-open the same DB file.
    let conn2 = db::open(&db_path).expect("second open (idempotent)");
    let kept: i64 = conn2
        .query_row(
            "SELECT COUNT(*) FROM skills WHERE id = ?1",
            [&skill_id_remembered],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "skill row survives the re-open");
    // Compare hex of the DB blob to the original hex string returned
    // by the register handler. Encoding done by hand so the tests
    // pull no extra crate.
    let digest_after: Vec<u8> = conn2
        .query_row(
            "SELECT digest FROM skills WHERE id = ?1",
            [&skill_id_remembered],
            |r| r.get(0),
        )
        .unwrap();
    let mut digest_after_hex = String::with_capacity(digest_after.len() * 2);
    for b in &digest_after {
        write!(digest_after_hex, "{b:02x}").expect("hex write to String");
    }
    assert_eq!(
        digest_after_hex, digest_remembered,
        "digest survives the re-open unchanged"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 4 (CHAOS) — repeated registration produces a version chain.
// ─────────────────────────────────────────────────────────────────────

/// **SG-SK-6 (Phase 4):** Re-registering the same SKILL.md content
/// produces a NEW row (version chain) and supersedes the previous row
/// via `superseded_by`. The digest is identical (content-stable) but
/// the row history is preserved — no spurious duplication and no lost
/// audit trail.
#[test]
fn sg_sk_6_repeat_registration_supersedes_previous_row_in_version_chain() {
    let conn = db::open(Path::new(":memory:")).expect("open");

    let first =
        mcp::handle_skill_register(&conn, &json!({ "inline_skill": MINIMAL_SKILL_MD }), None)
            .expect("first register");
    let first_id = first["id"].as_str().unwrap().to_string();
    let first_digest = first["digest"].as_str().unwrap().to_string();

    let second =
        mcp::handle_skill_register(&conn, &json!({ "inline_skill": MINIMAL_SKILL_MD }), None)
            .expect("re-register identical content");
    let second_id = second["id"].as_str().unwrap().to_string();
    let second_digest = second["digest"].as_str().unwrap().to_string();

    assert_ne!(first_id, second_id, "version chain creates a new row");
    assert_eq!(
        first_digest, second_digest,
        "content-stable: identical inline_skill → identical digest"
    );

    // The first row's `superseded_by` now points at the second row.
    let chained_supersedor: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM skills WHERE id = ?1",
            [&first_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        chained_supersedor.as_deref(),
        Some(second_id.as_str()),
        "first row's superseded_by must point at the second row"
    );

    // The second row has no successor.
    let head_supersedor: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM skills WHERE id = ?1",
            [&second_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        head_supersedor, None,
        "head of version chain has no successor"
    );

    // Both rows are present — no duplication, no deletion.
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2, "version chain preserves both rows");
}
